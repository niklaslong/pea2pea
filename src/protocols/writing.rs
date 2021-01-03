use crate::{connections::ConnectionWriter, protocols::ReturnableConnection, Pea2Pea};

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::FutureExt;
use tokio::{io::AsyncWriteExt, sync::mpsc, task::JoinHandle, time::sleep};
use tracing::*;

use std::{io, net::SocketAddr, time::Duration};

/// Can be used to specify and enable writing, i.e. sending outbound messages.
/// If handshaking is enabled too, it goes into force only after the handshake has been concluded.
#[async_trait]
pub trait Writing: Pea2Pea
where
    Self: Clone + Send + Sync + 'static,
{
    // TODO: add an associated type defaulting to ConnectionWriter once
    // https://github.com/rust-lang/rust/issues/29661 is resolved.

    /// Prepares the node to send messages.
    fn enable_writing(&self) {
        let (conn_sender, mut conn_receiver) =
            mpsc::channel::<ReturnableConnection>(self.node().config.writing_handler_queue_depth);

        // the task spawning tasks reading messages from the given stream
        let self_clone = self.clone();
        let writing_task = tokio::spawn(async move {
            trace!(parent: self_clone.node().span(), "spawned the `Writing` handler task");

            loop {
                // these objects are sent from `Node::adapt_stream`
                if let Some((mut conn, conn_returner)) = conn_receiver.recv().await {
                    let addr = conn.addr;
                    let mut conn_writer = conn.writer.take().unwrap(); // safe; it is available at this point

                    let (outbound_message_sender, mut outbound_message_receiver) =
                        mpsc::channel::<Bytes>(self_clone.node().config.conn_outbound_queue_depth);

                    // the task for writing outbound messages
                    let writer_clone = self_clone.clone();
                    let writer_task = tokio::spawn(async move {
                        let node = writer_clone.node();
                        trace!(parent: node.span(), "spawned a task for writing messages to {}", addr);

                        loop {
                            // TODO: when try_recv is available in tokio again (https://github.com/tokio-rs/tokio/issues/3350),
                            // use try_recv() instead
                            match outbound_message_receiver.recv().now_or_never() {
                                Some(Some(msg)) => {
                                    match writer_clone.write_to_stream(&msg, &mut conn_writer).await
                                    {
                                        Ok((written, msg_len)) => {
                                            if written != 0 {
                                                trace!(parent: node.span(), "sent {}B to {}", written, addr);
                                            }
                                            node.known_peers().register_sent_message(addr, msg_len);
                                            node.stats.register_sent_message(msg_len);
                                        }
                                        Err(e) => {
                                            node.known_peers().register_failure(addr);
                                            error!(parent: node.span(), "couldn't send a message to {}: {}", addr, e);
                                        }
                                    }
                                }
                                None => {
                                    if conn_writer.carry != 0 {
                                        if let Err(e) = conn_writer
                                            .writer
                                            .write_all(&conn_writer.buffer[..conn_writer.carry])
                                            .await
                                        {
                                            node.known_peers().register_failure(addr);
                                            error!(parent: node.span(), "couldn't send a message to {}: {}", addr, e);
                                        }
                                        trace!(parent: node.span(), "sent {}B to {}", conn_writer.carry, addr);
                                        conn_writer.carry = 0;
                                    }
                                    // introduce a delay in order not to block
                                    sleep(Duration::from_millis(1)).await;
                                }
                                Some(None) => panic!("can't receive messages sent to {}", addr), // can't recover
                            }
                        }
                    });

                    // the Connection object registers the handle of the newly created task and
                    // the Sender that will allow the Node to transmit messages to it
                    conn.tasks.push(writer_task);
                    conn.outbound_message_sender = Some(outbound_message_sender);

                    // return the Connection to the Node, resuming Node::adapt_stream
                    if conn_returner.send(Ok(conn)).is_err() {
                        // can't recover if this happens
                        panic!("can't return a Connection to the Node");
                    }
                }
            }
        });

        // register the WritingHandler with the Node
        self.node()
            .set_writing_handler((conn_sender, writing_task).into());
    }

    /// Writes the given message to `ConnectionWriter`'s stream; returns the number of bytes written paired
    /// with the number of bytes of the whole message.
    async fn write_to_stream(
        &self,
        message: &[u8],
        conn_writer: &mut ConnectionWriter,
    ) -> io::Result<(usize, usize)> {
        let ConnectionWriter {
            node: _,
            addr,
            writer,
            buffer,
            carry,
        } = conn_writer;

        let len = self.write_message(*addr, message, &mut buffer[*carry..])?;

        let (written, len) = if len != 0 {
            let written = writer.write(&buffer[..*carry + len]).await?;
            *carry = *carry + len - written;

            // move the leftover bytes to the beginning of the buffer; the next read will append bytes
            // starting from where the leftover ones end, allowing the message to be completed
            buffer.copy_within(written..written + *carry, 0);

            (written, len)
        } else {
            writer.write_all(&buffer[..*carry]).await?;
            let written = *carry;
            let len = self.write_message(*addr, message, buffer)?;
            *carry = len;

            (written, len)
        };

        Ok((written, len))
    }

    /// Writes the provided payload to `ConnectionWriter`'s buffer; the payload can get prepended with a header
    /// indicating its length, be suffixed with a character indicating that it's complete, etc. Returns the number
    /// of bytes written to the buffer.
    fn write_message(
        &self,
        target: SocketAddr,
        payload: &[u8],
        buffer: &mut [u8],
    ) -> io::Result<usize>;
}

/// An object dedicated to spawning outbound message handlers; used in the `Writing` protocol.
pub struct WritingHandler {
    sender: mpsc::Sender<ReturnableConnection>,
    _task: JoinHandle<()>,
}

impl WritingHandler {
    /// Sends a returnable `Connection` to a task spawned by the `WritingHandler`.
    pub async fn send(&self, returnable_conn: ReturnableConnection) {
        if self.sender.send(returnable_conn).await.is_err() {
            // can't recover if this happens
            panic!("WritingHandler's Receiver is closed")
        }
    }
}

impl From<(mpsc::Sender<ReturnableConnection>, JoinHandle<()>)> for WritingHandler {
    fn from((sender, _task): (mpsc::Sender<ReturnableConnection>, JoinHandle<()>)) -> Self {
        Self { sender, _task }
    }
}
