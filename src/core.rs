use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use rand::RngCore;
use iroh_blake3::Hasher;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

/// 256-bit IDs for nodes and keys
pub type NodeId = [u8; 32];
pub type Key = [u8; 32];

const CLUSTER_RECOMPUTE_INTERVAL: Duration = Duration::from_secs(300);
const MAX_RTT_SAMPLES_PER_NODE: usize = 32;
const MIN_LATENCY_CLUSTERS: usize = 1;
const MAX_LATENCY_CLUSTERS: usize = 6;
const KMEANS_ITERATIONS: usize = 20;
const CLUSTER_PENALTY_FACTOR: f32 = 1.5;
const PRESSURE_DISK_SOFT_LIMIT: usize = 8 * 1024 * 1024; // 8 MiB approx disk budget
const PRESSURE_MEMORY_SOFT_LIMIT: usize = 4 * 1024 * 1024; // 4 MiB approx memory budget
const PRESSURE_REQUEST_WINDOW: Duration = Duration::from_secs(60);
const PRESSURE_REQUEST_LIMIT: usize = 200;
const PRESSURE_THRESHOLD: f32 = 0.75;
const QUERY_STATS_WINDOW: usize = 100;

/// Content-addressed key: BLAKE3 hash of content bytes.
pub fn hash_content(data: &[u8]) -> Key {
    let mut hasher = Hasher::new();
    hasher.update(data);
    let digest = hasher.finalize();

    let mut key = [0u8; 32];
    key.copy_from_slice(digest.as_bytes());
    key
}

/// Verify that `key` matches `hash_content(value)`.
pub fn verify_key_value_pair(key: &Key, value: &[u8]) -> bool {
    hash_content(value) == *key
}

/// XOR distance between IDs.
pub fn xor_distance(a: &NodeId, b: &NodeId) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// Lexicographic compare of distances.
fn distance_cmp(a: &[u8; 32], b: &[u8; 32]) -> std::cmp::Ordering {
    for i in 0..32 {
        if a[i] < b[i] {
            return std::cmp::Ordering::Less;
        } else if a[i] > b[i] {
            return std::cmp::Ordering::Greater;
        }
    }
    std::cmp::Ordering::Equal
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClusterLevel(usize);

impl ClusterLevel {
    fn new(index: usize) -> Self {
        Self(index)
    }

    fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Debug, Default)]
pub struct ClusterStats {
    pub centroids: Vec<f32>,
    pub counts: Vec<usize>,
}

struct ClusterManager {
    assignments: HashMap<NodeId, ClusterLevel>,
    samples: HashMap<NodeId, VecDeque<f32>>,
    centroids: Vec<f32>,
    last_recompute: Instant,
    min_clusters: usize,
    max_clusters: usize,
}

impl ClusterManager {
    fn new() -> Self {
        Self {
            assignments: HashMap::new(),
            samples: HashMap::new(),
            centroids: vec![150.0],
            last_recompute: Instant::now() - CLUSTER_RECOMPUTE_INTERVAL,
            min_clusters: MIN_LATENCY_CLUSTERS,
            max_clusters: MAX_LATENCY_CLUSTERS,
        }
    }

    fn register_contact(&mut self, node: &NodeId) -> ClusterLevel {
        let default = self.default_level();
        *self.assignments.entry(*node).or_insert(default)
    }

    fn record_sample(&mut self, node: &NodeId, rtt_ms: f32) {
        let samples = self
            .samples
            .entry(*node)
            .or_insert_with(|| VecDeque::with_capacity(MAX_RTT_SAMPLES_PER_NODE));
        if samples.len() == MAX_RTT_SAMPLES_PER_NODE {
            samples.pop_front();
        }
        samples.push_back(rtt_ms);
        self.register_contact(node);
        self.recompute_if_needed();
    }

    fn level_for(&self, node: &NodeId) -> ClusterLevel {
        self.assignments
            .get(node)
            .copied()
            .unwrap_or_else(|| self.default_level())
    }

    fn stats(&self) -> ClusterStats {
        let mut counts = vec![0usize; self.centroids.len()];
        for level in self.assignments.values() {
            let idx = level.index();
            if idx < counts.len() {
                counts[idx] += 1;
            }
        }
        ClusterStats {
            centroids: self.centroids.clone(),
            counts,
        }
    }

    fn recompute_if_needed(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_recompute) < CLUSTER_RECOMPUTE_INTERVAL {
            return;
        }

        let mut per_node: Vec<(NodeId, f32)> = self
            .samples
            .iter()
            .filter_map(|(node, samples)| {
                if samples.is_empty() {
                    None
                } else {
                    let sum: f32 = samples.iter().sum();
                    let avg = sum / samples.len() as f32;
                    Some((*node, avg))
                }
            })
            .collect();

        if per_node.len() < self.min_clusters {
            self.last_recompute = now;
            return;
        }

        let max_k = per_node.len().min(self.max_clusters);
        let samples: Vec<f32> = per_node.iter().map(|(_, avg)| *avg).collect();

