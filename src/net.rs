use anyhow::{anyhow, Result};
use async_trait::async_trait;
use quinn::{RecvStream, SendStream};

use iroh::net::MagicEndpoint;
use iroh::net::NodeAddr;

use crate::core::{Contact, DhtNetwork, Key, NodeId};
use crate::framing::{read_frame, write_frame};
use crate::protocol::{Rpc, RpcKind};

pub const DHT_ALPN: &[u8] = b"myapp/dht/1";

pub struct IrohNetwork {
    pub endpoint: MagicEndpoint,
    pub self_contact: Contact,
}

impl IrohNetwork {
    fn parse_addr(&self, contact: &Contact) -> Result<NodeAddr> {
        Ok(serde_json::from_str(&contact.addr)?)
    }

    async fn call_rpc(&self, to: &Contact, rpc: Rpc) -> Result<Rpc> {
        let addr = self.parse_addr(to)?;
        let conn = self.endpoint.connect(addr, DHT_ALPN).await?;
        let (mut send, mut recv): (SendStream, RecvStream) = conn.open_bi().await?;

        let bytes = serde_json::to_vec(&rpc)?;
        write_frame(&mut send, &bytes).await?;
        send.finish().await?;

        if let Some(resp_bytes) = read_frame(&mut recv).await? {
            let resp: Rpc = serde_json::from_slice(&resp_bytes)?;
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
