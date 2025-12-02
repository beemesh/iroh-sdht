//! Network implementation using iroh QUIC transport.
//!
//! This module provides the [`IrohNetwork`] implementation of the [`DhtNetwork`] trait,
//! enabling DHT RPC communication over iroh's QUIC-based endpoints.
//!
//! # Protocol
//!
//! The network uses the ALPN identifier `myapp/dht/1` for connection negotiation.
//! All RPC calls are serialized using the irpc framework over iroh connections.

use anyhow::Result;
use async_trait::async_trait;
use iroh::Endpoint;
use iroh::EndpointAddr;
use irpc::Client;

use crate::core::{Contact, DhtNetwork, Key, NodeId};
use crate::protocol::{
    DhtProtocol, FindNodeRequest, FindValueRequest, FindValueResponse, PingRequest, StoreRequest,
};

/// ALPN protocol identifier for DHT connections.
pub const DHT_ALPN: &[u8] = b"myapp/dht/1";

/// Network implementation using iroh QUIC transport.
///
/// Wraps an iroh [`Endpoint`] and implements the [`DhtNetwork`] trait to provide
/// RPC communication between DHT nodes over QUIC.
pub struct IrohNetwork {
    /// The iroh endpoint used for QUIC connections.
    pub endpoint: Endpoint,
    /// Contact info for the local node (included in all RPC requests).
    pub self_contact: Contact,
}

impl IrohNetwork {
    /// Parse a contact's address from JSON-serialized EndpointAddr.
    fn parse_addr(&self, contact: &Contact) -> Result<EndpointAddr> {
        Ok(serde_json::from_str(&contact.addr)?)
    }

    /// Create an RPC client for communicating with a remote contact.
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
    /// Send a FIND_NODE RPC to find contacts near a target ID.
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

    /// Send a FIND_VALUE RPC to retrieve a value or get closer contacts.
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

    /// Send a STORE RPC to store a key-value pair on a node.
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

    /// Send a PING RPC to check if a node is responsive.
    async fn ping(&self, to: &Contact) -> Result<()> {
        let client = self.client(to)?;
        client
            .rpc(PingRequest {
                from: self.self_contact.clone(),
            })
            .await?;
        Ok(())
    }
}