        let (centroids, assignments) = dynamic_kmeans(&samples, self.min_clusters, max_k);

        for ((node, _avg), cluster_idx) in per_node.iter().zip(assignments.iter()) {
            self.assignments
                .insert(*node, ClusterLevel::new(*cluster_idx));
        }

        if !centroids.is_empty() {
            self.centroids = centroids;
        }
        self.last_recompute = now;
    }

    fn default_level(&self) -> ClusterLevel {
        if self.centroids.is_empty() {
            return ClusterLevel::new(0);
        }
        ClusterLevel::new(self.centroids.len() / 2)
    }

    fn fastest_level(&self) -> ClusterLevel {
        ClusterLevel::new(0)
    }

    fn slowest_level(&self) -> ClusterLevel {
        if self.centroids.is_empty() {
            ClusterLevel::new(0)
        } else {
            ClusterLevel::new(self.centroids.len() - 1)
        }
    }

    fn next_level(&self, level: ClusterLevel) -> Option<ClusterLevel> {
        let next_idx = level.index() + 1;
        if next_idx < self.centroids.len() {
            Some(ClusterLevel::new(next_idx))
        } else {
            None
        }
    }
}

fn dynamic_kmeans(samples: &[f32], min_k: usize, max_k: usize) -> (Vec<f32>, Vec<usize>) {
    if samples.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut best_centroids = vec![samples[0]];
    let mut best_assignments = vec![0; samples.len()];
    let mut best_score = f32::MAX;

    let min_k = min_k.max(1);
    let max_k = max_k.max(min_k);

    for k in min_k..=max_k {
        let (centroids, assignments, inertia) = run_kmeans(samples, k);

        let penalty = (k as f32) * (samples.len() as f32).ln().max(1.0) * CLUSTER_PENALTY_FACTOR;
        let score = inertia + penalty;

        if score < best_score {
            best_score = score;
            best_centroids = centroids;
            best_assignments = assignments;
        }
    }

    (best_centroids, best_assignments)
}

fn run_kmeans(samples: &[f32], k: usize) -> (Vec<f32>, Vec<usize>, f32) {
    let mut centroids = initialize_centroids(samples, k);
    let mut assignments = vec![0usize; samples.len()];

    for _ in 0..KMEANS_ITERATIONS {
        let mut changed = false;
        let mut sums = vec![0.0f32; k];
        let mut counts = vec![0usize; k];

        for (idx, sample) in samples.iter().enumerate() {
            let nearest = nearest_center_scalar(*sample, &centroids);
            if assignments[idx] != nearest {
                assignments[idx] = nearest;
                changed = true;
            }
            sums[nearest] += sample;
            counts[nearest] += 1;
        }

        for i in 0..k {
            if counts[i] > 0 {
                centroids[i] = sums[i] / counts[i] as f32;
            }
        }

        if !changed {
            break;
        }
    }

    // Reinitialize empty clusters if any
    ensure_cluster_coverage(samples, &mut centroids, &mut assignments);

    // Compute inertia and sort centroids to enforce ordering from fastest to slowest.
    let mut inertia = 0.0f32;
    for (sample, idx) in samples.iter().zip(assignments.iter()) {
        let diff = sample - centroids[*idx];
        inertia += diff * diff;
    }

    let mut order: Vec<usize> = (0..k).collect();
    order.sort_by(|a, b| centroids[*a].partial_cmp(&centroids[*b]).unwrap());

    let mut remap = vec![0usize; k];
    let mut sorted_centroids = vec![0.0f32; k];
    for (new_idx, old_idx) in order.iter().enumerate() {
        sorted_centroids[new_idx] = centroids[*old_idx];
        remap[*old_idx] = new_idx;
    }

    let mut sorted_assignments = assignments;
    for idx in sorted_assignments.iter_mut() {
        *idx = remap[*idx];
    }

    (sorted_centroids, sorted_assignments, inertia)
}

fn ensure_cluster_coverage(samples: &[f32], centroids: &mut [f32], assignments: &mut [usize]) {
    let k = centroids.len();
    let mut counts = vec![0usize; k];
    for idx in assignments.iter() {
        counts[*idx] += 1;
    }

    if counts.iter().all(|count| *count > 0) {
        return;
    }

    let mut sorted_samples: Vec<f32> = samples.to_vec();
    sorted_samples.sort_by(|a, b| a.partial_cmp(b).unwrap());

    for (cluster_idx, count) in counts.iter_mut().enumerate() {
        if *count == 0 {
            let pos = ((cluster_idx as f32 + 0.5) / k as f32 * (sorted_samples.len() - 1) as f32)
                .round() as usize;
            centroids[cluster_idx] = sorted_samples[pos];
        }
    }

    for (sample_idx, sample) in samples.iter().enumerate() {
        let nearest = nearest_center_scalar(*sample, centroids);
        assignments[sample_idx] = nearest;
    }
}

