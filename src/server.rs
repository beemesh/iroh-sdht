use std::fmt;
use std::sync::Arc;

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use irpc::channel::mpsc;
use irpc::WithChannels;
use irpc_iroh::IrohProtocol;

use crate::core::{DhtNetwork, DhtNode};
use crate::protocol::{
    DhtMessage, DhtProtocol, FindNodeRequest, FindValueRequest, FindValueResponse, PingRequest,
    StoreRequest,
};

/// The router entry point for inbound `DHT_ALPN` connections.
///
/// `Router::builder(endpoint.clone()).accept(DHT_ALPN, DhtProtocolHandler::new(...))` mirrors
/// the iroh echo example's `start_accept_side`: for each QUIC connection negotiated
/// with our ALPN the router invokes [`ProtocolHandler::accept`], which in turn
/// delegates to `irpc`'s [`IrohProtocol`] implementation backed by a lightweight actor.
#[derive(Clone)]
pub struct DhtProtocolHandler {
    inner: Arc<IrohProtocol<DhtProtocol>>,
}

impl DhtProtocolHandler {
    pub fn new<N: DhtNetwork>(node: Arc<DhtNode<N>>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(run_server(node, rx));
        let inner = IrohProtocol::with_sender(tx);
        Self {
            inner: Arc::new(inner),
        }
    }
}

impl fmt::Debug for DhtProtocolHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DhtProtocolHandler").finish()
    }
}

impl ProtocolHandler for DhtProtocolHandler {
    fn accept(
        &self,
        connection: Connection,
    ) -> impl std::future::Future<Output = Result<(), AcceptError>> + Send {
        self.inner.accept(connection)
    }
}

async fn run_server<N: DhtNetwork>(node: Arc<DhtNode<N>>, mut inbox: mpsc::Receiver<DhtMessage>) {
    while let Ok(Some(msg)) = inbox.recv().await {
        handle_message(node.clone(), msg).await;
    }
}

async fn handle_message<N: DhtNetwork>(node: Arc<DhtNode<N>>, msg: DhtMessage) {
    match msg {
        DhtMessage::FindNode(request) => handle_find_node(node, request).await,
        DhtMessage::FindValue(request) => handle_find_value(node, request).await,
        DhtMessage::Store(request) => handle_store(node, request).await,
        DhtMessage::Ping(request) => handle_ping(request).await,
    }
}

async fn handle_find_node<N: DhtNetwork>(
    node: Arc<DhtNode<N>>,
    request: WithChannels<FindNodeRequest, DhtProtocol>,
) {
    let WithChannels { inner, tx, .. } = request;
    let nodes = node
        .handle_find_node_request(&inner.from, inner.target)
        .await;
    let _ = tx.send(nodes).await;
}

async fn handle_find_value<N: DhtNetwork>(
    node: Arc<DhtNode<N>>,
    request: WithChannels<FindValueRequest, DhtProtocol>,
) {
    let WithChannels { inner, tx, .. } = request;
    let (value, closer) = node.handle_find_value_request(&inner.from, inner.key).await;
    let response = FindValueResponse { value, closer };
    let _ = tx.send(response).await;
}

async fn handle_store<N: DhtNetwork>(
    node: Arc<DhtNode<N>>,
    request: WithChannels<StoreRequest, DhtProtocol>,
) {
    let WithChannels { inner, tx, .. } = request;
    node.handle_store_request(&inner.from, inner.key, inner.value)
        .await;
    let _ = tx.send(()).await;
}

async fn handle_ping(request: WithChannels<PingRequest, DhtProtocol>) {
    let WithChannels { tx, .. } = request;
    let _ = tx.send(()).await;
}
