use anyhow::Result;
use async_trait::async_trait;
use iroh::Endpoint;
use iroh::EndpointAddr;
use irpc::Client;

use crate::core::{Contact, DhtNetwork, Key, NodeId};
use crate::protocol::{
    DhtProtocol, FindNodeRequest, FindValueRequest, FindValueResponse, StoreRequest,
};

pub const DHT_ALPN: &[u8] = b"myapp/dht/1";

pub struct IrohNetwork {
    pub endpoint: Endpoint,
    pub self_contact: Contact,
}

impl IrohNetwork {
    fn parse_addr(&self, contact: &Contact) -> Result<EndpointAddr> {
        Ok(serde_json::from_str(&contact.addr)?)
    }

    fn client(&self, contact: &Contact) -> Result<Client<DhtProtocol>> {
        let addr = self.parse_addr(contact)?;
        Ok(irpc_iroh::client::<DhtProtocol>(
            self.endpoint.clone(),
            addr,
            DHT_ALPN,
        ))
    }
}

#[async_trait]
impl DhtNetwork for IrohNetwork {
    async fn find_node(&self, to: &Contact, target: NodeId) -> Result<Vec<Contact>> {
        let client = self.client(to)?;
        let nodes = client
            .rpc(FindNodeRequest {
                from: self.self_contact.clone(),
                target,
            })
            .await?;
        Ok(nodes)
    }

    async fn find_value(&self, to: &Contact, key: Key) -> Result<(Option<Vec<u8>>, Vec<Contact>)> {
        let client = self.client(to)?;
        let FindValueResponse { value, closer } = client
            .rpc(FindValueRequest {
                from: self.self_contact.clone(),
                key,
            })
            .await?;
        Ok((value, closer))
    }

    async fn store(&self, to: &Contact, key: Key, value: Vec<u8>) -> Result<()> {
        let client = self.client(to)?;
        client
            .rpc(StoreRequest {
                from: self.self_contact.clone(),
                key,
                value,
            })
            .await?;
        Ok(())
    }
}