fn initialize_centroids(samples: &[f32], k: usize) -> Vec<f32> {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

    if sorted.is_empty() {
        return vec![0.0];
    }

    let mut centroids = Vec::with_capacity(k);
    let max_idx = sorted.len() - 1;
    for i in 0..k {
        let pos = if k == 1 {
            max_idx
        } else {
            ((i as f32 + 0.5) / k as f32 * max_idx as f32).round() as usize
        };
        centroids.push(sorted[pos]);
    }

    centroids
}

fn nearest_center_scalar(value: f32, centers: &[f32]) -> usize {
    let mut best_idx = 0;
    let mut best_dist = f32::MAX;
    for (i, center) in centers.iter().enumerate() {
        let dist = (value - *center).abs();
        if dist < best_dist {
            best_dist = dist;
            best_idx = i;
        }
    }
    best_idx
}

struct PressureMonitor {
    current_bytes: usize,
    requests: VecDeque<Instant>,
    request_window: Duration,
    request_limit: usize,
    disk_limit: usize,
    memory_limit: usize,
    current_pressure: f32,
}

impl PressureMonitor {
    fn new() -> Self {
        Self {
            current_bytes: 0,
            requests: VecDeque::new(),
            request_window: PRESSURE_REQUEST_WINDOW,
            request_limit: PRESSURE_REQUEST_LIMIT,
            disk_limit: PRESSURE_DISK_SOFT_LIMIT,
            memory_limit: PRESSURE_MEMORY_SOFT_LIMIT,
            current_pressure: 0.0,
        }
    }

    fn record_store(&mut self, bytes: usize) {
        self.current_bytes = self.current_bytes.saturating_add(bytes);
    }

    fn record_evict(&mut self, bytes: usize) {
        self.current_bytes = self.current_bytes.saturating_sub(bytes);
    }

    fn record_request(&mut self) {
        let now = Instant::now();
        self.requests.push_back(now);
        self.trim_requests(now);
    }

    fn trim_requests(&mut self, now: Instant) {
        while let Some(front) = self.requests.front() {
            if now.duration_since(*front) > self.request_window {
                self.requests.pop_front();
            } else {
                break;
            }
        }
    }

    fn update_pressure(&mut self, stored_keys: usize) {
        let disk_ratio = self.current_bytes as f32 / self.disk_limit as f32;
        let memory_ratio = self.current_bytes as f32 / self.memory_limit as f32;
        let request_ratio = self.requests.len() as f32 / self.request_limit as f32;
        let combined = (disk_ratio + memory_ratio + request_ratio) / 3.0;
        if combined > 1.0 {
            self.current_pressure = 1.0;
        } else if combined < 0.0 {
            self.current_pressure = 0.0;
        } else {
            self.current_pressure = combined;
        }

        if stored_keys == 0 {
            self.current_pressure = self.current_pressure.min(1.0);
        }
    }

    fn current_pressure(&self) -> f32 {
        self.current_pressure
    }
}

struct LocalStore {
    entries: HashMap<Key, Vec<u8>>,
    order: VecDeque<Key>,
    pressure: PressureMonitor,
}

