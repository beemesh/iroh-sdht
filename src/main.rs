//! Example DHT node binary demonstrating iroh-sdht usage.
//!
//! This binary starts a DHT node with mDNS discovery for local network peer
//! discovery and QUIC relay support for NAT traversal. It demonstrates the
//! basic setup pattern for using the iroh-sdht library.
//!
//! # Usage
//!
//! ```bash
//! # Default (info level)
//! cargo run
//!
//! # With debug logging
//! RUST_LOG=debug cargo run
//!
//! # With trace logging (very verbose)
//! RUST_LOG=trace cargo run
//!
//! # Filter to specific modules
//! RUST_LOG=iroh_sdht=debug cargo run
//! ```
//!
//! The node will start and print its NodeId and endpoint address. Telemetry
//! is printed every 5 minutes showing the current state of the node.

use anyhow::Result;
use futures::future;
use iroh::discovery::mdns::MdnsDiscovery;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, RelayMode};
use tokio::time::{self, Duration};
use tracing::{info, warn};
use tracing_subscriber::{fmt, EnvFilter};

use iroh_sdht::{
    derive_node_id, Contact, DhtProtocolHandler, DiscoveryNode, IrohNetwork, NodeId, DHT_ALPN,
};

/// Default bucket size and replication factor.
const K: usize = 20;
/// Default parallelism for concurrent lookups.
const ALPHA: usize = 3;

/// Convert an iroh endpoint ID to a DHT node ID using BLAKE3.
fn endpoint_id_to_node_id(endpoint: &Endpoint) -> NodeId {
    derive_node_id(endpoint.id().as_bytes())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing subscriber with env filter
    // Default to "info" level, can be overridden with RUST_LOG env var
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .init();

    // Create an iroh endpoint with our DHT ALPN.
    // Peers connecting with this ALPN will be routed to our DhtProtocolHandler.
    let endpoint = Endpoint::builder()
        .alpns(vec![DHT_ALPN.to_vec()])
        .relay_mode(RelayMode::Default)
        .bind()
        .await?;

    // Try to enable mDNS for local network discovery.
    if let Err(err) = enable_local_mdns(&endpoint) {
        warn!("Failed to initialize mDNS discovery: {err:?}; continuing with relay-only mode");
    } else {
        info!("mDNS discovery enabled; will fall back to relay if unavailable");
    }

    // Derive our node ID from the endpoint's public key.
    let node_id = endpoint_id_to_node_id(&endpoint);
    let endpoint_addr: EndpointAddr = endpoint.addr();

    // Create our contact info for sharing with peers.
    let addr_json = serde_json::to_string(&endpoint_addr)?;
    let self_contact = Contact {
        id: node_id,
        addr: addr_json.clone(),
    };

    info!("DHT node started");
    info!(node_id = %hex::encode(node_id), "NodeId");
    info!(addr = %addr_json, "Endpoint address");

    // Create the network layer and discovery node.
    let network = IrohNetwork {
        endpoint: endpoint.clone(),
        self_contact: self_contact.clone(),
    };

    let dht = DiscoveryNode::new(node_id, self_contact.clone(), network, K, ALPHA);

    // Start the protocol handler to accept incoming DHT connections.
    let _router = Router::builder(endpoint.clone())
        .accept(DHT_ALPN, DhtProtocolHandler::new(dht.clone()))
        .spawn();

    // Spawn a background task to periodically log telemetry.
    let telemetry_node = dht.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(300));
        loop {
            interval.tick().await;
            let snapshot = telemetry_node.telemetry_snapshot().await;
            info!(
                pressure = format!("{:.2}", snapshot.pressure),
                stored_keys = snapshot.stored_keys,
                tier_counts = ?snapshot.tier_counts,
                tier_centroids = ?snapshot.tier_centroids,
                k = snapshot.replication_factor,
                alpha = snapshot.concurrency,
                "telemetry snapshot"
            );
        }
    });

    // Park the main task indefinitely.
    // A real application would expose an API for feeding peer contacts and performing lookups.
    future::pending::<()>().await;
    Ok(())
}

/// Enable mDNS discovery for the endpoint.
///
/// This allows automatic discovery of other iroh-sdht nodes on the local network.
fn enable_local_mdns(endpoint: &Endpoint) -> anyhow::Result<()> {
    let mdns = MdnsDiscovery::builder()
        .service_name("iroh-sdht")
        .build(endpoint.id())
        .map_err(|err| anyhow::anyhow!("mDNS discovery initialization failed: {err}"))?;
    endpoint.discovery().add(mdns);
    Ok(())
}
