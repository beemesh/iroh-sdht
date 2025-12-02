//! DHT protocol message definitions.
//!
//! This module defines the RPC request and response types for DHT communication
//! using the irpc framework. All messages are serializable for network transport.

use irpc::channel::oneshot;
use irpc::rpc_requests;
use serde::{Deserialize, Serialize};

use crate::core::{Contact, Key, NodeId};

/// Ping request to check if a node is responsive.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PingRequest {
    /// The sender's contact information.
    pub from: Contact,
}

/// Find nodes closest to a target ID.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FindNodeRequest {
    /// The sender's contact information.
    pub from: Contact,
    /// The target node ID to find neighbors for.
    pub target: NodeId,
}

/// Find a value by key, or get closer nodes if not found.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FindValueRequest {
    /// The sender's contact information.
    pub from: Contact,
    /// The key to look up.
    pub key: Key,
}

/// Store a key-value pair on a node.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreRequest {
    /// The sender's contact information.
    pub from: Contact,
    /// The key to store.
    pub key: Key,
    /// The value to store.
    pub value: Vec<u8>,
}

/// Response to a FIND_VALUE request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FindValueResponse {
    /// The value if found locally.
    pub value: Option<Vec<u8>>,
    /// Closer nodes to continue the lookup if value not found.
    pub closer: Vec<Contact>,
}

/// The DHT protocol definition using irpc's RPC framework.
///
/// Each variant represents an RPC method with its associated request type
/// and response channel type.
#[rpc_requests(message = DhtMessage)]
#[derive(Debug, Serialize, Deserialize)]
pub enum DhtProtocol {
    /// Find nodes closest to a target ID. Returns a list of contacts.
    #[rpc(tx = oneshot::Sender<Vec<Contact>>)]
    FindNode(FindNodeRequest),
    /// Find a value by key. Returns the value and/or closer nodes.
    #[rpc(tx = oneshot::Sender<FindValueResponse>)]
    FindValue(FindValueRequest),
    /// Store a key-value pair. Returns acknowledgment.
    #[rpc(tx = oneshot::Sender<()>)]
    Store(StoreRequest),
    /// Ping to check responsiveness. Returns acknowledgment.
    #[rpc(tx = oneshot::Sender<()>)]
    Ping(PingRequest),
}
