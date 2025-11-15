use std::sync::Arc;

use anyhow::Result;
use futures::future;
use iroh::net::discovery::{ConcurrentDiscovery, Discovery};
use iroh::net::key::PublicKey;
use iroh::net::relay::RelayMode;
use iroh::net::{MagicEndpoint, NodeAddr};
use tokio::time::{self, Duration};

use iroh_kademlia_dht::{
    derive_node_id, handle_connection, Contact, DhtNode, IrohNetwork, NodeId, DHT_ALPN,
};
use iroh_mdns::MdnsDiscovery;

const K: usize = 20; // bucket/replication size
const ALPHA: usize = 3; // concurrent lookups

fn public_key_to_node_id(pk: PublicKey) -> NodeId {
    derive_node_id(pk.as_bytes())
}

fn endpoint_id_to_node_id(endpoint: &MagicEndpoint) -> NodeId {
    public_key_to_node_id(endpoint.node_id())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut endpoint_builder = MagicEndpoint::builder()
        .alpns(vec![DHT_ALPN.to_vec()])
        .relay_mode(RelayMode::Default);

    match build_discovery_service() {
        Ok(discovery) => {
            endpoint_builder = endpoint_builder.discovery(discovery);
            println!("mDNS discovery enabled; will fall back to relay if unavailable");
        }
        Err(err) => {
            eprintln!(
                "Failed to initialize mDNS discovery ({err:?}); continuing with relay-only mode"
            );
        }
    }

    let endpoint = endpoint_builder.bind(0).await?;
    let node_id = endpoint_id_to_node_id(&endpoint);
    let endpoint_addr: NodeAddr = endpoint.my_addr().await?;

    let addr_json = serde_json::to_string(&endpoint_addr)?;
    let self_contact = Contact {
        id: node_id,
        addr: addr_json.clone(),
    };

    println!("DHT node started");
    println!("  NodeId (hex): {}", hex::encode(node_id));
    println!("  Endpoint addr JSON: {}", addr_json);

    let network = IrohNetwork {
        endpoint: endpoint.clone(),
        self_contact: self_contact.clone(),
    };

    let dht = Arc::new(DhtNode::new(
        node_id,
        self_contact.clone(),
        network,
        K,
        ALPHA,
    ));

    // Accept incoming connections in background
    let dht_clone = dht.clone();
    let endpoint_clone = endpoint.clone();

    tokio::spawn(async move {
        loop {
            let Some(connecting) = endpoint_clone.accept().await else {
                break;
            };
            let dht_inner = dht_clone.clone();
            tokio::spawn(async move {
                match connecting.await {
                    Ok(conn) => {
                        if let Err(e) = handle_connection(dht_inner, conn).await {
                            eprintln!("connection error: {e:?}");
                        }
                    }
                    Err(e) => {
                        eprintln!("connection handshake error: {e:?}");
                    }
                }
            });
        }
    });

    let telemetry_node = dht.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(300));
        loop {
            interval.tick().await;
            let snapshot = telemetry_node.telemetry_snapshot().await;
            println!(
                "Telemetry: pressure={:.2}, stored_keys={}, tiers={:?}, centroids={:?}, k={}, alpha={}",
                snapshot.pressure,
                snapshot.stored_keys,
                snapshot.tier_counts,
                snapshot.tier_centroids,
                snapshot.replication_factor,
                snapshot.concurrency,
            );
        }
    });

    // For now, just park the main task.
    // In a real app, you'd expose an API (HTTP/CLI/etc.) that calls dht.put() / dht.get().
    future::pending::<()>().await;
    Ok(())
}

fn build_discovery_service() -> anyhow::Result<Box<dyn Discovery>> {
    let mdns = MdnsDiscovery::new("iroh-kademlia-dht")
        .map_err(|err| anyhow::anyhow!("mDNS discovery initialization failed: {err}"))?;
    let mut discovery = ConcurrentDiscovery::new();
    discovery.add(mdns);
    Ok(Box::new(discovery))
}
