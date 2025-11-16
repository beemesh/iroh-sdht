use std::fmt;
use std::io;
use std::sync::Arc;

use anyhow::Result;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};

use crate::core::{DhtNetwork, DhtNode};
use crate::framing::{read_frame, write_frame};
use crate::protocol::{Rpc, RpcKind};

/// Handles a single incoming connection for the DHT ALPN.
///
/// The router invokes this for every connection whose peer selected [`crate::net::DHT_ALPN`].
/// Much like the iroh echo example we limit the connection lifetime to a single
/// request/response exchange for now.  Supporting multiple bi-directional
/// streams per connection would require extra state management; a TODO below
/// documents that future enhancement.
pub async fn handle_connection<N: DhtNetwork>(
    node: Arc<DhtNode<N>>,
    conn: Connection,
) -> Result<()> {
    // Accept a single bi-directional stream for simplicity.  This mirrors the
    // echo example's shape; adding a loop here would allow multiple RPCs per
    // connection once the RPC framing grows a notion of session.
    let (mut send, mut recv): (SendStream, RecvStream) = conn.accept_bi().await?;
    let maybe_bytes = read_frame(&mut recv).await?;
    if maybe_bytes.is_none() {
        return Ok(());
    }

    let bytes = maybe_bytes.unwrap();
    let rpc: Rpc = serde_json::from_slice(&bytes)?;

    let from = rpc.from.clone();

    let reply_kind = match rpc.kind {
        RpcKind::Ping => RpcKind::Pong,
        RpcKind::FindNode { target } => {
            let nodes = node.handle_find_node_request(&from, target).await;
            RpcKind::Nodes { nodes }
        }
        RpcKind::FindValue { key } => {
            let (val, closer) = node.handle_find_value_request(&from, key).await;
            RpcKind::Value {
                key,
                value: val,
                closer,
            }
        }
        RpcKind::Store { key, value } => {
            node.handle_store_request(&from, key, value).await;
            // fire-and-forget; send back simple Pong.
            RpcKind::Pong
        }
        RpcKind::Pong => {
            // For now just ack with Pong again.
            RpcKind::Pong
        }
        RpcKind::Nodes { .. } | RpcKind::Value { .. } => {
            // These are responses in our RPC vocabulary and should never arrive
            // as unsolicited requests.  We respond with a simple Pong so the
            // peer learns we are alive without leaking routing information.
            RpcKind::Pong
        }
    };

    let reply = Rpc {
        from: node.self_contact.clone(),
        kind: reply_kind,
    };

    let reply_bytes = serde_json::to_vec(&reply)?;
    write_frame(&mut send, &reply_bytes).await?;
    send.finish()?;
    // We intentionally do not close the connection from the server side.  The
    // client performs the final read of the response frame, so following the
    // echo example's guidance it is responsible for closing the connection.  Our
    // handler simply finishes the stream and returns, which drops the
    // connection/stream handles.
    Ok(())
}

#[derive(Clone)]
/// The router entry point for inbound `DHT_ALPN` connections.
///
/// `Router::builder(endpoint.clone()).accept(DHT_ALPN, DhtProtocolHandler::new(...))` mirrors
/// the iroh echo example's `start_accept_side`: for each QUIC connection negotiated
/// with our ALPN the router invokes [`ProtocolHandler::accept`], which in turn
/// delegates to [`handle_connection`].
pub struct DhtProtocolHandler<N: DhtNetwork> {
    node: Arc<DhtNode<N>>,
}

impl<N: DhtNetwork> DhtProtocolHandler<N> {
    pub fn new(node: Arc<DhtNode<N>>) -> Self {
        Self { node }
    }
}

impl<N: DhtNetwork> fmt::Debug for DhtProtocolHandler<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DhtProtocolHandler").finish()
    }
}

impl<N: DhtNetwork> ProtocolHandler for DhtProtocolHandler<N> {
    fn accept(
        &self,
        connection: Connection,
    ) -> impl std::future::Future<Output = Result<(), AcceptError>> + Send {
        let node = self.node.clone();
        async move {
            // Every incoming connection for `DHT_ALPN` is handed to
            // `handle_connection`.  Any error from the handler is wrapped as an
            // `AcceptError` so the router can tear the connection down cleanly.
            handle_connection(node, connection)
                .await
                .map_err(|err| AcceptError::from_err(io::Error::new(io::ErrorKind::Other, err)))
        }
    }
}
