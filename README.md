# Iroh sDHT

A Kademlia‑ and Coral‑inspired, latency‑aware “sloppy DHT” (sDHT) with adaptive dynamic tiering and backpressure controls, built on [iroh](https://github.com/n0-computer/iroh)’s QUIC transport.

This crate exposes a small, embeddable DHT that you can plug into iroh‑based applications. It is transport‑agnostic at the core and ships with an iroh‑based network implementation and server glue.

---

## Crate layout

The library is organized into four main modules:

- [`core`] – transport‑agnostic DHT logic:
  - Node and key types (`NodeId`, `Key`).
  - Hashing helpers (`derive_node_id`, `hash_content`, `verify_key_value_pair`).
  - Routing table (`RoutingTable`) and distance helpers (`xor_distance`).
  - Local storage and backpressure.
  - Adaptive parameters (`k`, `α`) and latency tiering.
  - The main DHT state machine (`DhtNode`) and its discovery‑oriented wrapper (`DiscoveryNode`).
  - Network abstraction trait (`DhtNetwork`).
- [`net`] – an iroh‑based implementation of `DhtNetwork` (`IrohNetwork`) plus the DHT ALPN label (`DHT_ALPN`).
- [`protocol`] – RPC request/response types and the `DhtProtocol` definition for `irpc`.
- [`server`] – a `DhtProtocolHandler` that plugs into iroh’s `Router` to serve DHT RPCs over QUIC.

The public re‑exports from `lib.rs` are:

- Types and helpers from `core`:

  ```rust
  pub use core::{
      derive_node_id,
      hash_content,
      verify_key_value_pair,
      Contact,
      DhtNetwork,
      DiscoveryNode,
      Key,
      NodeId,
      RoutingTable,
  };
  ```

- Network and server glue:

  ```rust
  pub use net::{IrohNetwork, DHT_ALPN};
  pub use server::DhtProtocolHandler;
  ```

- Convenience re‑exports:

  ```rust
  pub use irpc;
  pub use irpc_iroh;
  ```

---

## Core concepts

### Node and key identity

The DHT uses 256‑bit identifiers for both nodes and content keys:

- `NodeId` – `type NodeId = [u8; 32];`
- `Key` – `type Key = [u8; 32];`

Helpers:

- `derive_node_id(data: &[u8]) -> NodeId`  
  Hashes arbitrary input with BLAKE3 and returns a 32‑byte digest, suitable for deriving a stable node ID from an iroh endpoint identity (`endpoint.id().as_bytes()`).

- `hash_content(data: &[u8]) -> Key`  
  Returns a 32‑byte BLAKE3 digest of the content bytes. This is used as the content‑addressed key for `put`/`get`.

- `verify_key_value_pair(key: &Key, value: &[u8]) -> bool`  
  Returns `true` iff `key == hash_content(value)`.

These helpers are used throughout the crate to ensure consistent hashing and integrity.

### Distance and routing

The DHT uses XOR distance over `NodeId`:

- `xor_distance(a: &NodeId, b: &NodeId) -> [u8; 32]`  
  Computes `a ^ b` byte‑wise.

- Internal helper `distance_cmp(&[u8; 32], &[u8; 32])`  
  Compares distances lexicographically to define an ordering.

Routing is based on a Kademlia‑style routing table:

```rust
pub struct Contact {
    pub id: NodeId,
    pub addr: String, // typically JSON-encoded EndpointAddr
}

pub struct RoutingTable {
    self_id: NodeId,
    k: usize,
    buckets: Vec<Bucket>, // 256 buckets for 256-bit IDs
}
```

**Key behaviors:**

- `RoutingTable::new(self_id, k)`:
  - Initializes 256 buckets.
- `RoutingTable::update(contact)`:
  - Computes the bucket index based on the first differing bit between `self_id` and `contact.id`.
  - Adds or refreshes the contact in an LRU‑like bucket (`Bucket::touch`), up to capacity `k`.
- `RoutingTable::set_k(k)`:
  - Updates the table’s `k` and truncates any buckets that currently hold more than `k` contacts.
- `RoutingTable::closest(target, k)`:
  - Flattens all buckets, sorts all contacts by XOR distance to `target`, and returns up to `k` contacts.

The bucket index is computed via:

```rust
fn bucket_index(self_id: &NodeId, other: &NodeId) -> usize {
    // XOR, then find the index (0..=255) of the first differing bit.
}
```

### Local store and backpressure

The crate includes a small in‑memory content store for values keyed by `Key`:

```rust
struct LocalStore {
    entries: HashMap<Key, Vec<u8>>,
    order: VecDeque<Key>,        // LRU-ish eviction order
    pressure: PressureMonitor,
}
```

- `LocalStore::store(key, value)`:
  - Replaces any existing value, updates internal byte accounting, and enqueues the key in `order`.
  - If the computed pressure exceeds `PRESSURE_THRESHOLD`, it evicts entries from the front of `order` (oldest first), accumulating them in a `spilled` list.
  - Marks that a spill happened if any keys were evicted.
- `LocalStore::get(key)`:
  - Returns a cloned value if present, and moves the key to the back of `order` (recently used).
- `LocalStore::current_pressure()` / `LocalStore::len()`:
  - Expose current pressure and stored key count.

Backpressure is tracked by a `PressureMonitor`:

```rust
struct PressureMonitor {
    current_bytes: usize,
    requests: VecDeque<Instant>,
    request_window: Duration,
    request_limit: usize,
    disk_limit: usize,
    memory_limit: usize,
    current_pressure: f32,
}
```

It combines:

- Approximate disk/memory usage (`current_bytes` vs soft limits).
- Request rate (count of recent store/get requests within `PRESSURE_REQUEST_WINDOW`).
- Into a normalized `current_pressure` in `[0.0, 1.0]`.

If entries are evicted from the store due to pressure, `record_spill()` is called so the pressure signal reflects the spill event.

Applications and tests can relax or override the soft limits using `DiscoveryNode::override_pressure_limits`.

### Latency tiering

Latency tiering assigns contacts to dynamic latency tiers based on observed RTTs.

Key structures:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TieringLevel(usize);

#[derive(Clone, Debug, Default)]
pub struct TieringStats {
    pub centroids: Vec<f32>, // ms (fastest → slowest)
    pub counts: Vec<usize>,  // number of peers per tier
}

struct TieringManager {
    assignments: HashMap<NodeId, TieringLevel>,
    samples: HashMap<NodeId, VecDeque<f32>>,
    centroids: Vec<f32>,
    last_recompute: Instant,
    min_tiers: usize,
    max_tiers: usize,
}
```

Key behaviors:

- `register_contact(node)`:
  - Ensures a contact has an assigned `TieringLevel` (defaulting to a middle tier).
- `record_sample(node, rtt_ms)`:
  - Records a bounded history of RTT samples per node and triggers a periodic recomputation.
- `stats()`:
  - Returns `TieringStats` with centroids and counts.
- Recomputations run a bounded k‑means variant (`dynamic_kmeans`) over per‑node average RTTs:
  - Chooses `k` between `MIN_LATENCY_TIERS` and `MAX_LATENCY_TIERS`.
  - Applies a penalty factor (`TIERING_PENALTY_FACTOR`) to avoid over‑fragmentation.
  - Ensures no empty tiers (`ensure_tier_coverage`).
  - Sorts centroids from fastest to slowest and remaps assignments accordingly.

Latency levels are used by the DHT node to:

- Filter candidate contacts for tier‑specific lookups (e.g., fast tiers first).
- Select a “slowest” tier for offloading spilled values.

### Adaptive parameters (`k` and `α`)

Adaptive behavior is encapsulated in `AdaptiveParams`:

```rust
struct AdaptiveParams {
    k: usize,
    alpha: usize,
    churn_history: VecDeque<bool>,
    query_history: VecDeque<bool>,
}
```

- `record_churn(success: bool)`:
  - Tracks success/failure of operations relevant to churn (e.g., replications, RPCs).
  - Uses a sliding window of size `QUERY_STATS_WINDOW`.
  - Computes a churn rate and adjusts `k` in a bounded range `[10, 30]`.
- `record_query_success(success: bool)`:
  - Tracks lookup successes/failures.
  - Adjusts `alpha` in `{2, 3, 4, 5}` based on the success rate:
    - Very low success → higher `α`.
    - High success → lower `α`.

`DhtNode` uses these to:

- Adapt the routing table’s effective `k` (`RoutingTable::set_k`) when churn changes.
- Adjust lookup concurrency during iterative searches and GETs.

### Telemetry

The crate exposes a compact telemetry struct:

```rust
#[derive(Clone, Debug, Default)]
pub struct TelemetrySnapshot {
    pub tier_centroids: Vec<f32>,
    pub tier_counts: Vec<usize>,
    pub pressure: f32,
    pub stored_keys: usize,
    pub replication_factor: usize,
    pub concurrency: usize,
}
```

- `tier_centroids` / `tier_counts` come from the `TieringManager`.
- `pressure` and `stored_keys` come from the `LocalStore`.
- `replication_factor` / `concurrency` mirror the current `k` and `α` from `AdaptiveParams`.

You can obtain a snapshot via:

```rust
let snapshot = dht.telemetry_snapshot().await;
```

The example binary (`src/main.rs`) logs this periodically.

---

## DhtNetwork abstraction

The DHT core is independent of the transport layer. It talks to peers via a `DhtNetwork` trait:

```rust
#[async_trait]
pub trait DhtNetwork: Send + Sync + 'static {
    async fn find_node(&self, to: &Contact, target: NodeId) -> Result<Vec<Contact>>;

    /// Return (value, closer_nodes).
    async fn find_value(&self, to: &Contact, key: Key)
        -> Result<(Option<Vec<u8>>, Vec<Contact>)>;

    async fn store(&self, to: &Contact, key: Key, value: Vec<u8>) -> Result<()>;
}
```

Implementors are responsible for:

- Encoding/decoding RPC messages.
- Establishing connections to `Contact.addr`.
- Mapping transport errors into `anyhow::Result`.

The crate provides one implementation:

### IrohNetwork (QUIC over iroh)

```rust
pub struct IrohNetwork {
    pub endpoint: Endpoint,
    pub self_contact: Contact,
}

