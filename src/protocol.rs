use serde::{Deserialize, Serialize};

use crate::core::{Contact, Key, NodeId};

#[derive(Debug, Serialize, Deserialize)]
pub enum RpcKind {
    Ping,
    Pong,
    FindNode {
        target: NodeId,
    },
    Nodes {
        nodes: Vec<Contact>,
    },
    FindValue {
        key: Key,
    },
    Value {
        key: Key,
        value: Option<Vec<u8>>,
        closer: Vec<Contact>,
    },
    Store {
        key: Key,
        value: Vec<u8>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Rpc {
    pub from: Contact,
    pub kind: RpcKind,
}