impl LocalStore {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            pressure: PressureMonitor::new(),
        }
    }

    fn record_request(&mut self) {
        self.pressure.record_request();
        let len = self.entries.len();
        self.pressure.update_pressure(len);
    }

    fn store(&mut self, key: Key, value: &[u8]) -> Vec<(Key, Vec<u8>)> {
        if let Some(existing) = self.entries.remove(&key) {
            self.pressure.record_evict(existing.len());
            self.order.retain(|k| *k != key);
        }

        let data = value.to_vec();
        self.pressure.record_store(data.len());
        self.order.push_back(key);
        self.entries.insert(key, data);
        self.pressure.update_pressure(self.entries.len());

        let mut spilled = Vec::new();
        while self.pressure.current_pressure() > PRESSURE_THRESHOLD {
            if let Some(evicted_key) = self.order.pop_front() {
                if let Some(evicted_val) = self.entries.remove(&evicted_key) {
                    self.pressure.record_evict(evicted_val.len());
                    self.pressure.update_pressure(self.entries.len());
                    spilled.push((evicted_key, evicted_val));
                }
            } else {
                break;
            }
        }
        spilled
    }

    fn get(&mut self, key: &Key) -> Option<Vec<u8>> {
        if let Some(value) = self.entries.get(key).cloned() {
            self.order.retain(|k| *k != *key);
            self.order.push_back(*key);
            return Some(value);
        }
        None
    }

    fn current_pressure(&self) -> f32 {
        self.pressure.current_pressure()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

struct AdaptiveParams {
    k: usize,
    alpha: usize,
    churn_history: VecDeque<bool>,
    #[allow(dead_code)]
    query_history: VecDeque<bool>,
}

impl AdaptiveParams {
    fn new(k: usize, alpha: usize) -> Self {
        Self {
            k,
            alpha,
            churn_history: VecDeque::new(),
            query_history: VecDeque::new(),
        }
    }

    fn record_churn(&mut self, success: bool) -> bool {
        self.churn_history.push_back(success);
        if self.churn_history.len() > QUERY_STATS_WINDOW {
            self.churn_history.pop_front();
        }
        let old_k = self.k;
        self.update_k();
        old_k != self.k
    }

    #[allow(dead_code)]
    fn record_query_success(&mut self, success: bool) {
        self.query_history.push_back(success);
        if self.query_history.len() > QUERY_STATS_WINDOW {
            self.query_history.pop_front();
        }
        self.update_alpha();
    }

    fn update_k(&mut self) {
        if self.churn_history.is_empty() {
            return;
        }
        let failures = self.churn_history.iter().filter(|entry| !**entry).count();
        let churn_rate = failures as f32 / self.churn_history.len() as f32;
        let mut new_k = 10.0 + (20.0 * churn_rate).round();
        if new_k < 10.0 {
            new_k = 10.0;
        }
        if new_k > 30.0 {
            new_k = 30.0;
        }
        self.k = new_k as usize;
    }

    #[allow(dead_code)]
    fn update_alpha(&mut self) {
        if self.query_history.is_empty() {
            return;
        }
        let successes = self.query_history.iter().filter(|entry| **entry).count();
        let success_rate = successes as f32 / self.query_history.len() as f32;
        self.alpha = if success_rate < 0.5 {
            5
        } else if success_rate < 0.7 {
            4
        } else if success_rate < 0.9 {
            3
        } else {
            2
        };
    }

    fn current_k(&self) -> usize {
        self.k
    }

    fn current_alpha(&self) -> usize {
        self.alpha
    }
}

#[allow(dead_code)]
struct LevelHistory {
    history: VecDeque<bool>,
    window: usize,
}

#[allow(dead_code)]
impl LevelHistory {
    fn new(window: usize) -> Self {
        Self {
            history: VecDeque::new(),
            window,
        }
    }

    fn record(&mut self, success: bool) -> f32 {
        self.history.push_back(success);
        if self.history.len() > self.window {
            self.history.pop_front();
        }
        self.miss_rate()
    }

    fn miss_rate(&self) -> f32 {
        if self.history.is_empty() {
            return 0.0;
        }
        let misses = self.history.iter().filter(|entry| !**entry).count();
        misses as f32 / self.history.len() as f32
    }
}

#[allow(dead_code)]
struct QueryEscalation {
    levels: HashMap<ClusterLevel, LevelHistory>,
    window: usize,
}

#[allow(dead_code)]
impl QueryEscalation {
    fn new(window: usize) -> Self {
        Self {
            levels: HashMap::new(),
            window,
        }
    }

    fn record(&mut self, level: ClusterLevel, success: bool) -> f32 {
        let history = self
            .levels
            .entry(level)
            .or_insert_with(|| LevelHistory::new(self.window));
        history.record(success)
    }

    fn miss_rate(&self, level: ClusterLevel) -> f32 {
        self.levels
            .get(&level)
            .map(|history| history.miss_rate())
            .unwrap_or(0.0)
    }
}

#[derive(Clone, Debug, Default)]
pub struct TelemetrySnapshot {
    pub cluster_centroids: Vec<f32>,
    pub cluster_counts: Vec<usize>,
    pub pressure: f32,
    pub stored_keys: usize,
    pub replication_factor: usize,
    pub concurrency: usize,
}

/// Find the bucket index for a node ID relative to self.
/// Use index of first differing bit (0..=255).
fn bucket_index(self_id: &NodeId, other: &NodeId) -> usize {
    let dist = xor_distance(self_id, other);
    for (byte_idx, byte) in dist.iter().enumerate() {
        if *byte != 0 {
            let leading = byte.leading_zeros() as usize; // 0..7
            let bit_index = byte_idx * 8 + leading;
            return bit_index; // 0..=255
        }
    }
    // identical ID: put in the "last" bucket
    255
}

/// Represents another DHT node (ID + opaque address).
/// We'll use `addr` to store JSON-serialized iroh EndpointAddr.
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Contact {
    pub id: NodeId,
    pub addr: String,
}

/// A single Kademlia routing bucket (LRU-ish).
#[derive(Debug, Default, Clone)]
struct Bucket {
    contacts: Vec<Contact>,
}

impl Bucket {
    fn new() -> Self {
        Self {
            contacts: Vec::new(),
        }
    }

    fn touch(&mut self, contact: Contact, k: usize) {
        if let Some(pos) = self.contacts.iter().position(|c| c.id == contact.id) {
            let existing = self.contacts.remove(pos);
            self.contacts.push(existing);
            return;
        }

        if self.contacts.len() < k {
            self.contacts.push(contact);
        } else {
            // Real Kademlia would ping the oldest before evicting.
            // For simplicity we just drop the new one here.
        }
    }
}

/// Kademlia routing table: 256 buckets for 256-bit IDs.
#[derive(Debug)]
pub struct RoutingTable {
    self_id: NodeId,
    k: usize,
    buckets: Vec<Bucket>, // len = 256
}

impl RoutingTable {
    pub fn new(self_id: NodeId, k: usize) -> Self {
        let mut buckets = Vec::with_capacity(256);
        for _ in 0..256 {
            buckets.push(Bucket::new());
        }
        Self {
            self_id,
            k,
            buckets,
        }
    }

    pub fn set_k(&mut self, k: usize) {
        self.k = k;
        for bucket in &mut self.buckets {
            if bucket.contacts.len() > self.k {
                while bucket.contacts.len() > self.k {
                    bucket.contacts.remove(0);
                }
            }
        }
    }

    pub fn update(&mut self, contact: Contact) {
        if contact.id == self.self_id {
            return;
        }
        let idx = bucket_index(&self.self_id, &contact.id);
        self.buckets[idx].touch(contact, self.k);
    }

    pub fn closest(&self, target: &NodeId, k: usize) -> Vec<Contact> {
        let mut all: Vec<Contact> = self
            .buckets
            .iter()
            .flat_map(|b| b.contacts.iter().cloned())
            .collect();

        all.sort_by(|a, b| {
            let da = xor_distance(&a.id, target);
            let db = xor_distance(&b.id, target);
            distance_cmp(&da, &db)
        });

        if all.len() > k {
            all.truncate(k);
        }
        all
    }
}

/// Network abstraction used by the core.
/// We'll implement it for iroh transport.
#[async_trait]
pub trait DhtNetwork: Send + Sync + 'static {
    async fn find_node(&self, to: &Contact, target: NodeId) -> Result<Vec<Contact>>;

    /// Return (value, closer_nodes).
    #[allow(dead_code)]
    async fn find_value(&self, to: &Contact, key: Key) -> Result<(Option<Vec<u8>>, Vec<Contact>)>;

    async fn store(&self, to: &Contact, key: Key, value: Vec<u8>) -> Result<()>;
}