pub const DHT_ALPN: &[u8] = b"myapp/dht/1";
```

`IrohNetwork`:

- Encodes `Contact.addr` as JSON `EndpointAddr`.
- Builds an `irpc` client bound to `DHT_ALPN`.
- Implements `DhtNetwork` by invoking RPCs defined in `protocol`:

  - `FindNodeRequest` → `Vec<Contact>`
  - `FindValueRequest` → `FindValueResponse { value, closer }`
  - `StoreRequest` → `()`

This lets you run the DHT over iroh’s QUIC transport with minimal glue.

---

## DhtProtocol and server integration

The `protocol` module defines the RPC payloads:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PingRequest { pub from: Contact }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FindNodeRequest { pub from: Contact, pub target: NodeId }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FindValueRequest { pub from: Contact, pub key: Key }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreRequest { pub from: Contact, pub key: Key, pub value: Vec<u8> }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FindValueResponse {
    pub value: Option<Vec<u8>>,
    pub closer: Vec<Contact>,
}
```

And an `irpc` RPC enum:

```rust
#[rpc_requests(message = DhtMessage)]
#[derive(Debug, Serialize, Deserialize)]
pub enum DhtProtocol {
    #[rpc(tx = oneshot::Sender<Vec<Contact>>)]
    FindNode(FindNodeRequest),

    #[rpc(tx = oneshot::Sender<FindValueResponse>)]
    FindValue(FindValueRequest),

    #[rpc(tx = oneshot::Sender<()>>)]
    Store(StoreRequest),

    #[rpc(tx = oneshot::Sender<()>>)]
    Ping(PingRequest),
}
```

