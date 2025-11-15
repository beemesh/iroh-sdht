use std::fmt;
use std::io;
use std::sync::Arc;

use anyhow::Result;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};

use crate::core::{DhtNetwork, DhtNode};
use crate::framing::{read_frame, write_frame};
use crate::protocol::{Rpc, RpcKind};

pub async fn handle_connection<N: DhtNetwork>(
    node: Arc<DhtNode<N>>,
    conn: Connection,
) -> Result<()> {
    // Accept a single bi-directional stream for simplicity.
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
            // These should not arrive unsolicited in this simple design.
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
    Ok(())
}

#[derive(Clone)]
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
            handle_connection(node, connection)
                .await
                .map_err(|err| AcceptError::from_err(io::Error::new(io::ErrorKind::Other, err)))
        }
    }
}