/// High level state machine that drives the adaptive Kademlia algorithm.
///
/// A `DhtNode` owns a routing table, a small content-addressable store and the
/// [`DhtNetwork`] transport that is used to send RPCs to other peers.  The type
/// is intentionally generic over the network layer so that tests can supply an
/// in-memory mock while applications embed the production
/// [`crate::net::IrohNetwork`].
///
/// The public async methods implement the core Kademlia workflows:
///
/// * [`observe_contact`](Self::observe_contact) updates the routing table when
///   new peers are discovered.
/// * [`iterative_find_node`](Self::iterative_find_node) performs a full
///   lookup, automatically tuning `k` and `α` based on recent success metrics.
/// * [`handle_find_node_request`](Self::handle_find_node_request),
///   [`handle_find_value_request`](Self::handle_find_value_request) and
///   [`handle_store_request`](Self::handle_store_request) implement the RPCs a
///   remote peer can invoke.
///
/// By keeping the type small and `Arc` friendly we can cheaply clone it and
/// share it between background tasks such as telemetry collection, incoming
/// connection handlers and API frontends.
pub struct DhtNode<N: DhtNetwork> {
    #[allow(dead_code)]
    pub id: NodeId,
    pub self_contact: Contact,
    routing: Arc<Mutex<RoutingTable>>,
    store: Arc<Mutex<LocalStore>>,
    network: Arc<N>,
    params: Arc<Mutex<AdaptiveParams>>,
    cluster: Arc<Mutex<ClusterManager>>,
    #[allow(dead_code)]
    escalation: Arc<Mutex<QueryEscalation>>,
}

impl<N: DhtNetwork> DhtNode<N> {
    pub fn new(id: NodeId, self_contact: Contact, network: N, k: usize, alpha: usize) -> Self {
        Self {
            id,
            self_contact,
            routing: Arc::new(Mutex::new(RoutingTable::new(id, k))),
            store: Arc::new(Mutex::new(LocalStore::new())),
            network: Arc::new(network),
            params: Arc::new(Mutex::new(AdaptiveParams::new(k, alpha))),
            cluster: Arc::new(Mutex::new(ClusterManager::new())),
            escalation: Arc::new(Mutex::new(QueryEscalation::new(QUERY_STATS_WINDOW))),
        }
    }

    pub async fn observe_contact(&self, contact: Contact) {
        {
            let mut cluster = self.cluster.lock().await;
            cluster.register_contact(&contact.id);
        }
        let k = {
            let params = self.params.lock().await;
            params.current_k()
        };
        let mut rt = self.routing.lock().await;
        rt.set_k(k);
        rt.update(contact);
    }

    async fn store_local(&self, key: Key, value: Vec<u8>) {
        if !verify_key_value_pair(&key, &value) {
            return;
        }
        let spilled = {
            let mut store = self.store.lock().await;
            store.record_request();
            store.store(key, &value)
        };
        if !spilled.is_empty() {
            self.offload_spilled(spilled).await;
        }
    }

    async fn get_local(&self, key: &Key) -> Option<Vec<u8>> {
        let mut store = self.store.lock().await;
        store.record_request();
        store.get(key)
    }

    /// Answer a FIND_NODE coming from the network.
    pub async fn handle_find_node_request(&self, from: &Contact, target: NodeId) -> Vec<Contact> {
        self.observe_contact(from.clone()).await;
        let k = {
            let params = self.params.lock().await;
            params.current_k()
        };
        let rt = self.routing.lock().await;
        rt.closest(&target, k)
    }

