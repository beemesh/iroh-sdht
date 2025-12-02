//! DHT protocol server for handling incoming RPC requests.
//!
//! This module provides the [`DhtProtocolHandler`] which integrates with iroh's
//! router to handle incoming DHT protocol connections. It dispatches incoming
//! RPC messages to the appropriate handlers on the [`DiscoveryNode`].
//!
//! # Usage
//!
//! ```ignore
//! let handler = DhtProtocolHandler::new(discovery_node);
//! let router = Router::builder(endpoint.clone())
//!     .accept(DHT_ALPN, handler)
//!     .spawn()
//!     .await?;
//! ```

use std::fmt;
use std::sync::Arc;

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use irpc::channel::mpsc;
use irpc::WithChannels;
use irpc_iroh::IrohProtocol;
use tracing::{debug, trace};

use crate::core::{DhtNetwork, DiscoveryNode};
use crate::protocol::{
    DhtMessage, DhtProtocol, FindNodeRequest, FindValueRequest, FindValueResponse, PingRequest,
    StoreRequest,
};

/// Protocol handler for incoming DHT connections.
///
/// Integrates with iroh's Router to handle connections negotiated with the DHT ALPN.
/// Each connection is handled by irpc's protocol machinery, which dispatches
/// incoming RPC messages to a background actor that calls into the [`DiscoveryNode`].
#[derive(Clone)]
pub struct DhtProtocolHandler {
    /// The irpc protocol handler that manages connection state.
    inner: Arc<IrohProtocol<DhtProtocol>>,
}

impl DhtProtocolHandler {
    /// Create a new protocol handler backed by the given discovery node.
    ///
    /// Spawns a background task to process incoming RPC messages.
    pub fn new<N: DhtNetwork>(node: DiscoveryNode<N>) -> Self {
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
    /// Accept an incoming connection and delegate to the irpc protocol handler.
    fn accept(
        &self,
        connection: Connection,
    ) -> impl std::future::Future<Output = Result<(), AcceptError>> + Send {
        self.inner.accept(connection)
    }
}

/// Background task that processes incoming RPC messages.
async fn run_server<N: DhtNetwork>(node: DiscoveryNode<N>, mut inbox: mpsc::Receiver<DhtMessage>) {
    while let Ok(Some(msg)) = inbox.recv().await {
        handle_message(node.clone(), msg).await;
    }
}

/// Dispatch an incoming message to the appropriate handler.
async fn handle_message<N: DhtNetwork>(node: DiscoveryNode<N>, msg: DhtMessage) {
    match msg {
        DhtMessage::FindNode(request) => handle_find_node(node.clone(), request).await,
        DhtMessage::FindValue(request) => handle_find_value(node.clone(), request).await,
        DhtMessage::Store(request) => handle_store(node.clone(), request).await,
        DhtMessage::Ping(request) => handle_ping(request).await,
    }
}

/// Handle a FIND_NODE RPC request.
async fn handle_find_node<N: DhtNetwork>(
    node: DiscoveryNode<N>,
    request: WithChannels<FindNodeRequest, DhtProtocol>,
) {
    let WithChannels { inner, tx, .. } = request;
    trace!(
        from = ?hex::encode(&inner.from.id[..8]),
        target = ?hex::encode(&inner.target[..8]),
        "handling FIND_NODE request"
    );
    let nodes = node
        .handle_find_node_request(&inner.from, inner.target)
        .await;
    debug!(
        from = ?hex::encode(&inner.from.id[..8]),
        returned = nodes.len(),
        "FIND_NODE response"
    );
    let _ = tx.send(nodes).await;
}

/// Handle a FIND_VALUE RPC request.
async fn handle_find_value<N: DhtNetwork>(
    node: DiscoveryNode<N>,
    request: WithChannels<FindValueRequest, DhtProtocol>,
) {
    let WithChannels { inner, tx, .. } = request;
    trace!(
        from = ?hex::encode(&inner.from.id[..8]),
        key = ?hex::encode(&inner.key[..8]),
        "handling FIND_VALUE request"
    );
    let (value, closer) = node.handle_find_value_request(&inner.from, inner.key).await;
    let found = value.is_some();
    debug!(
        from = ?hex::encode(&inner.from.id[..8]),
        found = found,
        closer_nodes = closer.len(),
        "FIND_VALUE response"
    );
    let response = FindValueResponse { value, closer };
    let _ = tx.send(response).await;
}

/// Handle a STORE RPC request.
async fn handle_store<N: DhtNetwork>(
    node: DiscoveryNode<N>,
    request: WithChannels<StoreRequest, DhtProtocol>,
) {
    let WithChannels { inner, tx, .. } = request;
    debug!(
        from = ?hex::encode(&inner.from.id[..8]),
        key = ?hex::encode(&inner.key[..8]),
        value_len = inner.value.len(),
        "handling STORE request"
    );
    node.handle_store_request(&inner.from, inner.key, inner.value)
        .await;
    let _ = tx.send(()).await;
}

/// Handle a PING RPC request.
async fn handle_ping(request: WithChannels<PingRequest, DhtProtocol>) {
    let WithChannels { inner, tx, .. } = request;
    trace!(
        from = ?hex::encode(&inner.from.id[..8]),
        "handling PING request"
    );
    let _ = tx.send(()).await;
}
