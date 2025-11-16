use irpc::channel::oneshot;
use irpc::rpc_requests;
use serde::{Deserialize, Serialize};

use crate::core::{Contact, Key, NodeId};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PingRequest {
    pub from: Contact,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FindNodeRequest {
    pub from: Contact,
    pub target: NodeId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FindValueRequest {
    pub from: Contact,
    pub key: Key,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreRequest {
    pub from: Contact,
    pub key: Key,
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FindValueResponse {
    pub value: Option<Vec<u8>>,
    pub closer: Vec<Contact>,
}

#[rpc_requests(message = DhtMessage)]
#[derive(Debug, Serialize, Deserialize)]
pub enum DhtProtocol {
    #[rpc(tx = oneshot::Sender<Vec<Contact>>)]
    FindNode(FindNodeRequest),
    #[rpc(tx = oneshot::Sender<FindValueResponse>)]
    FindValue(FindValueRequest),
    #[rpc(tx = oneshot::Sender<()>)]
    Store(StoreRequest),
    #[rpc(tx = oneshot::Sender<()>)]
    Ping(PingRequest),
}
