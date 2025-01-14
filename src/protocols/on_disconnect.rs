use std::net::SocketAddr;

use tokio::sync::{mpsc, oneshot};
use tracing::*;

use crate::{node::NodeTask, protocols::ProtocolHandler, Pea2Pea};
#[cfg(doc)]
use crate::{
    protocols::{Reading, Writing},
    Connection,
};

/// Can be used to automatically perform some extra actions when the connection with a peer is
/// severed, which is especially practical if the disconnect is triggered automatically, e.g. due
/// to the peer sending a noncompliant message or when the peer is the one to shut down the
/// connection with the node.
///
/// note: the node can only tell that a peer disconnected from it if it is actively trying to read
/// from the associated connection (i.e. [`Reading`] is enabled) or if it attempts to send a message
/// to it (i.e. one of the [`Writing`] methods is called).
#[async_trait::async_trait]
pub trait OnDisconnect: Pea2Pea
where
    Self: Clone + Send + Sync + 'static,
{
    /// Attaches the behavior specified in [`OnDisconnect::on_disconnect`] to every occurrence of the
    /// node disconnecting from a peer.
    async fn enable_disconnect(&self) {
        let (from_node_sender, mut from_node_receiver) =
            mpsc::unbounded_channel::<(SocketAddr, oneshot::Sender<()>)>();

        // use a channel to know when the disconnect task is ready
        let (tx, rx) = oneshot::channel::<()>();

        // spawn a background task dedicated to handling disconnect events
        let self_clone = self.clone();
        let disconnect_task = tokio::spawn(async move {
            trace!(parent: self_clone.node().span(), "spawned the OnDisconnect handler task");
            if tx.send(()).is_err() {
                error!(parent: self_clone.node().span(), "OnDisconnect handler creation interrupted! shutting down the node");
                self_clone.node().shut_down().await;
                return;
            }

            while let Some((addr, notifier)) = from_node_receiver.recv().await {
                let self_clone2 = self_clone.clone();
                tokio::spawn(async move {
                    // perform the specified extra actions
                    self_clone2.on_disconnect(addr).await;
                    // notify the node that the extra actions have concluded
                    // and that the related connection can be dropped
                    let _ = notifier.send(()); // can't really fail
                });
            }
        });
        let _ = rx.await;
        self.node()
            .tasks
            .lock()
            .insert(NodeTask::OnDisconnect, disconnect_task);

        // register the OnDisconnect handler with the Node
        let hdl = ProtocolHandler(from_node_sender);
        assert!(
            self.node().protocols.on_disconnect.set(hdl).is_ok(),
            "the OnDisconnect protocol was enabled more than once!"
        );
    }

    /// Any extra actions to be executed during a disconnect; in order to still be able to
    /// communicate with the peer in the usual manner (i.e. via [`Writing`]), only its [`SocketAddr`]
    /// (as opposed to the related [`Connection`] object) is provided as an argument.
    async fn on_disconnect(&self, addr: SocketAddr);
}