The `server` module wires this into iroh’s `Router`:

```rust
#[derive(Clone)]
pub struct DhtProtocolHandler {
    inner: Arc<IrohProtocol<DhtProtocol>>,
}
```

- `DhtProtocolHandler::new(node: DiscoveryNode<N>)`:
  - Spawns a small server loop that receives `DhtMessage` instances from `irpc_iroh`.
  - Dispatches them to the appropriate handlers:
    - `handle_find_node` → `DiscoveryNode::handle_find_node_request`
    - `handle_find_value` → `DiscoveryNode::handle_find_value_request`
    - `handle_store` → `DiscoveryNode::handle_store_request`
    - `handle_ping` → sends an empty `()` response.

- Implements iroh’s `ProtocolHandler`:
  - `accept(connection)` forwards to `IrohProtocol<DhtProtocol>::accept(connection)`.

### Using DhtProtocolHandler with iroh

You typically wire the server side like this:

```rust
use iroh::protocol::Router;
use iroh_sdht::{DhtProtocolHandler, DiscoveryNode, IrohNetwork, DHT_ALPN, Contact, derive_node_id};

let endpoint = /* build an iroh Endpoint */;
let addr: EndpointAddr = endpoint.addr();
let node_id = derive_node_id(endpoint.id().as_bytes());
let self_contact = Contact {
    id: node_id,
    addr: serde_json::to_string(&addr)?,
};
let network = IrohNetwork { endpoint: endpoint.clone(), self_contact };
let dht = DiscoveryNode::new(node_id, self_contact, network, /*k*/ 20, /*alpha*/ 3);

let _router = Router::builder(endpoint.clone())
    .accept(DHT_ALPN, DhtProtocolHandler::new(dht.clone()))
    .spawn();
```