    /// Answer a FIND_VALUE coming from the network.
    /// Returns (value, closer_nodes).
    pub async fn handle_find_value_request(
        &self,
        from: &Contact,
        key: Key,
    ) -> (Option<Vec<u8>>, Vec<Contact>) {
        self.observe_contact(from.clone()).await;
        if let Some(v) = self.get_local(&key).await {
            return (Some(v), Vec::new());
        }
        let target: NodeId = key;
        let k = {
            let params = self.params.lock().await;
            params.current_k()
        };
        let rt = self.routing.lock().await;
        let closer = rt.closest(&target, k);
        (None, closer)
    }

    /// Handle a STORE request from the network.
    pub async fn handle_store_request(&self, from: &Contact, key: Key, value: Vec<u8>) {
        self.observe_contact(from.clone()).await;
        self.store_local(key, value).await;
    }

    async fn level_matches(&self, node: &NodeId, level_filter: Option<ClusterLevel>) -> bool {
        if let Some(level) = level_filter {
            let cluster = self.cluster.lock().await;
            cluster.level_for(node) == level
        } else {
            true
        }
    }

    async fn filter_contacts(
        &self,
        contacts: Vec<Contact>,
        level_filter: Option<ClusterLevel>,
    ) -> Vec<Contact> {
        if level_filter.is_none() {
            return contacts;
        }
        let level = level_filter.unwrap();
        let cluster = self.cluster.lock().await;
        contacts
            .into_iter()
            .filter(|c| cluster.level_for(&c.id) == level)
            .collect()
    }

    async fn record_rtt(&self, contact: &Contact, elapsed: Duration) {
        let rtt_ms = (elapsed.as_secs_f64() * 1000.0) as f32;
        let mut cluster = self.cluster.lock().await;
        cluster.record_sample(&contact.id, rtt_ms);
    }

    async fn adjust_k(&self, success: bool) {
        let (changed, new_k) = {
            let mut params = self.params.lock().await;
            let changed = params.record_churn(success);
            let current_k = params.current_k();
            (changed, current_k)
        };
        if changed {
            let mut rt = self.routing.lock().await;
            rt.set_k(new_k);
        }
    }

    async fn current_k(&self) -> usize {
        let params = self.params.lock().await;
        params.current_k()
    }

    async fn current_alpha(&self) -> usize {
        let params = self.params.lock().await;
        params.current_alpha()
    }

    pub async fn iterative_find_node(&self, target: NodeId) -> Result<Vec<Contact>> {
        self.iterative_find_node_with_level(target, None).await
    }

    async fn iterative_find_node_with_level(
        &self,
        target: NodeId,
        level_filter: Option<ClusterLevel>,
    ) -> Result<Vec<Contact>> {
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut queried: HashSet<NodeId> = HashSet::new();
        let k_initial = self.current_k().await;
        let mut shortlist = {
            let rt = self.routing.lock().await;
            rt.closest(&target, k_initial)
        };
        shortlist = self.filter_contacts(shortlist, level_filter).await;
        shortlist.sort_by(|a, b| {
            let da = xor_distance(&a.id, &target);
            let db = xor_distance(&b.id, &target);
            distance_cmp(&da, &db)
        });

        for c in &shortlist {
            seen.insert(c.id);
        }

        let mut best_distance = shortlist
            .first()
            .map(|c| xor_distance(&c.id, &target))
            .unwrap_or([0xff; 32]);

        loop {
            let alpha = self.current_alpha().await;
            let candidates: Vec<Contact> = shortlist
                .iter()
                .filter(|c| !queried.contains(&c.id))
                .take(alpha)
                .cloned()
                .collect();

            if candidates.is_empty() {
                break;
            }

            let mut any_closer = false;

            for contact in candidates {
                queried.insert(contact.id);

                let start = Instant::now();
                let response = match self.network.find_node(&contact, target).await {
                    Ok(nodes) => {
                        let elapsed = start.elapsed();
                        self.record_rtt(&contact, elapsed).await;
                        self.adjust_k(true).await;
                        self.observe_contact(contact.clone()).await;
                        for n in &nodes {
                            self.observe_contact(n.clone()).await;
                        }
                        nodes
                    }
                    Err(_) => {
                        self.adjust_k(false).await;
                        continue;
                    }
                };

                for n in response {
                    if seen.insert(n.id) {
                        if self.level_matches(&n.id, level_filter).await {
                            shortlist.push(n.clone());
                        }
                    }
                }

                shortlist.sort_by(|a, b| {
                    let da = xor_distance(&a.id, &target);
                    let db = xor_distance(&b.id, &target);
                    distance_cmp(&da, &db)
                });

                let k = self.current_k().await;
                if shortlist.len() > k {
                    shortlist.truncate(k);
                }

                if let Some(first) = shortlist.first() {
                    let new_best = xor_distance(&first.id, &target);
                    if distance_cmp(&new_best, &best_distance) == std::cmp::Ordering::Less {
                        best_distance = new_best;
                        any_closer = true;
                    }
                }
            }

            if !any_closer {
                break;
            }
        }

        Ok(shortlist)
    }

