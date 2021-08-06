use crate::Pea2Pea;

use tokio::{
    sync::{mpsc, oneshot},
    task,
};
use tracing::*;

use std::net::SocketAddr;

/// Can be used to automatically perform some extra actions when the node disconnects from its
/// peer, which is especially practical if the disconnect itself is triggered automatically,
/// e.g. due to the peer exceeding the allowed number of failures or severing its connection
/// with the node on its own.
#[async_trait::async_trait]
pub trait Disconnect: Pea2Pea
where
    Self: Clone + Send + Sync + 'static,
{
    /// Attaches the behavior specified in `Disconnect::handle_disconnect` to every occurrence of the
    /// node disconnecting from a peer.
    fn enable_disconnect(&self) {
        let (from_node_sender, mut from_node_receiver) =
            mpsc::channel::<(SocketAddr, oneshot::Sender<()>)>(
                self.node().config().protocol_handler_queue_depth,
            );

        // spawn a background task dedicated to handling disconnect events
        let self_clone = self.clone();
        let disconnect_task = tokio::spawn(async move {
            trace!(parent: self_clone.node().span(), "spawned the Disconnect handler task");

            while let Some((addr, notifier)) = from_node_receiver.recv().await {
                let self_clone2 = self_clone.clone();
                task::spawn(async move {
                    // perform the specified extra actions
                    self_clone2.handle_disconnect(addr).await;
                    // notify the node that the extra actions have concluded
                    // and that the related connection can be dropped
                    let _ = notifier.send(()); // can't really fail
                });
            }
        });
        self.node().tasks.lock().push(disconnect_task);

        self.node().set_disconnect_handler(from_node_sender.into());
    }

    /// The extra actions to be executed during a disconnect; in order to still be able to
    /// communicate with the peer in the usual manner, only its `SocketAddr` (as opposed
    /// to the related `Connection` object) is provided as an argument.
    async fn handle_disconnect(&self, addr: SocketAddr);
}