---

## DhtNode and DiscoveryNode

### DhtNode

`DhtNode<N: DhtNetwork>` is the internal state machine that owns:

- `id: NodeId` – the node’s identifier.
- `self_contact: Contact` – how to reach this node.
- `routing: Arc<Mutex<RoutingTable>>` – routing table.
- `store: Arc<Mutex<LocalStore>>` – local content store.
- `network: Arc<N>` – implementation of `DhtNetwork`.
- `params: Arc<Mutex<AdaptiveParams>>` – adaptive `k`, `α`.
- `tiering: Arc<Mutex<TieringManager>>` – latency tier management.
- `escalation: Arc<Mutex<QueryEscalation>>` – tier‑specific miss history.

Key methods (internal; accessed via `DiscoveryNode`):

- `observe_contact(contact: Contact)` – incorporate new peers into tiering and routing.
- `handle_find_node_request(from, target) -> Vec<Contact>` – serve `FIND_NODE`.
- `handle_find_value_request(from, key) -> (Option<Vec<u8>>, Vec<Contact>)` – serve `FIND_VALUE`.
- `handle_store_request(from, key, value)` – store or spill a value.
- `iterative_find_node(target) -> Result<Vec<Contact>>` – perform iterative node lookup.
- `iterative_find_value(key) -> Result<(Option<Vec<u8>>, Vec<Contact>)>` – perform iterative value lookup.
- `put(value) -> Result<Key>` – local+remote replication based on distance.
- `get(key) -> Result<Option<Vec<u8>>>` – GET with integrity, tier escalation, and re‑replication.
- `telemetry_snapshot() -> TelemetrySnapshot` – capture internal metrics.

`DhtNode` is not exported directly; instead, the crate exposes `DiscoveryNode` which wraps these capabilities in a more constrained API.

### DiscoveryNode

`DiscoveryNode<N: DhtNetwork>` is the public, clonable handle you use in applications:

```rust
pub struct DiscoveryNode<N: DhtNetwork> {
    inner: Arc<DhtNode<N>>,
}
```

Public methods:

- Construction and identity:

  ```rust
  pub fn new(id: NodeId, self_contact: Contact, network: N, k: usize, alpha: usize) -> Self;
  pub fn contact(&self) -> Contact;
  pub fn node_id(&self) -> NodeId;
  ```

- Routing and discovery:

  ```rust
  pub async fn observe_contact(&self, contact: Contact);
  pub async fn iterative_find_node(&self, target: NodeId) -> Result<Vec<Contact>>;
  ```

