use std::{
    collections::HashSet,
    io,
    net::{IpAddr, SocketAddr},
    ops::Deref,
    sync::{
        atomic::{AtomicUsize, Ordering::*},
        Arc,
    },
};

use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use tokio::{
    io::split,
    net::{TcpListener, TcpSocket, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};
use tracing::*;

use crate::{
    connections::{Connection, ConnectionSide, Connections},
    protocols::{Protocol, Protocols},
    Config, KnownPeers, Stats,
};

macro_rules! enable_protocol {
    ($handler_type: ident, $node:expr, $conn: expr) => {
        if let Some(handler) = $node.protocols.$handler_type.get() {
            let (conn_returner, conn_retriever) = oneshot::channel();

            handler.trigger(($conn, conn_returner));

            match conn_retriever.await {
                Ok(Ok(conn)) => conn,
                Err(_) => return Err(io::ErrorKind::BrokenPipe.into()),
                Ok(e) => return e,
            }
        } else {
            $conn
        }
    };
}

// A seuential numeric identifier assigned to `Node`s that were not provided with a name.
static SEQUENTIAL_NODE_ID: AtomicUsize = AtomicUsize::new(0);

/// The central object responsible for handling connections.
#[derive(Clone)]
pub struct Node(Arc<InnerNode>);

impl Deref for Node {
    type Target = Arc<InnerNode>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[doc(hidden)]
pub struct InnerNode {
    /// The tracing span.
    span: Span,
    /// The node's configuration.
    config: Config,
    /// The node's listening address.
    listening_addr: OnceCell<SocketAddr>,
    /// Contains objects used by the protocols implemented by the node.
    pub(crate) protocols: Protocols,
    /// A list of connections that have not been finalized yet.
    connecting: Mutex<HashSet<SocketAddr>>,
    /// Contains objects related to the node's active connections.
    connections: Connections,
    /// Collects statistics related to the node's peers.
    known_peers: KnownPeers,
    /// Collects statistics related to the node itself.
    stats: Stats,
    /// The node's tasks.
    pub(crate) tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Node {
    /// Creates a new [`Node`] using the given [`Config`].
    pub fn new(mut config: Config) -> Self {
        // if there is no pre-configured name, assign a sequential numeric identifier
        if config.name.is_none() {
            config.name = Some(SEQUENTIAL_NODE_ID.fetch_add(1, SeqCst).to_string());
        }

        // create a tracing span containing the node's name
        let span = create_span(config.name.as_deref().unwrap());

        let node = Node(Arc::new(InnerNode {
            span,
            config,
            listening_addr: Default::default(),
            protocols: Default::default(),
            connecting: Default::default(),
            connections: Default::default(),
            known_peers: Default::default(),
            stats: Default::default(),
            tasks: Default::default(),
        }));

        debug!(parent: node.span(), "the node is ready");

        node
    }

    async fn create_listener(&self, listener_ip: IpAddr) -> io::Result<TcpListener> {
        if let Some(port) = self.config().desired_listening_port {
            let desired_listening_addr = SocketAddr::new(listener_ip, port);
            match TcpListener::bind(desired_listening_addr).await {
                Err(e) => {
                    if self.config().allow_random_port {
                        warn!(
                            parent: self.span(),
                            "trying any port, the desired one is unavailable: {}", e
                        );
                        let random_available_addr = SocketAddr::new(listener_ip, 0);
                        TcpListener::bind(random_available_addr).await
                    } else {
                        error!(parent: self.span(), "the desired port is unavailable: {}", e);
                        Err(e)
                    }
                }
                listener => listener,
            }
        } else if self.config().allow_random_port {
            let random_available_addr = SocketAddr::new(listener_ip, 0);
            TcpListener::bind(random_available_addr).await
        } else {
            panic!("you must either provide a desired listening port or allow a random one");
        }
    }

    /// Makes the node listen for inbound connections; returns the associated socket address, which will
    /// correspond to [`Config::listener_ip`] and either [`Config::desired_listening_port`] or the port
    /// provided by the OS.
    pub async fn start_listening(&self) -> io::Result<SocketAddr> {
        if let Some(listening_addr) = self.listening_addr.get() {
            panic!(
                "the node already has a listening address associated with it: {}",
                listening_addr
            );
        } else {
            let listener_ip = self
                .config()
                .listener_ip
                .expect("Node::start_listening was called, but Config::listener_ip is not set");
            let listener = self.create_listener(listener_ip).await?;
            let port = listener.local_addr()?.port(); // discover the port if it was unspecified
            let listening_addr = (listener_ip, port).into();

            self.listening_addr
                .set(listening_addr)
                .expect("the node's listener was started more than once");

            // use a channel to know when the listening task is ready
            let (tx, rx) = oneshot::channel();

            let node = self.clone();
            let listening_task = tokio::spawn(async move {
                trace!(parent: node.span(), "spawned the listening task");
                if tx.send(()).is_err() {
                    error!(parent: node.span(), "node creation interrupted; shutting down the listening task");
                    return;
                }

                loop {
                    match listener.accept().await {
                        Ok((stream, addr)) => {
                            debug!(parent: node.span(), "tentatively accepted a connection from {}", addr);

                            if !node.can_add_connection() {
                                debug!(parent: node.span(), "rejecting the connection from {}", addr);
                                continue;
                            }

                            node.connecting.lock().insert(addr);

                            let node = node.clone();
                            tokio::spawn(async move {
                                if let Err(e) = node
                                    .adapt_stream(stream, addr, ConnectionSide::Responder)
                                    .await
                                {
                                    node.connecting.lock().remove(&addr);
                                    node.known_peers().register_failure(addr);
                                    error!(parent: node.span(), "couldn't accept a connection: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            error!(parent: node.span(), "couldn't accept a connection: {}", e);
                        }
                    }
                }
            });
            self.tasks.lock().push(listening_task);
            let _ = rx.await;
            debug!(parent: self.span(), "listening on {}", listening_addr);

            Ok(listening_addr)
        }
    }

    /// Returns the name assigned to the node.
    #[inline]
    pub fn name(&self) -> &str {
        // safe; can be set as None in Config, but receives a default value on Node creation
        self.config.name.as_deref().unwrap()
    }

    /// Returns a reference to the node's config.
    #[inline]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Returns a reference to the node's stats.
    #[inline]
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Returns the tracing [`Span`] associated with the node.
    #[inline]
    pub fn span(&self) -> &Span {
        &self.span
    }

    /// Returns the node's listening address; returns an error if the node was configured
    /// to not listen for inbound connections or if the listener hasn't been started yet.
    pub fn listening_addr(&self) -> io::Result<SocketAddr> {
        self.listening_addr
            .get()
            .copied()
            .ok_or_else(|| io::ErrorKind::AddrNotAvailable.into())
    }

    async fn enable_protocols(&self, conn: Connection) -> io::Result<Connection> {
        let mut conn = enable_protocol!(handshake, self, conn);

        // split the stream after the handshake (if not done before)
        if let Some(stream) = conn.stream.take() {
            let (reader, writer) = split(stream);
            conn.reader = Some(Box::new(reader));
            conn.writer = Some(Box::new(writer));
        }

        let conn = enable_protocol!(reading, self, conn);
        let conn = enable_protocol!(writing, self, conn);

        Ok(conn)
    }

    /// Prepares the freshly acquired connection to handle the protocols the Node implements.
    async fn adapt_stream(
        &self,
        stream: TcpStream,
        peer_addr: SocketAddr,
        own_side: ConnectionSide,
    ) -> io::Result<()> {
        self.known_peers.add(peer_addr);

        // register the port seen by the peer
        if own_side == ConnectionSide::Initiator {
            if let Ok(addr) = stream.local_addr() {
                debug!(
                    parent: self.span(), "establishing connection with {}; the peer is connected on port {}",
                    peer_addr, addr.port()
                );
            } else {
                warn!(parent: self.span(), "couldn't determine the peer's port");
            }
        }

        let connection = Connection::new(peer_addr, stream, !own_side);

        // enact the enabled protocols
        let mut connection = self.enable_protocols(connection).await?;

        // if Reading is enabled, we'll notify the related task when the connection is fully ready
        let conn_ready_tx = connection.readiness_notifier.take();

        self.connections.add(connection);
        self.connecting.lock().remove(&peer_addr);

        // send the aforementioned notification so that reading from the socket can commence
        if let Some(tx) = conn_ready_tx {
            let _ = tx.send(());
        }

        Ok(())
    }

    // A helper method to facilitate a common potential disconnect at the callsite.
    async fn create_stream(
        &self,
        addr: SocketAddr,
        socket: Option<TcpSocket>,
    ) -> io::Result<TcpStream> {
        if let Some(socket) = socket {
            socket.connect(addr).await
        } else {
            TcpStream::connect(addr).await
        }
    }

    /// Connects to the provided `SocketAddr`.
    pub async fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        self.connect_inner(addr, None).await
    }

    /// Connects to a `SocketAddr` using the provided `TcpSocket`.
    pub async fn connect_using_socket(
        &self,
        addr: SocketAddr,
        socket: TcpSocket,
    ) -> io::Result<()> {
        self.connect_inner(addr, Some(socket)).await
    }

    /// Connects to the provided `SocketAddr` using an optional `TcpSocket`.
    async fn connect_inner(&self, addr: SocketAddr, socket: Option<TcpSocket>) -> io::Result<()> {
        if let Ok(listening_addr) = self.listening_addr() {
            if addr == listening_addr
                || addr.ip().is_loopback() && addr.port() == listening_addr.port()
            {
                error!(parent: self.span(), "can't connect to node's own listening address ({})", addr);
                return Err(io::ErrorKind::AddrInUse.into());
            }
        }

        if !self.can_add_connection() {
            error!(parent: self.span(), "too many connections; refusing to connect to {}", addr);
            return Err(io::ErrorKind::PermissionDenied.into());
        }

        if self.connections.is_connected(addr) {
            warn!(parent: self.span(), "already connected to {}", addr);
            return Err(io::ErrorKind::AlreadyExists.into());
        }

        if !self.connecting.lock().insert(addr) {
            warn!(parent: self.span(), "already connecting to {}", addr);
            return Err(io::ErrorKind::AlreadyExists.into());
        }

        let stream = self.create_stream(addr, socket).await.map_err(|e| {
            self.connecting.lock().remove(&addr);
            e
        })?;

        let ret = self
            .adapt_stream(stream, addr, ConnectionSide::Initiator)
            .await;

        if let Err(ref e) = ret {
            self.connecting.lock().remove(&addr);
            self.known_peers().register_failure(addr);
            error!(parent: self.span(), "couldn't initiate a connection with {}: {}", addr, e);
        }

        ret
    }

    /// Disconnects from the provided `SocketAddr`.
    pub async fn disconnect(&self, addr: SocketAddr) -> bool {
        if let Some(handler) = self.protocols.disconnect.get() {
            if self.is_connected(addr) {
                let (sender, receiver) = oneshot::channel();

                handler.trigger((addr, sender));
                let _ = receiver.await; // can't really fail
            }
        }

        let conn = self.connections.remove(addr);

        if let Some(ref conn) = conn {
            debug!(parent: self.span(), "disconnecting from {}", conn.addr());

            // shut the associated tasks down
            for task in conn.tasks.iter().rev() {
                task.abort();
            }

            // if the (owning) node was not the initiator of the connection, it doesn't know the listening address
            // of the associated peer, so the related stats are unreliable; the next connection initiated by the
            // peer could be bound to an entirely different port number
            if conn.side() == ConnectionSide::Initiator {
                self.known_peers().remove(conn.addr());
            }

            debug!(parent: self.span(), "disconnected from {}", addr);
        } else {
            debug!(parent: self.span(), "couldn't disconnect from {}, as it wasn't connected", addr);
        }

        conn.is_some()
    }

    /// Returns a list containing addresses of active connections.
    pub fn connected_addrs(&self) -> Vec<SocketAddr> {
        self.connections.addrs()
    }

    /// Returns a reference to the collection of statistics of node's known peers.
    #[inline]
    pub fn known_peers(&self) -> &KnownPeers {
        &self.known_peers
    }

    /// Checks whether the provided address is connected.
    pub fn is_connected(&self, addr: SocketAddr) -> bool {
        self.connections.is_connected(addr)
    }

    /// Checks if the node is currently setting up a connection with the provided address.
    pub fn is_connecting(&self, addr: SocketAddr) -> bool {
        self.connecting.lock().contains(&addr)
    }

    /// Returns the number of active connections.
    pub fn num_connected(&self) -> usize {
        self.connections.num_connected()
    }

    /// Returns the number of connections that are currently being set up.
    pub fn num_connecting(&self) -> usize {
        self.connecting.lock().len()
    }

    /// Checks whether the `Node` can handle an additional connection.
    fn can_add_connection(&self) -> bool {
        let num_connected = self.num_connected();
        let limit = self.config.max_connections as usize;
        if num_connected >= limit || num_connected + self.num_connecting() >= limit {
            warn!(parent: self.span(), "maximum number of connections ({}) reached", limit);
            false
        } else {
            true
        }
    }

    /// Gracefully shuts the node down.
    pub async fn shut_down(&self) {
        debug!(parent: self.span(), "shutting down");

        let mut tasks = std::mem::take(&mut *self.tasks.lock()).into_iter();
        if let Some(listening_task) = tasks.next() {
            listening_task.abort(); // abort the listening task first
        }

        for addr in self.connected_addrs() {
            self.disconnect(addr).await;
        }

        for handle in tasks {
            handle.abort();
        }
    }
}

// FIXME: this can probably be done more elegantly
/// Creates the node's tracing span based on its name.
fn create_span(node_name: &str) -> Span {
    let mut span = trace_span!("node", name = node_name);
    if !span.is_disabled() {
        return span;
    } else {
        span = debug_span!("node", name = node_name);
    }
    if !span.is_disabled() {
        return span;
    } else {
        span = info_span!("node", name = node_name);
    }
    if !span.is_disabled() {
        return span;
    } else {
        span = warn_span!("node", name = node_name);
    }
    if !span.is_disabled() {
        span
    } else {
        error_span!("node", name = node_name)
    }
}
