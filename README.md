# Iroh sDHT

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/iroh_sdht.svg)](https://crates.io/crates/iroh_sdht)
[![Documentation](https://docs.rs/iroh_sdht/badge.svg)](https://docs.rs/iroh_sdht)

A Kademlia-inspired, latency-aware "sloppy DHT" (sDHT) with adaptive dynamic tiering and backpressure controls, built on [iroh](https://github.com/n0-computer/iroh)'s QUIC transport.

This crate provides a lightweight, embeddable distributed hash table for iroh-based applications. It is transport-agnostic at the core and ships with an iroh-based network implementation and server glue.

---

## Features

- **Kademlia-style routing** with 256 buckets and XOR distance metric
- **Adaptive k parameter** (10-30) that adjusts based on network churn
- **Latency-based tiering** using k-means clustering for peer prioritization
- **Backpressure controls** with O(1) LRU eviction and pressure monitoring
- **Content-addressed storage** using BLAKE3 hashing
- **QUIC transport** via iroh with mDNS discovery and relay support
- **Telemetry snapshots** for monitoring and debugging

---

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
iroh_sdht = "0.1"
```

---

## Quick Start

```rust
use anyhow::Result;
use iroh::{Endpoint, EndpointAddr};
use iroh::protocol::Router;
use iroh_sdht::{
    derive_node_id, Contact, DiscoveryNode, DhtProtocolHandler,
    IrohNetwork, DHT_ALPN,
};

async fn launch(endpoint: Endpoint, addr: EndpointAddr) -> Result<()> {
    // Derive node ID from endpoint identity
    let node_id = derive_node_id(endpoint.id().as_bytes());

    // Build contact info
    let self_contact = Contact {
        id: node_id,
        addr: serde_json::to_string(&addr)?,
    };

    // Create network layer
    let network = IrohNetwork {
        endpoint: endpoint.clone(),
        self_contact: self_contact.clone(),
    };

    // Create discovery node with k=20, alpha=3
    let dht = DiscoveryNode::new(node_id, self_contact, network, 20, 3);

    // Register protocol handler
    let _router = Router::builder(endpoint.clone())
        .accept(DHT_ALPN, DhtProtocolHandler::new(dht.clone()))
        .spawn();

    // Use the DHT
    // dht.observe_contact(peer_contact).await;
    // let nodes = dht.iterative_find_node(target_id).await?;
    // let snapshot = dht.telemetry_snapshot().await;

    Ok(())
}
```

---

## Architecture

### Module Overview

| Module | Description |
|--------|-------------|
| `core` | Transport-agnostic DHT logic: routing table, storage, tiering, adaptive parameters |
| `net` | iroh-based `DhtNetwork` implementation using QUIC transport |
| `protocol` | RPC message definitions for `irpc` framework |
| `server` | Protocol handler for integrating with iroh's `Router` |

### Public API

```rust
// Types and helpers
pub use core::{
    derive_node_id,      // Hash bytes to NodeId using BLAKE3
    hash_content,        // Hash content to Key using BLAKE3
    verify_key_value_pair, // Verify key matches content hash
    Contact,             // Peer identity + address
    DhtNetwork,          // Network abstraction trait
    DiscoveryNode,       // Main DHT node handle
    Key,                 // 32-byte content key
    NodeId,              // 32-byte node identifier
    RoutingTable,        // Kademlia routing table
};

// Network and server
pub use net::{IrohNetwork, DHT_ALPN};
pub use server::DhtProtocolHandler;
```

---

## Core Concepts

### Node and Key Identity

The DHT uses 256-bit identifiers:

```rust
type NodeId = [u8; 32];  // Node identifier (BLAKE3 hash of public key)
type Key = [u8; 32];     // Content key (BLAKE3 hash of value)
```

### Routing Table

Kademlia-style routing with:
- **256 buckets** for 256-bit XOR distance space
- **Adaptive k** (10-30 contacts per bucket) based on churn rate
- **LRU-like eviction** preferring long-lived nodes
- **Ping-before-evict** rule for bucket maintenance

### Latency Tiering

Contacts are dynamically assigned to latency tiers:
- **K-means clustering** on RTT samples (2-5 tiers)
- **Periodic recomputation** every 30 seconds
- **Tier-aware lookups** prioritizing fast peers
- **Spill offloading** to slower tiers under pressure

### Backpressure & Storage

- **LRU cache** with O(1) operations (get, put, eviction)
- **Pressure monitoring** based on memory, disk, and request rate
- **Automatic eviction** when pressure exceeds threshold (0.8)
- **Content verification** using BLAKE3 hash

### Adaptive Parameters

| Parameter | Range | Adaptation |
|-----------|-------|------------|
| k (bucket size) | 10-30 | Increases with churn rate |
| α (parallelism) | 2-5 | Adjusts based on lookup success |

---

## Configuration Constants

| Constant | Default | Description |
|----------|---------|-------------|
| `K_DEFAULT` | 20 | Default bucket size |
| `ALPHA_DEFAULT` | 3 | Default lookup parallelism |
| `MIN_LATENCY_TIERS` | 2 | Minimum number of latency tiers |
| `MAX_LATENCY_TIERS` | 5 | Maximum number of latency tiers |
| `TIERING_RECOMPUTE_INTERVAL` | 30s | Tier recomputation frequency |
| `PRESSURE_THRESHOLD` | 0.8 | Eviction trigger threshold |
| `LOCAL_STORE_MAX_ENTRIES` | 100,000 | Maximum LRU cache entries |

---

## Network Protocol

### ALPN

```rust
pub const DHT_ALPN: &[u8] = b"myapp/dht/1";
```

### RPC Messages

| Request | Response | Description |
|---------|----------|-------------|
| `FindNodeRequest` | `Vec<Contact>` | Find k closest nodes to target |
| `FindValueRequest` | `FindValueResponse` | Get value or closer nodes |
| `StoreRequest` | `()` | Store key-value pair |
| `PingRequest` | `()` | Check node responsiveness |

---

## Telemetry

```rust
#[derive(Clone, Debug, Default)]
pub struct TelemetrySnapshot {
    pub tier_centroids: Vec<f32>,    // Latency tier centers (ms)
    pub tier_counts: Vec<usize>,     // Nodes per tier
    pub pressure: f32,               // Current pressure (0.0-1.0)
    pub stored_keys: usize,          // Keys in local storage
    pub replication_factor: usize,   // Current k
    pub concurrency: usize,          // Current alpha
}

// Get a snapshot
let snapshot = dht.telemetry_snapshot().await;
```

---

## Example Binary

The included example binary demonstrates a complete DHT node:

```bash
cargo run
```

This starts a node with:
- mDNS discovery for local network peers
- Relay fallback for NAT traversal
- Periodic telemetry logging (every 5 minutes)

Output:
```
DHT node started
  NodeId (hex): a1b2c3d4...
  Endpoint addr JSON: {"node_id":"...","direct_addresses":[...]}
mDNS discovery enabled; will fall back to relay if unavailable
Telemetry: pressure=0.00, stored_keys=0, tiers=[0], centroids=[150.0], k=20, alpha=3
```

---

## Testing

```bash
# Run all tests
cargo test

# Run with output
cargo test -- --nocapture

# Run specific test
cargo test iterative_find_node
```

---

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `iroh` | 0.95.1 | QUIC transport with discovery |
| `irpc` / `irpc-iroh` | 0.11 | RPC framework |
| `iroh-blake3` | 1.4 | BLAKE3 hashing |
| `tokio` | 1.x | Async runtime |
| `lru` | 0.12 | O(1) LRU cache |

---

## License

- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

---

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