- Telemetry:

  ```rust
  pub async fn telemetry_snapshot(&self) -> TelemetrySnapshot;
  ```

- RPC entry points (used by the server side):

  ```rust
  pub async fn handle_find_node_request(&self, from: &Contact, target: NodeId) -> Vec<Contact>;
  pub async fn handle_find_value_request(
      &self,
      from: &Contact,
      key: Key,
  ) -> (Option<Vec<u8>>, Vec<Contact>);
  pub async fn handle_store_request(&self, from: &Contact, key: Key, value: Vec<u8>);
  ```

- Backpressure configuration:

  ```rust
  pub async fn override_pressure_limits(
      &self,
      disk_limit: usize,
      memory_limit: usize,
      request_limit: usize,
  );
  ```

- Data replication (primarily for internal and diagnostic use):

  ```rust
  pub async fn put(&self, value: Vec<u8>) -> Result<Key>;
  ```

Internally, `DiscoveryNode` forwards to the inner `DhtNode` and ensures that all operations run through the adaptive, tiering, and telemetry logic.

---

## Example binary (`src/main.rs`)

The repository includes a small example binary that:

- Creates an iroh `Endpoint` with `DHT_ALPN`.
- Optionally enables mDNS discovery (`MdnsDiscovery`) for local peer discovery, and falls back to relay mode otherwise.
- Derives a `NodeId` from the endpoint identity.
- Creates a `Contact` using the node ID and JSON‑encoded `EndpointAddr`.
- Constructs an `IrohNetwork` and `DiscoveryNode`.
- Registers `DhtProtocolHandler` on the router for `DHT_ALPN`.
- Spawns a periodic telemetry logger that prints:

  - `pressure`
  - `stored_keys`
  - `tier_counts`
  - `tier_centroids`
  - `replication_factor` (`k`)
  - `concurrency` (`α`)

- Parks the main task with `future::pending::<()>().await`, leaving the node running to serve RPCs.

You can run it with:

```bash
cargo run
```

and then connect other instances or tools that speak the same `DhtProtocol` over iroh.

---

## Minimal usage outline

To embed `iroh-sdht` into your own iroh application:

1. Create an iroh `Endpoint`.
2. Derive a DHT `NodeId` from the endpoint identity.
3. Build a `Contact` with `NodeId` and JSON `EndpointAddr`.
4. Wrap the endpoint in `IrohNetwork`.
5. Construct a `DiscoveryNode`.
6. Register `DhtProtocolHandler` on a `Router` for `DHT_ALPN`.
7. Feed observed contacts into `DiscoveryNode` and use `iterative_find_node` for discovery.

```rust
use anyhow::Result;
use iroh::{Endpoint, EndpointAddr, RelayMode};
use iroh::protocol::Router;
use iroh_sdht::{
    derive_node_id,
    Contact,
    DiscoveryNode,
    DhtProtocolHandler,
    IrohNetwork,
    NodeId,
    DHT_ALPN,
};

async fn launch(endpoint: Endpoint, addr: EndpointAddr) -> Result<()> {
    // Derive node ID
    let node_id: NodeId = derive_node_id(endpoint.id().as_bytes());

    // Build contact (EndpointAddr as JSON)
    let self_contact = Contact {
        id: node_id,
        addr: serde_json::to_string(&addr)?,
    };

    // Wrap in an IrohNetwork
    let network = IrohNetwork {
        endpoint: endpoint.clone(),
        self_contact: self_contact.clone(),
    };

    // Create discovery node with chosen k and alpha
    let dht = DiscoveryNode::new(node_id, self_contact, network, /*k*/ 20, /*alpha*/ 3);

    // Register DHT protocol handler
    let _router = Router::builder(endpoint.clone())
        .accept(DHT_ALPN, DhtProtocolHandler::new(dht.clone()))
        .spawn();

    // Now you can:
    // - observe peers: dht.observe_contact(contact).await;
    // - perform lookups: let nodes = dht.iterative_find_node(target_id).await?;
    // - inspect telemetry: let snap = dht.telemetry_snapshot().await;

    Ok(())
}
```

---

## License

See [LICENSE](LICENSE).
