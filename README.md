# Iroh sDHT

A Kademlia‑ and Coral‑inspired, latency‑aware “sloppy DHT” (sDHT) with adaptive dynamic tiering and backpressure controls, built on [iroh](https://github.com/n0-computer/iroh)’s QUIC transport.

This crate aims to be a **practical, observable DHT core** for iroh‑based applications. The behavior described in this README is backed by concrete tests in the repository; where RFC 2119 keywords (**MUST**, **SHOULD**, **MAY**, etc.) are used, they refer to behavior that is either already enforced in tests or is a documented expectation for users.

---

## Overview

The core provides:

- **Kademlia‑style routing**  
  32‑byte node IDs and keys derived from BLAKE3, XOR distance, and bucketed routing tables.

- **Coral‑style sloppy DHT behavior**  
  Peers are grouped into **latency‑based tiers**, and lookups escalate from fast/near tiers to slower/further ones.

- **Adaptive dynamic tiering**  
  Tier boundaries are learned from observed RTTs using a bounded k‑means variant. The number of latency tiers is chosen dynamically within configured limits.

- **Backpressure‑aware storage**  
  A local key/value store with soft limits on memory/disk and request rate, exposing a `pressure` signal and evicting or spilling under load.

- **Adaptive parameters**  
  Replication factor (`k`) and query concurrency (`α`) adjust based on churn and lookup success/failure.

- **Iroh transport integration**  
  A ready‑to‑use `IrohNetwork` implementation running over iroh’s QUIC `Endpoint`/`EndpointAddr`.

---

## Implemented behaviors and scenarios

This section describes the behaviors that are backed by the current test suite and example binary. Each subsection references the relevant test(s) or example file(s).

### 1. Routing table behavior (`RoutingTable`)

Tests: [`tests/routing_table.rs`](tests/routing_table.rs)

#### 1.1 Ordering by XOR distance

Given:

- A `RoutingTable` with a self node ID (e.g. `0x00` in the first byte).
- A set of contacts inserted via `RoutingTable::update`.

The following **MUST** hold:

- `RoutingTable::closest(target, limit)` **MUST** return contacts sorted by **increasing XOR distance** to the `target`.
- For the specific case in `routing_table_orders_contacts_by_distance`:
  - With contacts `0x10`, `0x20`, and `0x08` inserted,
  - And target `0x18`,
  - `closest(target, 3)` **MUST** return contacts whose first byte equals `[0x10, 0x08, 0x20]`.

#### 1.2 Bucket capacity

Given:

- A `RoutingTable` created with `RoutingTable::new(self_id, k)`.

The following **MUST** hold:

- The table **MUST** enforce a capacity of at most `k` contacts per bucket.
- In `routing_table_respects_bucket_capacity`:
  - Creating a table with `k = 2`,
  - Inserting three contacts (`0x80`, `0xC0`, `0xA0`),
  - Calling `closest(target, 10)` **MUST** return exactly 2 contacts.
  - The two returned contacts **MUST** include `0x80` and `0xC0`.

#### 1.3 Dynamic `k` (`set_k`)

Given:

- A `RoutingTable` created with some initial `k_initial` and then updated to `k_new` using `set_k(k_new)`.

The following **MUST** hold:

- After calling `set_k(k_new)`, calls to `closest` **MUST** return at most `k_new` contacts per bucket, even if more contacts were previously stored.
- In `routing_table_truncates_when_k_changes`:
  - With `k_initial = 4` and 3 contacts inserted,
  - After `set_k(2)`,
  - `closest(target, 10)` **MUST** return exactly 2 contacts.

---

### 2. Iterative FIND_NODE behavior and routing quality

Tests: [`tests/integration.rs`](tests/integration.rs), [`tests/iterative_find_node_scale.rs`](tests/iterative_find_node_scale.rs)

#### 2.1 Small‑scale correctness

Test: `iterative_find_node_returns_expected_contacts` in [`tests/integration.rs`](tests/integration.rs)

Given:

- Three nodes: `main`, `peer_one`, `peer_two`.
- `main` observes `peer_one` and `peer_two` via `observe_contact`.
- `peer_one` and `peer_two` observe `main`.

The following **MUST** hold:

- A call to `main.node.iterative_find_node(peer_two.id)` **MUST**:
  - Return a result list whose first element’s `id` equals `peer_two.id`.
  - Return a result list that **SHOULD** contain a contact whose `id` equals `peer_one.id`.

This scenario establishes that, for a simple topology, `iterative_find_node` returns the exact target at the front and preserves useful intermediate contacts.

#### 2.2 Large‑scale routing quality and overlap

Test: `iterative_find_node_quality_report` in [`tests/iterative_find_node_scale.rs`](tests/iterative_find_node_scale.rs)

Given:

- A swarm of **1024 nodes** constructed via `TestNode::new`, sharing a `NetworkRegistry`.
- Each node’s routing table populated with **all other nodes** using `observe_contact`.
- `K_PARAM = 20`, `TARGET_SAMPLES = 512`, `ORIGINS_PER_TARGET = 10`.

The test:

1. **MUST** compute, for each random target `NodeId`, the **perfect set** of `k` closest node IDs by brute‑force XOR distance over all node IDs.
2. **MUST** select multiple origin nodes per target.
3. For each (origin, target) pair, **MUST**:
   - Run `iterative_find_node(target)`.
   - Collect the returned contact IDs.
   - Compute the **overlap** between the perfect set and the returned set.
   - Compute `overlap_fraction = (#perfect ∩ result) / k`.
   - Track whether the single closest perfect node is present in the result (`closest_present`).

The test then:

- **MUST** compute:
  - `mean_overlap_fraction` across all samples.
  - `median_overlap_fraction` across all samples.
  - A histogram (`HISTOGRAM_BUCKETS = 10`) of overlap fractions over `[0.0, 1.0]`.
- **MUST** print an aggregate report as **pretty‑printed JSON** with the fields:
  - `node_count`
  - `target_samples`
  - `origins_per_target`
  - `routing_table_population`
  - `mean_overlap_fraction`
  - `median_overlap_fraction`
  - `histogram` (each bucket includes `bucket_start`, `bucket_end`, `count`)
  - `sample_count`
- **MUST** print CSV data with:
  - Header line:  
    `origin_index,target_index,overlap_fraction,closest_present`
  - One line per (origin, target) query.

Finally, the test asserts that:

- **For all samples**, the single closest node from the perfect set **MUST** be present in the results:
  - `samples.iter().all(|row| row.closest_present)` **MUST** hold.

This gives you a spec‑like guarantee: with fully populated routing tables, `iterative_find_node` **MUST** always return the closest possible node, and the rest of the result set is quantitatively close to the ideal set.

---

### 3. Data distribution and replication

Test: `data_distribution_is_relatively_even` in [`tests/data_distribution.rs`](tests/data_distribution.rs)

Given:

- A swarm of **256 nodes** with:
  - `K_PARAM = 20`, `ALPHA_PARAM = 3`.
- A ring‑plus‑random adjacency:
  - Each node has at least `MIN_CONTACTS_PER_NODE = 24` neighbors.
- Routing tables initially populated with ring neighbors, then fully populated with all contacts.
- Per‑node pressure limits relaxed via `override_pressure_limits` to avoid premature backpressure during this test.
- `TOTAL_PUTS = 2048` with `PAYLOAD_LEN = 64`.

The test:

1. **MUST** issue `TOTAL_PUTS` `put` operations:
   - Each originating from a random node.
   - Each with random payload (so keys are random BLAKE3 hashes).
2. After all PUTs, it **MUST**:
   - Call `telemetry_snapshot()` on every node.
   - Collect `(node_index, snapshot.stored_keys)` pairs.

From this data, the test:

- **MUST** compute:
  - Total number of stored keys.
  - Per‑node minimum, maximum, and mean stored key counts.
  - Standard deviation of per‑node stored key counts.
  - Coefficient of variation = `stddev / mean`.
- **MUST** print:
  - CSV header: `node_index,stored_keys`
  - One line per node with index and key count.
  - Summary line:  
    `summary,min,max,mean,stddev,total_keys`
  - Coefficient of variation line:  
    `summary_cv,<coefficient_of_variation>`

The test asserts that:

- Every node **MUST** store at least one key: `min > 0`.
- The exact threshold for “even distribution” is not enforced as an assertion, but the coefficient of variation **SHOULD** be reasonably small in practice.

This scenario gives you:

- A **MUST** guarantee that no node is completely idle after a large number of random PUTs.
- A **SHOULD** level expectation that keys are roughly evenly replicated across the swarm, visible via the printed metrics.

---

### 4. Adaptive parameters and backpressure

Tests: `adaptive_k_tracks_network_successes_and_failures`, `backpressure_spills_large_values_and_records_pressure` in [`tests/integration.rs`](tests/integration.rs)

#### 4.1 Adaptive replication factor under failures

Given:

- Two nodes `main` and `peer`, both part of a mocked network (`NetworkRegistry`).
- An initial configuration with `k = 10` (as used in the test).
- `main` and `peer` observing each other via `observe_contact`.

The test:

1. **MUST** configure the network so that all RPCs to `peer` **fail**, via `main.network.set_failure(peer.id, true)`.
2. **MUST** run `main.node.iterative_find_node(target)` under these failure conditions:
   - The lookup **MUST** succeed or at least **MUST NOT** panic.
3. After this, a `telemetry_snapshot()` from `main`:
   - **MUST** report `replication_factor == 30`.

Then:

4. **MUST** configure the network so that RPCs to `peer` succeed again via `set_failure(peer.id, false)`.
5. **MUST** run `iterative_find_node(target)` again.
6. A new `telemetry_snapshot()` from `main`:
   - **MUST** report `replication_factor == 20`.

This scenario demonstrates that:

- Under persistent failures, the node **MUST** raise its effective replication factor.
- After successful operations, it **MUST** reduce `replication_factor` back toward a lower baseline.

#### 4.2 Backpressure and spilling large values

Given:

- A node `node` with `k = 20`, `α = 3`.
- A `peer` contact.
- A large `value` (~12 MiB) and `key = hash_content(value)`.

The test:

1. **MUST** call `node.node.handle_store_request(&peer, key, value)` directly.
2. **MUST** then call `telemetry_snapshot()` on the node.
3. The snapshot:
   - **MUST** report `pressure >= 0.99`.
   - **MUST** report `stored_keys == 0` (the large value is not kept locally).
4. The test **MUST** then inspect `node.network.store_calls()`:
   - There **MUST** be at least one store call recorded.
   - The first store call:
     - `contact.id` **MUST** equal `peer.id`.
     - The stored key **MUST** equal `key`.
     - The stored length **MUST** equal `value.len()`.

This scenario establishes that:

- Under heavy load from large values, the node **MUST** signal high `pressure`.
- It **MUST** spill or forward such values instead of storing them locally.
- The forwarding behavior **MUST** be observable via the mock network’s recorded calls.

---

### 5. Latency‑based tiering

Test: `tiering_clusters_contacts_by_latency` in [`tests/integration.rs`](tests/integration.rs)

Given:

- A node `main`.
- Three peers `fast`, `medium`, `slow`.
- `main` and all peers observe each other via `observe_contact`.
- The mocked network configured with:
  - `fast`: 5 ms latency.
  - `medium`: 25 ms latency.
  - `slow`: 50 ms latency.

The test:

1. **MUST** run `main.node.iterative_find_node(target)`.
2. **MUST** then call `telemetry_snapshot()` on `main`.

The snapshot:

- **MUST** report `tier_centroids.len() >= 2`.
- The sum of `tier_counts` **MUST** equal 3 (all peers assigned to tiers).
- `tier_centroids.first().unwrap()` **MUST** be less than `tier_centroids.last().unwrap()`, reflecting increasing latency from fastest to slowest tier.

This gives a concrete guarantee that:

- The implementation **MUST** cluster peers by observed latency into tiers.
- The tiering information **MUST** be visible via telemetry.

---

### 6. Chatroom example: DHT used for peer discovery

Example: [`examples/chatroom.rs`](examples/chatroom.rs)

The chatroom example is an application that uses `iroh-sdht` for **peer discovery only**; chat messages themselves are sent directly over QUIC.

#### 6.1 Setup

The example:

- **MUST** run an iroh `Endpoint` configured with:
  - `DHT_ALPN` for DHT traffic.
  - `CHAT_ALPN` (`b"iroh-chatroom/1"`) for chat messages.
- **MUST** derive a DHT `node_id` from the iroh endpoint identity via `derive_node_id`.
- **MUST** build a `Contact` using:
  - `id = node_id`.
  - `addr` as a JSON‑encoded `EndpointAddr`.
- **MUST** wrap the endpoint and contact into an `IrohNetwork`.
- **MUST** construct a `DiscoveryNode` using that network.

The chatroom **MUST**:

- Register `DhtProtocolHandler` on `DHT_ALPN`.
- Register `ChatProtocolHandler` on `CHAT_ALPN`.

#### 6.2 Discovery behavior

When sending a chat message to a room:

1. The example **MUST** derive a “room discovery key”:
   - Via `room_discovery_key(room)` which uses `hash_content(room.as_bytes())`.
2. It **MUST** call `dht.iterative_find_node(discovery_key)` to retrieve DHT contacts.
3. It **MUST** merge:
   - Static peers, provided via CLI `--peer` (as JSON `EndpointAddr`) and `/add`.
   - Dynamic peers, interpreting each `Contact.addr` as JSON `EndpointAddr`.
4. It **MUST** avoid duplicates when merging.

This ensures that the DHT is used to discover peers that “belong” to a room, but does not handle the chat payload itself.

#### 6.3 Chat transport behavior

For each peer `EndpointAddr`:

- The example **MUST** open a QUIC connection using `CHAT_ALPN`.
- It **MUST** send a single length‑prefixed JSON `ChatMessage`:
  - 4‑byte big‑endian length.
  - JSON payload.
- It **MAY** read a small ACK frame (the example does, but ignores the body).
- It **MUST NOT** store chat messages in the DHT:
  - Chat messages are never passed to any `put`/`store` DHT APIs.
  - DHT is strictly used as a discovery mechanism.

#### 6.4 REPL behavior

The chatroom REPL:

- **MUST** support `/add <addr-json>`:
  - Parses JSON into `EndpointAddr`.
  - On success, appends it to the peer list and prints confirmation.
  - On error, prints a descriptive message.
- **MUST** support `/peers`:
  - Prints the current static peer list (CLI + `/add`).
  - Dynamic peers from the DHT are not persisted but are used for each send.
- **MUST** support `/quit`:
  - Exits the application.
- Any other non‑empty line **MUST** be interpreted as a chat message:
  - It is sent to all known peers in the room (static + DHT‑discovered), as described above.

---

## Telemetry API

You can inspect the node’s internal behavior via:

```rust
let snapshot = dht.telemetry_snapshot().await;
```

`TelemetrySnapshot` includes:

- `tier_centroids: Vec<f32>` – latency tier centers in ms (fast → slow).
- `tier_counts: Vec<usize>` – number of peers in each tier.
- `pressure: f32` – backpressure signal in `[0.0, 1.0]`.
- `stored_keys: usize` – number of keys in the local store.
- `replication_factor: usize` – current effective `k`.
- `concurrency: usize` – current effective `alpha`.

The tests above define how these values **MUST** behave under specific scenarios. You **SHOULD** export them into your metrics / logging system to monitor:

- Routing quality.
- Data distribution.
- Adaptive parameter behavior.
- Tiering behavior under varying latency.

---

## Minimal integration example

To embed `iroh-sdht` in your own application, a typical setup is:

```toml
[dependencies]
iroh-sdht = "0.x"                  # this crate
iroh = "0.x"                       # for Endpoint / QUIC transport
tokio = { version = "1", features = ["full"] }
anyhow = "1"
```

```rust
use anyhow::Result;
use iroh::{Endpoint, EndpointAddr};
use iroh::protocol::Router;
use iroh_sdht::{
    derive_node_id, Contact, DiscoveryNode, DhtProtocolHandler, IrohNetwork, DHT_ALPN,
};

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Build an iroh Endpoint. See iroh docs for full configuration.
    let (endpoint, addr): (Endpoint, EndpointAddr) = /* construct or obtain an Endpoint */ {
        unimplemented!("set up your iroh Endpoint here");
    };

    // 2. Derive a stable DHT node ID from the iroh endpoint identity.
    let node_id = derive_node_id(endpoint.id().as_bytes());

    // 3. Build our Contact. We store EndpointAddr as JSON in `addr`.
    let self_contact = Contact {
        id: node_id,
        addr: serde_json::to_string(&addr)?,
    };

    // 4. Wrap the endpoint in an IrohNetwork.
    let network = IrohNetwork {
        endpoint,
        self_contact: self_contact.clone(),
    };

    // 5. Create a discovery-only DHT node (k = 20, alpha = 3 are reasonable starting values).
    let dht = DiscoveryNode::new(node_id, self_contact, network, /*k=*/ 20, /*alpha=*/ 3);

    // 6. Spin up the Router accept loop.
    let _router = Router::builder(endpoint.clone())
        .accept(DHT_ALPN, DhtProtocolHandler::new(dht.clone()))
        .spawn();

    // 7. Observe peers and issue iterative FIND_NODE or FIND_VALUE lookups via `dht`.

    Ok(())
}
```

---

## When to use this vs. “plain” Kademlia

Use this library if you:

- Already use iroh (or want a QUIC‑based, NAT‑friendly transport).
- Care about **latency‑aware routing**, not just hop count.
- Want **bounded**, **adaptive** behavior:
  - Dynamic tiering based on RTTs.
  - Adaptive `k`/`α`.
  - Backpressure and eviction/spilling instead of unbounded growth.

If you just need a minimal, spec‑like Kademlia for an academic project, a simpler Kademlia crate may be enough. This crate targets “small but realistic” DHT deployments.

---

## License

See [LICENSE](LICENSE).