    #[allow(dead_code)]
    pub async fn iterative_find_value(&self, key: Key) -> Result<(Option<Vec<u8>>, Vec<Contact>)> {
        self.iterative_find_value_with_level(key, None).await
    }

    #[allow(dead_code)]
    async fn iterative_find_value_with_level(
        &self,
        key: Key,
        level_filter: Option<ClusterLevel>,
    ) -> Result<(Option<Vec<u8>>, Vec<Contact>)> {
        if let Some(v) = self.get_local(&key).await {
            return Ok((Some(v), Vec::new()));
        }

        let target: NodeId = key;
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut queried: HashSet<NodeId> = HashSet::new();

        let k_initial = self.current_k().await;
        let mut shortlist = {
            let rt = self.routing.lock().await;
            rt.closest(&target, k_initial)
        };
        shortlist = self.filter_contacts(shortlist, level_filter).await;
        shortlist.sort_by(|a, b| {
            let da = xor_distance(&a.id, &target);
            let db = xor_distance(&b.id, &target);
            distance_cmp(&da, &db)
        });

        for c in &shortlist {
            seen.insert(c.id);
        }

        let mut best_distance = shortlist
            .first()
            .map(|c| xor_distance(&c.id, &target))
            .unwrap_or([0xff; 32]);

        loop {
            let alpha = self.current_alpha().await;
            let candidates: Vec<Contact> = shortlist
                .iter()
                .filter(|c| !queried.contains(&c.id))
                .take(alpha)
                .cloned()
                .collect();

            if candidates.is_empty() {
                break;
            }

            let mut any_closer = false;

            for contact in candidates {
                queried.insert(contact.id);

                let start = Instant::now();
                let response = match self.network.find_value(&contact, key).await {
                    Ok(result) => {
                        let elapsed = start.elapsed();
                        self.record_rtt(&contact, elapsed).await;
                        self.adjust_k(true).await;
                        self.observe_contact(contact.clone()).await;
                        result
                    }
                    Err(_) => {
                        self.adjust_k(false).await;
                        continue;
                    }
                };

                let (maybe_val, closer) = response;

                if let Some(v) = maybe_val {
                    if verify_key_value_pair(&key, &v) {
                        self.store_local(key, v.clone()).await;
                        return Ok((Some(v), Vec::new()));
                    }
                }

                for n in &closer {
                    self.observe_contact(n.clone()).await;
                }

                for n in closer {
                    if seen.insert(n.id) {
                        if self.level_matches(&n.id, level_filter).await {
                            shortlist.push(n.clone());
                        }
                    }
                }

                shortlist.sort_by(|a, b| {
                    let da = xor_distance(&a.id, &target);
                    let db = xor_distance(&b.id, &target);
                    distance_cmp(&da, &db)
                });

                let k = self.current_k().await;
                if shortlist.len() > k {
                    shortlist.truncate(k);
                }

                if let Some(first) = shortlist.first() {
                    let new_best = xor_distance(&first.id, &target);
                    if distance_cmp(&new_best, &best_distance) == std::cmp::Ordering::Less {
                        best_distance = new_best;
                        any_closer = true;
                    }
                }
            }

            if !any_closer {
                break;
            }
        }

        Ok((None, shortlist))
    }

    async fn offload_spilled(&self, spilled: Vec<(Key, Vec<u8>)>) {
        if spilled.is_empty() {
            return;
        }

        let target_level = {
            let cluster = self.cluster.lock().await;
            cluster.slowest_level()
        };

        for (key, value) in spilled {
            self.replicate_to_level(key, value.clone(), target_level)
                .await;
        }
    }

    async fn replicate_to_level(&self, key: Key, value: Vec<u8>, level: ClusterLevel) {
        let target: NodeId = key;
        if let Ok(contacts) = self
            .iterative_find_node_with_level(target, Some(level))
            .await
        {
            let k = self.current_k().await;
            for contact in contacts.into_iter().take(k) {
                self.send_store(&contact, key, value.clone()).await;
            }
        }
    }

    async fn send_store(&self, contact: &Contact, key: Key, value: Vec<u8>) {
        let start = Instant::now();
        let result = self.network.store(contact, key, value).await;
        match result {
            Ok(_) => {
                let elapsed = start.elapsed();
                self.record_rtt(contact, elapsed).await;
                self.adjust_k(true).await;
                self.observe_contact(contact.clone()).await;
            }
            Err(_) => {
                self.adjust_k(false).await;
            }
        }
    }

    #[allow(dead_code)]
    async fn adjust_alpha(&self, success: bool) {
        let mut params = self.params.lock().await;
        params.record_query_success(success);
    }

    /// PUT with distance-based replication.
    #[allow(dead_code)]
    pub async fn put(&self, value: Vec<u8>) -> Result<Key> {
        let key = hash_content(&value);

        self.store_local(key, value.clone()).await;

        let target: NodeId = key;
        let closest = self.iterative_find_node_with_level(target, None).await?;
        let k = self.current_k().await;

        for contact in closest.into_iter().take(k) {
            self.send_store(&contact, key, value.clone()).await;
        }

        Ok(key)
    }

