# Iroh sDHT

A Kademlia‑ and Coral‑inspired, latency‑aware “sloppy DHT” (sDHT) with adaptive dynamic tiering and backpressure controls, built on [iroh](https://github.com/n0-computer/iroh)’s QUIC transport.

---

## Overview

This crate provides a small, self‑contained distributed hash table that combines:

- **Kademlia‑style routing**  
  32‑byte node IDs and keys derived from BLAKE3, XOR distance, and bucketed routing tables.

- **Coral‑style sloppy DHT behavior**  
  Peers are grouped into **latency‑based tiers**, and lookups escalate from fast/near tiers to slower/further ones.

- **Adaptive dynamic tiering**  
  Tier boundaries are learned from observed RTTs using a bounded k‑means variant. The number of latency tiers is chosen dynamically within configured limits.

- **Backpressure‑aware storage**  
  A local key/value store with soft limits on memory/disk and request rate, exposing a `pressure` signal and evicting under load.

- **Adaptive parameters**  
  Replication factor (`k`) and query concurrency (`α`) adjust based on churn and lookup success.

- **Iroh transport integration**  
  A ready‑to‑use `IrohNetwork` implementation running over iroh’s QUIC `MagicEndpoint`, with relay and mDNS discovery support.

This is intended as a **practical, observable DHT core** you can embed into iroh‑based applications, not just an academic toy.

---

## Features

- **Kademlia‑inspired DHT**
  - 32‑byte node IDs and keys derived from BLAKE3.
  - XOR distance and bucketed routing table.
  - Iterative `FIND_NODE` and `FIND_VALUE` lookups.

- **Latency‑aware, sloppy sDHT behavior**
  - Per‑peer RTT sampling.
  - Dynamic latency tiers (fast → slow).
  - Query escalation across tiers based on miss rate.

- **Adaptive tiering**
  - Bounded number of tiers (e.g. 1–6).
  - Periodic recomputation via k‑means with a complexity penalty.
  - Telemetry exposing tier centroids and counts.

- **Resource‑aware local store**
  - LRU‑like eviction under pressure.
  - Soft limits for disk/memory and request rate.
  - Telemetry `pressure` output.

- **Adaptive K/α**
  - `k` (replication factor) responds to churn.
  - `α` (parallelism) responds to lookup success.

- **Transport‑agnostic core, plus**
  - `DhtNetwork` trait for custom transports.
  - `IrohNetwork` implementation using `MagicEndpoint`.

---

## Status

This library is **production‑leaning**:

- Core algorithms are bounded and observable.
- Latency tiering, backpressure, and adaptive parameters are designed with real‑world constraints in mind.
- It is suitable for prototypes, internal services, and experimentation with iroh‑based P2P systems.

If you plan a hostile, internet‑wide deployment, you will still want:

- More testing and fuzzing under churn and adversarial conditions.
- A security and abuse‑resistance pass.
- Integration with your metrics/logging stack.

---

## Getting Started

Add the dependencies:

```toml
[dependencies]
iroh-sdht = "0.x"                  # this crate
iroh = "0.x"                      # for MagicEndpoint / transport
tokio = { version = "1", features = ["full"] }
anyhow = "1"
```

### Minimal example (outline)

A typical setup looks like:

1. Create an iroh `MagicEndpoint`.
2. Wrap it in `IrohNetwork`.
3. Construct a `DhtNode`.
4. Run the server loop to handle inbound RPC.
5. Use the node to `put` / `get` keys.

```rust
use iroh::net::{MagicEndpoint, NodeAddr};
use iroh_sdht::{DhtNode, IrohNetwork, Contact, DHT_ALPN};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Build an iroh MagicEndpoint
    let (endpoint, local_addr) = MagicEndpoint::builder()
        .alpns(vec![DHT_ALPN.to_vec()])
        .bind(0)
        .await?;

    // 2. Wrap the endpoint in an IrohNetwork
    let network = IrohNetwork::new(endpoint);

    // 3. Create a DHT node
    let node_id = iroh_sdht::random_node_id();
    let self_contact = Contact::new(node_id, NodeAddr::from_socket_addr(local_addr));
    let dht = DhtNode::new(node_id, self_contact, network, /*k=*/ 20, /*alpha=*/ 3);

    // 4. TODO: run server loop for incoming connections (see examples)
    // 5. TODO: perform puts/gets

    Ok(())
}
```

For a complete runnable example with server loop and telemetry, see the example binary in this repository.

---

## Telemetry

You can inspect the node’s internal behavior via:

```rust
let snapshot = dht.telemetry_snapshot().await;
```

`TelemetrySnapshot` includes:

- `tier_centroids: Vec<f32>` – latency tier centers in ms (fast → slow).
- `tier_counts: Vec<usize>` – number of peers in each tier.
- `pressure: f32` – backpressure signal in `[0.0, 1.0]`.
- `stored_keys: usize` – number of keys in the local store.
- `replication_factor: usize` – current `k`.
- `concurrency: usize` – current `alpha`.

This is intended to be wired into your logging/metrics system so you can tune tiering and resource limits in real deployments.

---

## When to use this vs. “plain” Kademlia

Use this library if you:

- Already use iroh (or want a QUIC‑based, NAT‑friendly transport).
- Care about **latency‑aware routing**, not just hop count.
- Want **bounded**, **adaptive** behavior:
  - dynamic tiering based on RTTs,
  - adaptive `k`/`α`,
  - backpressure and eviction instead of unbounded growth.

If you just need a minimal, spec‑like Kademlia for an academic project, a simpler Kademlia crate may be enough. This crate targets “small but realistic” DHT deployments.

---

## License

Apache‑2.0
