use anyhow::{anyhow, Result};
use async_trait::async_trait;
use iroh::Endpoint;
use iroh::EndpointAddr;

use crate::core::{Contact, DhtNetwork, Key, NodeId};
use crate::framing::{read_frame, write_frame};
use crate::protocol::{Rpc, RpcKind};

pub const DHT_ALPN: &[u8] = b"myapp/dht/1";

pub struct IrohNetwork {
    pub endpoint: Endpoint,
    pub self_contact: Contact,
}

impl IrohNetwork {
    fn parse_addr(&self, contact: &Contact) -> Result<EndpointAddr> {
        Ok(serde_json::from_str(&contact.addr)?)
    }

    async fn call_rpc(&self, to: &Contact, rpc: Rpc) -> Result<Rpc> {
        let addr = self.parse_addr(to)?;
        // For the moment we create a fresh QUIC connection per RPC.  This keeps the
        // implementation very close to the iroh echo example: a single
        // request/response exchange per connection and bi-directional stream.  A
        // future optimisation could pool long-lived connections per peer, but that
        // would require connection-liveness tracking inside [`DhtNetwork`].
        let conn = self.endpoint.connect(addr, DHT_ALPN).await?;
        let (mut send, mut recv) = conn.open_bi().await?;

        let bytes = serde_json::to_vec(&rpc)?;
        write_frame(&mut send, &bytes).await?;
        send.finish()?;

        if let Some(resp_bytes) = read_frame(&mut recv).await? {
            let resp: Rpc = serde_json::from_slice(&resp_bytes)?;

            // Per the echo example's guidance, "the side that last reads should
            // close" so that graceful shutdown propagates.  The client performs the
            // final read in this RPC flow, so we explicitly close the connection
            // once we have decoded the response.  Waiting for `closed()` allows the
            // peer to observe the close cleanly before we drop the handle.
            conn.close(0u32.into(), b"dht-rpc-complete");
            let _ = conn.closed().await;

            Ok(resp)
        } else {
            Err(anyhow!("no response frame"))
        }
    }
}

#[async_trait]
impl DhtNetwork for IrohNetwork {
    async fn find_node(&self, to: &Contact, target: NodeId) -> Result<Vec<Contact>> {
        let rpc = Rpc {
            from: self.self_contact.clone(),
            kind: RpcKind::FindNode { target },
        };
        let resp = self.call_rpc(to, rpc).await?;
        match resp.kind {
            RpcKind::Nodes { nodes } => Ok(nodes),
            _ => Err(anyhow!("unexpected RPC response to FindNode")),
        }
    }

    async fn find_value(&self, to: &Contact, key: Key) -> Result<(Option<Vec<u8>>, Vec<Contact>)> {
        let rpc = Rpc {
            from: self.self_contact.clone(),
            kind: RpcKind::FindValue { key },
        };
        let resp = self.call_rpc(to, rpc).await?;
        match resp.kind {
            RpcKind::Value {
                key: _k,
                value,
                closer,
            } => Ok((value, closer)),
            _ => Err(anyhow!("unexpected RPC response to FindValue")),
        }
    }

    async fn store(&self, to: &Contact, key: Key, value: Vec<u8>) -> Result<()> {
        let rpc = Rpc {
            from: self.self_contact.clone(),
            kind: RpcKind::Store { key, value },
        };
        let _ = self.call_rpc(to, rpc).await?;
        Ok(())
    }
}