    /// GET with integrity, escalation, and optional re-replication.
    #[allow(dead_code)]
    pub async fn get(&self, key: Key) -> Result<Option<Vec<u8>>> {
        if let Some(v) = self.get_local(&key).await {
            self.adjust_alpha(true).await;
            return Ok(Some(v));
        }

        let Some(mut current_level) = ({
            let cluster = self.cluster.lock().await;
            if cluster.centroids.is_empty() {
                None
            } else {
                Some(cluster.fastest_level())
            }
        }) else {
            return Ok(None);
        };

        loop {
            let (maybe_val, _closer) = self
                .iterative_find_value_with_level(key, Some(current_level))
                .await?;
            let success = maybe_val.is_some();
            let miss_rate = {
                let mut escalation = self.escalation.lock().await;
                escalation.record(current_level, success)
            };

            if let Some(value) = maybe_val {
                self.adjust_alpha(true).await;
                let target: NodeId = key;
                let closest = self.iterative_find_node_with_level(target, None).await?;
                let k = self.current_k().await;
                for contact in closest.into_iter().take(k) {
                    self.send_store(&contact, key, value.clone()).await;
                }
                return Ok(Some(value));
            }

            let next_level = {
                let cluster = self.cluster.lock().await;
                if miss_rate > 0.30 {
                    cluster.next_level(current_level)
                } else {
                    None
                }
            };

            if let Some(level) = next_level {
                current_level = level;
                continue;
            }

            break;
        }

        self.adjust_alpha(false).await;
        Ok(None)
    }

    pub async fn telemetry_snapshot(&self) -> TelemetrySnapshot {
        let cluster_stats = {
            let cluster = self.cluster.lock().await;
            cluster.stats()
        };
        let (pressure, stored_keys) = {
            let store = self.store.lock().await;
            (store.current_pressure(), store.len())
        };
        let params = self.params.lock().await;
        TelemetrySnapshot {
            cluster_centroids: cluster_stats.centroids,
            cluster_counts: cluster_stats.counts,
            pressure,
            stored_keys,
            replication_factor: params.current_k(),
            concurrency: params.current_alpha(),
        }
    }
}

/// Helper: random NodeId for tests (not used with iroh IDs).
#[allow(dead_code)]
pub fn random_node_id() -> NodeId {
    let mut id = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn hash_content_is_deterministic() {
        let data = b"hello world";
        let hash_one = hash_content(data);
        let hash_two = hash_content(data);
        assert_eq!(hash_one, hash_two, "hashes of identical data should match");

        let different_hash = hash_content(b"goodbye world");
        assert_ne!(hash_one, different_hash, "hashes of different data should differ");
    }

    #[test]
    fn verify_key_value_pair_matches_hash() {
        let data = b"payload";
        let key = hash_content(data);
        assert!(
            verify_key_value_pair(&key, data),
            "verify_key_value_pair should accept matching key/value pairs"
        );

        let mut wrong_key = key;
        wrong_key[0] ^= 0xFF;
        assert!(
            !verify_key_value_pair(&wrong_key, data),
            "verify_key_value_pair should reject non-matching key/value pairs"
        );
    }

    #[test]
    fn hash_content_matches_blake3_reference() {
        let data = b"hello world";
        let expected = iroh_blake3::hash(data);
        let mut expected_bytes = [0u8; 32];
        expected_bytes.copy_from_slice(expected.as_bytes());

        assert_eq!(
            hash_content(data),
            expected_bytes,
            "hash_content should produce the BLAKE3 digest"
        );
    }

    #[test]
    fn xor_distance_produces_expected_value() {
        let mut a = [0u8; 32];
        a[0] = 0b1010_1010;
        let mut b = [0u8; 32];
        b[0] = 0b0101_0101;

        let dist = xor_distance(&a, &b);
        assert_eq!(dist[0], 0b1111_1111);
        assert!(dist.iter().skip(1).all(|byte| *byte == 0));
    }

    #[test]
    fn distance_cmp_orders_lexicographically() {
        let mut smaller = [0u8; 32];
        smaller[1] = 1;
        let mut larger = [0u8; 32];
        larger[1] = 2;

        assert_eq!(distance_cmp(&smaller, &larger), Ordering::Less);
        assert_eq!(distance_cmp(&larger, &smaller), Ordering::Greater);
        assert_eq!(distance_cmp(&smaller, &smaller), Ordering::Equal);
    }

    #[test]
    fn bucket_index_finds_first_different_bit() {
        let self_id = [0u8; 32];

        let mut other = [0u8; 32];
        other[0] = 0b1000_0000;
        assert_eq!(bucket_index(&self_id, &other), 0);

        let mut other_two = [0u8; 32];
        other_two[1] = 0b0001_0000;
        assert_eq!(bucket_index(&self_id, &other_two), 11);

        assert_eq!(bucket_index(&self_id, &self_id), 255);
    }
}
