use std::sync::Arc;

use anyhow::Result;
use futures::future;
use iroh::discovery::mdns::MdnsDiscovery;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, RelayMode};
use tokio::time::{self, Duration};

use iroh_sdht::{
    derive_node_id, Contact, DhtNode, DhtProtocolHandler, IrohNetwork, NodeId, DHT_ALPN,
};

const K: usize = 20; // bucket/replication size
const ALPHA: usize = 3; // concurrent lookups

fn endpoint_id_to_node_id(endpoint: &Endpoint) -> NodeId {
    derive_node_id(endpoint.id().as_bytes())
}

#[tokio::main]
async fn main() -> Result<()> {
    let endpoint = Endpoint::builder()
        // Match the echo example by configuring our custom ALPN up front.  Any
        // peer selecting `DHT_ALPN` will be routed to `DhtProtocolHandler` below.
        .alpns(vec![DHT_ALPN.to_vec()])
        .relay_mode(RelayMode::Default)
        .bind()
        .await?;

    if let Err(err) = enable_local_mdns(&endpoint) {
        eprintln!("Failed to initialize mDNS discovery ({err:?}); continuing with relay-only mode");
    } else {
        println!("mDNS discovery enabled; will fall back to relay if unavailable");
    }

    let node_id = endpoint_id_to_node_id(&endpoint);
    let endpoint_addr: EndpointAddr = endpoint.addr();

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

    // Mirrors the `start_accept_side` snippet from the echo example: register a
    // protocol handler for the DHT ALPN so every incoming connection is handed to
    // `handle_connection`.
    let _router = Router::builder(endpoint.clone())
        .accept(DHT_ALPN, DhtProtocolHandler::new(dht.clone()))
        .spawn();

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

fn enable_local_mdns(endpoint: &Endpoint) -> anyhow::Result<()> {
    let mdns = MdnsDiscovery::builder()
        .service_name("iroh-kademlia-dht")
        .build(endpoint.id())
        .map_err(|err| anyhow::anyhow!("mDNS discovery initialization failed: {err}"))?;
    endpoint.discovery().add(mdns);
    Ok(())
}
