use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, Duration};

use iroh_sdht::{Contact, DhtNetwork, DiscoveryNode, Key, NodeId};

#[derive(Clone)]
pub struct TestNetwork {
    registry: Arc<NetworkRegistry>,
    self_contact: Contact,
    latencies: Arc<Mutex<HashMap<NodeId, Duration>>>,
    failures: Arc<Mutex<HashSet<NodeId>>>,
    stores: Arc<Mutex<Vec<(Contact, Key, usize)>>>,
    pings: Arc<Mutex<Vec<NodeId>>>,
}

impl TestNetwork {
    pub fn new(registry: Arc<NetworkRegistry>, self_contact: Contact) -> Self {
        Self {
            registry,
            self_contact,
            latencies: Arc::new(Mutex::new(HashMap::new())),
            failures: Arc::new(Mutex::new(HashSet::new())),
            stores: Arc::new(Mutex::new(Vec::new())),
            pings: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn set_latency(&self, node: NodeId, latency: Duration) {
        let mut latencies = self.latencies.lock().await;
        latencies.insert(node, latency);
    }

    pub async fn set_failure(&self, node: NodeId, fail: bool) {
        let mut failures = self.failures.lock().await;
        if fail {
            failures.insert(node);
        } else {
            failures.remove(&node);
        }
    }

    pub async fn store_calls(&self) -> Vec<(Contact, Key, usize)> {
        let stores = self.stores.lock().await;
        stores.clone()
    }

    pub async fn ping_calls(&self) -> Vec<NodeId> {
        let calls = self.pings.lock().await;
        calls.clone()
    }
}

#[derive(Default)]
pub struct NetworkRegistry {
    peers: RwLock<HashMap<NodeId, DiscoveryNode<TestNetwork>>>,
}

impl NetworkRegistry {
    pub async fn register(&self, node: &DiscoveryNode<TestNetwork>) {
        let mut peers = self.peers.write().await;
        peers.insert(node.contact().id, node.clone());
    }

    pub async fn get(&self, id: &NodeId) -> Option<DiscoveryNode<TestNetwork>> {
        let peers = self.peers.read().await;
        peers.get(id).cloned()
    }
}

#[async_trait::async_trait]
impl DhtNetwork for TestNetwork {
    async fn find_node(&self, to: &Contact, target: NodeId) -> Result<Vec<Contact>> {
        if self.should_fail(&to.id).await {
            return Err(anyhow!("injected network failure"));
        }
        self.maybe_sleep(&to.id).await;
        if let Some(peer) = self.registry.get(&to.id).await {
            Ok(peer
                .handle_find_node_request(&self.self_contact, target)
                .await)
        } else {
            Ok(Vec::new())
        }
    }

    async fn find_value(&self, to: &Contact, key: Key) -> Result<(Option<Vec<u8>>, Vec<Contact>)> {
        if self.should_fail(&to.id).await {
            return Err(anyhow!("injected network failure"));
        }
        self.maybe_sleep(&to.id).await;
        if let Some(peer) = self.registry.get(&to.id).await {
            Ok(peer
                .handle_find_value_request(&self.self_contact, key)
                .await)
        } else {
            Ok((None, Vec::new()))
        }
    }

    async fn store(&self, to: &Contact, key: Key, value: Vec<u8>) -> Result<()> {
        if self.should_fail(&to.id).await {
            return Err(anyhow!("injected network failure"));
        }
        self.maybe_sleep(&to.id).await;
        {
            let mut stores = self.stores.lock().await;
            stores.push((to.clone(), key, value.len()));
        }
        if let Some(peer) = self.registry.get(&to.id).await {
            peer.handle_store_request(&self.self_contact, key, value)
                .await;
        }
        Ok(())
    }

    async fn ping(&self, to: &Contact) -> Result<()> {
        if self.should_fail(&to.id).await {
            return Err(anyhow!("injected network failure"));
        }
        self.maybe_sleep(&to.id).await;
        {
            let mut calls = self.pings.lock().await;
            calls.push(to.id);
        }
        if self.registry.get(&to.id).await.is_some() {
            Ok(())
        } else {
            Err(anyhow!("peer not reachable"))
        }
    }
}

impl TestNetwork {
    async fn should_fail(&self, node: &NodeId) -> bool {
        let failures = self.failures.lock().await;
        failures.contains(node)
    }

    async fn maybe_sleep(&self, node: &NodeId) {
        let latency = {
            let latencies = self.latencies.lock().await;
            latencies.get(node).copied()
        };
        if let Some(delay) = latency {
            sleep(delay).await;
        }
    }
}

pub struct TestNode {
    pub node: DiscoveryNode<TestNetwork>,
    pub network: TestNetwork,
}

impl TestNode {
    pub async fn new(registry: Arc<NetworkRegistry>, index: u32, k: usize, alpha: usize) -> Self {
        let contact = make_contact(index);
        let network = TestNetwork::new(registry.clone(), contact.clone());
        let node = DiscoveryNode::new(contact.id, contact.clone(), network.clone(), k, alpha);
        registry.register(&node).await;
        Self { node, network }
    }

    pub fn contact(&self) -> Contact {
        self.node.contact()
    }
}

pub fn make_node_id(index: u32) -> NodeId {
    let mut id = [0u8; 32];
    id[..4].copy_from_slice(&index.to_be_bytes());
    id
}

pub fn make_contact(index: u32) -> Contact {
    Contact {
        id: make_node_id(index),
        addr: format!("node-{index}"),
    }
}
