//! Core DHT logic: transport-agnostic Kademlia implementation with adaptive tiering.
//!
//! This module contains the fundamental building blocks of the sloppy DHT:
//!
//! - **Identity & Hashing**: [`NodeId`], [`Key`], [`derive_node_id`], [`hash_content`]
//! - **Distance Metrics**: [`xor_distance`] for Kademlia-style routing
//! - **Routing**: [`RoutingTable`], [`Contact`] for peer management
//! - **Storage**: Local content-addressable store with LRU eviction and backpressure
//! - **Tiering**: Latency-based peer classification using k-means clustering
//! - **Adaptive Parameters**: Dynamic `k` adjustment based on network churn
//! - **Node State Machine**: [`DhtNode`] and [`DiscoveryNode`] for DHT operations

use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use iroh_blake3::Hasher;
use lru::LruCache;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tracing::debug;

// ============================================================================
// Type Aliases
// ============================================================================

/// A 256-bit identifier for DHT nodes.
///
/// Node IDs are derived from the node's public key using BLAKE3 hashing,
/// ensuring a uniform distribution across the identifier space.
pub type NodeId = [u8; 32];

/// A 256-bit content-addressed key for stored values.
///
/// Keys are computed as the BLAKE3 hash of the content, providing
/// content-addressable storage with built-in integrity verification.
pub type Key = [u8; 32];

// ============================================================================
// Configuration Constants
// ============================================================================

/// How often to recompute latency tier assignments (5 minutes).
const TIERING_RECOMPUTE_INTERVAL: Duration = Duration::from_secs(300);

/// Maximum RTT samples retained per node for latency averaging.
const MAX_RTT_SAMPLES_PER_NODE: usize = 32;

/// Minimum number of latency tiers to maintain.
const MIN_LATENCY_TIERS: usize = 1;

/// Maximum number of latency tiers (prevents over-fragmentation).
const MAX_LATENCY_TIERS: usize = 7;

/// Number of iterations for k-means clustering convergence.
const KMEANS_ITERATIONS: usize = 20;

/// Penalty factor to discourage excessive tier count in k-means.
/// Higher values favor fewer, larger tiers.
const TIERING_PENALTY_FACTOR: f32 = 1.5;

/// Soft limit for approximate disk usage before triggering backpressure (8 MiB).
const PRESSURE_DISK_SOFT_LIMIT: usize = 8 * 1024 * 1024;

/// Soft limit for approximate memory usage before triggering backpressure (4 MiB).
const PRESSURE_MEMORY_SOFT_LIMIT: usize = 4 * 1024 * 1024;

/// Time window for counting store/get requests in pressure calculation.
const PRESSURE_REQUEST_WINDOW: Duration = Duration::from_secs(60);

/// Maximum requests within the window before contributing to pressure.
const PRESSURE_REQUEST_LIMIT: usize = 200;

/// Pressure threshold (0.0-1.0) above which LRU eviction is triggered.
const PRESSURE_THRESHOLD: f32 = 0.75;

/// Sliding window size for tracking RPC success/failure for adaptive `k`.
const QUERY_STATS_WINDOW: usize = 100;

// ============================================================================
// Hashing Functions
// ============================================================================

/// Compute a 32-byte BLAKE3 digest of the input data.
fn blake3_digest(data: &[u8]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(data);
    let digest = hasher.finalize();

    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_bytes());
    out
}

/// Derive a stable 32-byte [`NodeId`] by hashing arbitrary input with BLAKE3.
///
/// Typically used to derive a node's DHT identity from its public key:
///
/// ```
/// use iroh_sdht::derive_node_id;
///
/// let public_key = b"example-public-key-bytes";
/// let node_id = derive_node_id(public_key);
/// assert_eq!(node_id.len(), 32);
/// ```
pub fn derive_node_id(data: &[u8]) -> NodeId {
    blake3_digest(data)
}

/// Compute a content-addressed key as the BLAKE3 hash of content bytes.
///
/// This is the standard way to derive a DHT key for storing content:
///
/// ```
/// use iroh_sdht::hash_content;
///
/// let content = b"hello world";
/// let key = hash_content(content);
/// // The same content always produces the same key
/// assert_eq!(key, hash_content(content));
/// ```
pub fn hash_content(data: &[u8]) -> Key {
    blake3_digest(data)
}

/// Verify that a key matches the hash of a value.
///
/// Used to validate content integrity after retrieval:
///
/// ```
/// use iroh_sdht::{hash_content, verify_key_value_pair};
///
/// let content = b"my data";
/// let key = hash_content(content);
/// assert!(verify_key_value_pair(&key, content));
/// assert!(!verify_key_value_pair(&key, b"wrong data"));
/// ```
pub fn verify_key_value_pair(key: &Key, value: &[u8]) -> bool {
    hash_content(value) == *key
}

// ============================================================================
// Distance Metrics
// ============================================================================

/// Compute the XOR distance between two node IDs.
///
/// XOR distance is the foundation of Kademlia routing. Nodes that are
/// "closer" in XOR space share more leading bits in common.
///
/// # Properties
/// - `xor_distance(a, a) == [0; 32]` (reflexive)
/// - `xor_distance(a, b) == xor_distance(b, a)` (symmetric)
/// - The result is used with [`distance_cmp`] to order nodes by proximity.
pub fn xor_distance(a: &NodeId, b: &NodeId) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// Compare two XOR distances lexicographically.
///
/// Returns `Ordering::Less` if `a` represents a smaller distance,
/// `Ordering::Greater` if larger, or `Ordering::Equal` if identical.
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

// ============================================================================
// Latency Tiering
// ============================================================================

/// A tier assignment for a node based on observed latency.
///
/// Lower tier indices represent faster (lower latency) peers.
/// Tier 0 is the fastest tier, and the highest index is the slowest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TieringLevel(usize);

impl TieringLevel {
    /// Create a new tiering level with the given index.
    fn new(index: usize) -> Self {
        Self(index)
    }

    /// Get the numeric index of this tier (0 = fastest).
    fn index(self) -> usize {
        self.0
    }
}

/// Statistics about the current latency tier distribution.
#[derive(Clone, Debug, Default)]
pub struct TieringStats {
    /// Centroid latencies for each tier in milliseconds (sorted fastest to slowest).
    pub centroids: Vec<f32>,
    /// Number of peers assigned to each tier.
    pub counts: Vec<usize>,
}

/// Manages latency-based tiering of DHT peers using k-means clustering.
///
/// The tiering manager:
/// 1. Collects RTT samples from RPC interactions
/// 2. Periodically recomputes tier assignments using k-means
/// 3. Assigns new contacts to a default middle tier
///
/// This enables latency-aware routing where fast peers are preferred.
struct TieringManager {
    /// Current tier assignment for each known node.
    assignments: HashMap<NodeId, TieringLevel>,
    /// Rolling RTT samples per node (up to MAX_RTT_SAMPLES_PER_NODE).
    samples: HashMap<NodeId, VecDeque<f32>>,
    /// Current tier centroids in milliseconds (sorted fastest to slowest).
    centroids: Vec<f32>,
    /// Timestamp of last tier recomputation.
    last_recompute: Instant,
    /// Minimum number of tiers to maintain.
    min_tiers: usize,
    /// Maximum number of tiers allowed.
    max_tiers: usize,
}

impl TieringManager {
    /// Create a new tiering manager with default settings.
    fn new() -> Self {
        Self {
            assignments: HashMap::new(),
            samples: HashMap::new(),
            // Start with a single tier at 150ms as a reasonable default
            centroids: vec![150.0],
            last_recompute: Instant::now() - TIERING_RECOMPUTE_INTERVAL,
            min_tiers: MIN_LATENCY_TIERS,
            max_tiers: MAX_LATENCY_TIERS,
        }
    }

    /// Register a contact and assign it to the default tier if new.
    fn register_contact(&mut self, node: &NodeId) -> TieringLevel {
        let default = self.default_level();
        *self.assignments.entry(*node).or_insert(default)
    }

    /// Record an RTT sample for a node and trigger recomputation if due.
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

    /// Get the current tier level for a node.
    fn level_for(&self, node: &NodeId) -> TieringLevel {
        self.assignments
            .get(node)
            .copied()
            .unwrap_or_else(|| self.default_level())
    }

    /// Get current tiering statistics including centroids and node counts per tier.
    fn stats(&self) -> TieringStats {
        let mut counts = vec![0usize; self.centroids.len()];
        for level in self.assignments.values() {
            let idx = level.index();
            if idx < counts.len() {
                counts[idx] += 1;
            }
        }
        TieringStats {
            centroids: self.centroids.clone(),
            counts,
        }
    }

    /// Recompute tier assignments using k-means clustering if the recompute interval has elapsed.
    ///
    /// This method:
    /// 1. Computes average RTT for each node from collected samples
    /// 2. Runs dynamic k-means to find optimal tier centroids
    /// 3. Reassigns all nodes to their closest tier
    fn recompute_if_needed(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_recompute) < TIERING_RECOMPUTE_INTERVAL {
            return;
        }

        let per_node: Vec<(NodeId, f32)> = self
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

        let min_required = self.min_tiers.max(2);
        if per_node.len() < min_required {
            return;
        }

        let max_k = per_node.len().min(self.max_tiers);
        let samples: Vec<f32> = per_node.iter().map(|(_, avg)| *avg).collect();

        let (centroids, assignments) = dynamic_kmeans(&samples, self.min_tiers, max_k);

        for ((node, _avg), tier_idx) in per_node.iter().zip(assignments.iter()) {
            self.assignments.insert(*node, TieringLevel::new(*tier_idx));
        }

        if !centroids.is_empty() {
            self.centroids = centroids;
        }
        self.last_recompute = now;
    }

    /// Get the default tier level (middle tier) for new nodes without samples.
    fn default_level(&self) -> TieringLevel {
        if self.centroids.is_empty() {
            return TieringLevel::new(0);
        }
        TieringLevel::new(self.centroids.len() / 2)
    }

    /// Get the slowest (highest latency) tier level.
    fn slowest_level(&self) -> TieringLevel {
        if self.centroids.is_empty() {
            TieringLevel::new(0)
        } else {
            TieringLevel::new(self.centroids.len() - 1)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// K-means Clustering for Latency Tiering
// ─────────────────────────────────────────────────────────────────────────────

/// Run dynamic k-means clustering to find the optimal number of tiers.
///
/// Uses a BIC-like penalty to balance cluster fit against model complexity.
/// Returns (centroids sorted by latency, tier assignments for each sample).
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

        // Penalize more clusters using BIC-like criterion
        let penalty = (k as f32) * (samples.len() as f32).ln().max(1.0) * TIERING_PENALTY_FACTOR;
        let score = inertia + penalty;

        if score < best_score {
            best_score = score;
            best_centroids = centroids;
            best_assignments = assignments;
        }
    }

    (best_centroids, best_assignments)
}

/// Run k-means clustering with a fixed number of clusters.
///
/// Returns (centroids, assignments, inertia) where inertia is the sum of squared
/// distances from samples to their assigned centroids.
fn run_kmeans(samples: &[f32], k: usize) -> (Vec<f32>, Vec<usize>, f32) {
    let mut centroids = initialize_centroids(samples, k);
    let mut assignments = vec![0usize; samples.len()];

    for _ in 0..KMEANS_ITERATIONS {
        let mut changed = false;
        let mut sums = vec![0.0f32; k];
        let mut counts = vec![0usize; k];

        // Assign each sample to the nearest centroid
        for (idx, sample) in samples.iter().enumerate() {
            let nearest = nearest_center_scalar(*sample, &centroids);
            if assignments[idx] != nearest {
                assignments[idx] = nearest;
                changed = true;
            }
            sums[nearest] += sample;
            counts[nearest] += 1;
        }

        // Update centroids to mean of assigned samples
        for i in 0..k {
            if counts[i] > 0 {
                centroids[i] = sums[i] / counts[i] as f32;
            }
        }

        if !changed {
            break;
        }
    }

    // Reinitialize empty tiers if any
    ensure_tier_coverage(samples, &mut centroids, &mut assignments);

    // Compute inertia and sort centroids to enforce ordering from fastest to slowest.
    let mut inertia = 0.0f32;
    for (sample, idx) in samples.iter().zip(assignments.iter()) {
        let diff = sample - centroids[*idx];
        inertia += diff * diff;
    }

    // Sort centroids and remap assignments to maintain tier ordering
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

/// Ensure all tiers have at least one node by redistributing empty centroids.
///
/// If any tier is empty after k-means, this reinitializes its centroid to an
/// evenly-spaced position in the latency distribution and reassigns samples.
fn ensure_tier_coverage(samples: &[f32], centroids: &mut [f32], assignments: &mut [usize]) {
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

    // Reinitialize empty tier centroids to evenly-spaced percentile positions
    for (tier_idx, count) in counts.iter_mut().enumerate() {
        if *count == 0 {
            let pos = ((tier_idx as f32 + 0.5) / k as f32 * (sorted_samples.len() - 1) as f32)
                .round() as usize;
            centroids[tier_idx] = sorted_samples[pos];
        }
    }

    // Reassign all samples to nearest centroid after redistribution
    for (sample_idx, sample) in samples.iter().enumerate() {
        let nearest = nearest_center_scalar(*sample, centroids);
        assignments[sample_idx] = nearest;
    }
}

/// Initialize k-means centroids using uniform percentile spacing.
///
/// Centroids are placed at evenly-spaced positions across the sorted sample
/// distribution to ensure good initial coverage.
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

/// Find the index of the nearest centroid to a given value.
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

// ─────────────────────────────────────────────────────────────────────────────
// Pressure Monitoring and Rate Limiting
// ─────────────────────────────────────────────────────────────────────────────

/// Monitors system pressure to prevent resource exhaustion.
///
/// Tracks memory usage, disk usage, and request rates to compute a composite
/// pressure score used for adaptive rate limiting and request rejection.
struct PressureMonitor {
    /// Current estimated memory usage in bytes.
    current_bytes: usize,
    /// Sliding window of recent request timestamps.
    requests: VecDeque<Instant>,
    /// Duration of the request rate window.
    request_window: Duration,
    /// Maximum requests allowed per window.
    request_limit: usize,
    /// Maximum disk storage in bytes.
    disk_limit: usize,
    /// Maximum memory usage in bytes.
    memory_limit: usize,
    /// Current composite pressure score (0.0 = no pressure, 1.0 = critical).
    current_pressure: f32,
}

impl PressureMonitor {
    /// Create a new pressure monitor with default limits.
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

    /// Record bytes added to storage.
    fn record_store(&mut self, bytes: usize) {
        self.current_bytes = self.current_bytes.saturating_add(bytes);
    }

    /// Record bytes removed from storage via eviction.
    fn record_evict(&mut self, bytes: usize) {
        self.current_bytes = self.current_bytes.saturating_sub(bytes);
    }

    /// Record a disk spill event, indicating critical pressure.
    fn record_spill(&mut self) {
        self.current_pressure = 1.0;
    }

    /// Record an incoming request for rate limiting.
    fn record_request(&mut self) {
        let now = Instant::now();
        self.requests.push_back(now);
        self.trim_requests(now);
    }

    /// Remove expired requests outside the rate limit window.
    fn trim_requests(&mut self, now: Instant) {
        while let Some(front) = self.requests.front() {
            if now.duration_since(*front) > self.request_window {
                self.requests.pop_front();
            } else {
                break;
            }
        }
    }

    /// Recompute the composite pressure score from disk, memory, and request metrics.
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

    /// Get the current pressure score.
    fn current_pressure(&self) -> f32 {
        self.current_pressure
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Local Key-Value Storage
// ─────────────────────────────────────────────────────────────────────────────

/// Maximum number of entries in the LRU cache.
/// This is a reasonable default; pressure-based eviction may kick in earlier.
const LOCAL_STORE_MAX_ENTRIES: usize = 100_000;

/// Local key-value store using LRU eviction with pressure-based adaptive behavior.
///
/// Uses an O(1) LRU cache for efficient storage operations and integrates with
/// pressure monitoring for adaptive eviction under resource constraints.
struct LocalStore {
    /// LRU cache providing O(1) get, put, and eviction operations.
    cache: LruCache<Key, Vec<u8>>,
    /// Pressure monitor for adaptive resource management.
    pressure: PressureMonitor,
}

impl LocalStore {
    /// Create a new local store with default capacity.
    fn new() -> Self {
        let cap = NonZeroUsize::new(LOCAL_STORE_MAX_ENTRIES).expect("capacity must be non-zero");
        Self {
            cache: LruCache::new(cap),
            pressure: PressureMonitor::new(),
        }
    }

    /// Override default pressure limits for testing or custom configurations.
    fn override_limits(&mut self, disk_limit: usize, memory_limit: usize, request_limit: usize) {
        self.pressure.disk_limit = disk_limit;
        self.pressure.memory_limit = memory_limit;
        self.pressure.request_limit = request_limit;
    }

    /// Record an incoming request for rate limiting purposes.
    fn record_request(&mut self) {
        self.pressure.record_request();
        let len = self.cache.len();
        self.pressure.update_pressure(len);
    }

    /// Store a key-value pair, performing pressure-based eviction if needed.
    ///
    /// Returns a list of key-value pairs that were evicted due to pressure.
    /// The LRU cache automatically handles capacity-based eviction.
    fn store(&mut self, key: Key, value: &[u8]) -> Vec<(Key, Vec<u8>)> {
        // If key exists, remove it first to update pressure accounting
        if let Some(existing) = self.cache.pop(&key) {
            self.pressure.record_evict(existing.len());
        }

        let data = value.to_vec();
        self.pressure.record_store(data.len());
        // put() is O(1) and automatically handles LRU eviction at capacity
        self.cache.put(key, data);
        self.pressure.update_pressure(self.cache.len());

        // Pressure-based eviction: evict LRU entries until pressure is acceptable
        let mut spilled = Vec::new();
        let mut spill_happened = false;
        while self.pressure.current_pressure() > PRESSURE_THRESHOLD {
            // pop_lru() is O(1)
            if let Some((evicted_key, evicted_val)) = self.cache.pop_lru() {
                self.pressure.record_evict(evicted_val.len());
                self.pressure.update_pressure(self.cache.len());
                spilled.push((evicted_key, evicted_val));
                spill_happened = true;
            } else {
                break;
            }
        }
        if spill_happened {
            self.pressure.record_spill();
        }
        spilled
    }

    /// Get a value by key, promoting it to most-recently-used in O(1) time.
    fn get(&mut self, key: &Key) -> Option<Vec<u8>> {
        // get() is O(1) and automatically promotes the key to most-recently-used
        self.cache.get(key).cloned()
    }

    /// Get the current pressure score.
    fn current_pressure(&self) -> f32 {
        self.pressure.current_pressure()
    }

    /// Get the current number of stored entries.
    fn len(&self) -> usize {
        self.cache.len()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Adaptive Parameters
// ─────────────────────────────────────────────────────────────────────────────

/// Adaptive parameters for DHT operations based on network conditions.
///
/// Dynamically adjusts k (bucket size) based on observed churn rate to maintain
/// routing table health in varying network conditions.
struct AdaptiveParams {
    /// Current k parameter (bucket size), ranges from 10 to 30.
    k: usize,
    /// Parallelism factor for lookups.
    alpha: usize,
    /// Sliding window of recent churn observations (true = success, false = failure).
    churn_history: VecDeque<bool>,
}

impl AdaptiveParams {
    /// Create new adaptive parameters with initial k and alpha values.
    fn new(k: usize, alpha: usize) -> Self {
        Self {
            k,
            alpha,
            churn_history: VecDeque::new(),
        }
    }

    /// Record a churn observation and update k if needed.
    ///
    /// Returns true if k was changed.
    fn record_churn(&mut self, success: bool) -> bool {
        self.churn_history.push_back(success);
        if self.churn_history.len() > QUERY_STATS_WINDOW {
            self.churn_history.pop_front();
        }
        let old_k = self.k;
        self.update_k();
        old_k != self.k
    }

    /// Recompute k based on observed churn rate.
    ///
    /// Higher churn rates result in larger k values to maintain routing table resilience.
    /// k ranges from 10 (low churn) to 30 (high churn).
    fn update_k(&mut self) {
        if self.churn_history.is_empty() {
            return;
        }
        let failures = self.churn_history.iter().filter(|entry| !**entry).count();
        let churn_rate = failures as f32 / self.churn_history.len() as f32;
        let new_k = (10.0 + (20.0 * churn_rate).round()).clamp(10.0, 30.0);
        self.k = new_k as usize;
    }

    /// Get the current k parameter.
    fn current_k(&self) -> usize {
        self.k
    }

    /// Get the current alpha (parallelism) parameter.
    fn current_alpha(&self) -> usize {
        self.alpha
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Telemetry and Diagnostics
// ─────────────────────────────────────────────────────────────────────────────

/// Snapshot of current DHT node state for telemetry and debugging.
#[derive(Clone, Debug, Default)]
pub struct TelemetrySnapshot {
    /// Current latency tier centroids in milliseconds.
    pub tier_centroids: Vec<f32>,
    /// Number of nodes in each tier.
    pub tier_counts: Vec<usize>,
    /// Current resource pressure (0.0 to 1.0).
    pub pressure: f32,
    /// Number of key-value pairs in local storage.
    pub stored_keys: usize,
    /// Current replication factor (k parameter).
    pub replication_factor: usize,
    /// Current lookup concurrency (alpha parameter).
    pub concurrency: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Routing Table
// ─────────────────────────────────────────────────────────────────────────────

/// Find the bucket index for a node ID relative to self.
///
/// Uses the XOR distance to determine which bucket a node belongs to.
/// The bucket index is the position of the first differing bit (0..=255).
/// Bucket 0 is the furthest (most different), bucket 255 is the closest.
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

/// Represents another DHT node with its ID and serialized endpoint address.
///
/// The address is stored as a JSON-serialized iroh EndpointAddr for transport flexibility.
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Contact {
    /// The node's unique identifier (BLAKE3 hash of public key).
    pub id: NodeId,
    /// JSON-serialized iroh EndpointAddr for connecting to this node.
    pub addr: String,
}

/// A single Kademlia routing bucket with LRU-like behavior.
///
/// Maintains up to k contacts, preferring long-lived nodes (older contacts)
/// over newly discovered ones to improve routing stability.
#[derive(Debug, Default, Clone)]
struct Bucket {
    /// Contacts in LRU order (oldest first, newest last).
    contacts: Vec<Contact>,
}

/// Outcome of attempting to add or refresh a contact in a bucket.
#[derive(Debug)]
enum BucketTouchOutcome {
    /// Contact was newly inserted (bucket had space).
    Inserted,
    /// Existing contact was refreshed (moved to end of LRU queue).
    Refreshed,
    /// Bucket is full; includes the oldest contact for potential eviction.
    Full {
        new_contact: Contact,
        oldest: Contact,
    },
}

/// Pending bucket update when a bucket is full and oldest contact needs ping check.
#[derive(Clone, Debug)]
struct PendingBucketUpdate {
    bucket_index: usize,
    oldest: Contact,
    new_contact: Contact,
}

impl Bucket {
    /// Create a new empty bucket.
    fn new() -> Self {
        Self {
            contacts: Vec::new(),
        }
    }

    /// Attempt to add or refresh a contact in the bucket.
    ///
    /// - If contact exists, moves it to end (most recently seen)
    /// - If bucket has space, inserts the contact
    /// - If bucket is full, returns the oldest contact for potential eviction
    fn touch(&mut self, contact: Contact, k: usize) -> BucketTouchOutcome {
        if let Some(pos) = self.contacts.iter().position(|c| c.id == contact.id) {
            let existing = self.contacts.remove(pos);
            self.contacts.push(existing);
            return BucketTouchOutcome::Refreshed;
        }

        if self.contacts.len() < k {
            self.contacts.push(contact);
            BucketTouchOutcome::Inserted
        } else {
            let oldest = self
                .contacts
                .first()
                .cloned()
                .expect("bucket cannot be empty when full");
            BucketTouchOutcome::Full {
                new_contact: contact,
                oldest,
            }
        }
    }

    /// Refresh a contact by moving it to the end of the LRU queue.
    ///
    /// Returns true if the contact was found and refreshed.
    fn refresh(&mut self, id: &NodeId) -> bool {
        if let Some(pos) = self.contacts.iter().position(|c| &c.id == id) {
            let existing = self.contacts.remove(pos);
            self.contacts.push(existing);
            true
        } else {
            false
        }
    }

    /// Remove a contact from the bucket.
    ///
    /// Returns true if the contact was found and removed.
    fn remove(&mut self, id: &NodeId) -> bool {
        if let Some(pos) = self.contacts.iter().position(|c| &c.id == id) {
            self.contacts.remove(pos);
            true
        } else {
            false
        }
    }
}

/// Kademlia routing table with 256 buckets for 256-bit node IDs.
///
/// Each bucket stores up to k contacts at a specific XOR distance from the local node.
/// Buckets use LRU-like behavior, preferring long-lived nodes for stability.
#[derive(Debug)]
pub struct RoutingTable {
    /// This node's ID.
    self_id: NodeId,
    /// Maximum contacts per bucket (adaptive k parameter).
    k: usize,
    /// 256 buckets, one for each bit position of the XOR distance.
    buckets: Vec<Bucket>,
}

impl RoutingTable {
    /// Create a new routing table for the given node ID.
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

    /// Update the k parameter, trimming buckets if they exceed the new limit.
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

    /// Add or update a contact in the routing table.
    pub fn update(&mut self, contact: Contact) {
        let _ = self.update_with_pending(contact);
    }

    /// Add or update a contact, returning pending update info if bucket is full.
    ///
    /// When a bucket is full and a new contact is seen, this returns info
    /// about the oldest contact so the caller can ping it to decide whether
    /// to evict it or discard the new contact.
    fn update_with_pending(&mut self, contact: Contact) -> Option<PendingBucketUpdate> {
        if contact.id == self.self_id {
            return None;
        }
        let idx = bucket_index(&self.self_id, &contact.id);
        match self.buckets[idx].touch(contact, self.k) {
            BucketTouchOutcome::Inserted | BucketTouchOutcome::Refreshed => None,
            BucketTouchOutcome::Full {
                new_contact,
                oldest,
            } => Some(PendingBucketUpdate {
                bucket_index: idx,
                oldest,
                new_contact,
            }),
        }
    }

    /// Find the k closest contacts to a target node ID.
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

    /// Apply the result of pinging the oldest contact in a full bucket.
    ///
    /// If the oldest contact is still alive, it is refreshed (moved to end).
    /// If the oldest is dead, it is removed and the new contact is inserted.
    fn apply_ping_result(&mut self, pending: PendingBucketUpdate, oldest_alive: bool) {
        let bucket = &mut self.buckets[pending.bucket_index];
        if oldest_alive {
            bucket.refresh(&pending.oldest.id);
            return;
        }

        let _ = bucket.remove(&pending.oldest.id);
        let already_present = bucket
            .contacts
            .iter()
            .any(|contact| contact.id == pending.new_contact.id);
        if already_present {
            return;
        }
        if bucket.contacts.len() < self.k {
            bucket.contacts.push(pending.new_contact);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Network Trait
// ─────────────────────────────────────────────────────────────────────────────

/// Network abstraction for DHT RPC operations.
///
/// This trait abstracts the transport layer, allowing the core DHT logic to work
/// with different network implementations (e.g., iroh QUIC, mock for testing).
#[async_trait]
pub trait DhtNetwork: Send + Sync + 'static {
    /// Send a FIND_NODE RPC to find contacts near a target ID.
    async fn find_node(&self, to: &Contact, target: NodeId) -> Result<Vec<Contact>>;

    /// Send a FIND_VALUE RPC to retrieve a value or get closer contacts.
    ///
    /// Returns (value, closer_nodes) where value is Some if the key was found.
    async fn find_value(&self, to: &Contact, key: Key) -> Result<(Option<Vec<u8>>, Vec<Contact>)>;

    /// Send a STORE RPC to store a key-value pair on a node.
    async fn store(&self, to: &Contact, key: Key, value: Vec<u8>) -> Result<()>;

    /// Ping a contact to check if it's still responsive.
    ///
    /// Used for the Kademlia "ping-before-evict" rule: when a bucket is full,
    /// the oldest contact is pinged to verify it's still alive before deciding
    /// whether to keep it or replace it with the new contact.
    async fn ping(&self, to: &Contact) -> Result<()>;
}

// ─────────────────────────────────────────────────────────────────────────────
// DHT Node
// ─────────────────────────────────────────────────────────────────────────────

/// High-level DHT node implementing adaptive Kademlia.
///
/// A `DhtNode` owns a routing table, a content-addressable store, and the
/// [`DhtNetwork`] transport used to send RPCs to other peers. The type is
/// generic over the network layer so tests can use an in-memory mock while
/// production uses [`crate::net::IrohNetwork`].
///
/// # Key Methods
///
/// * [`observe_contact`](Self::observe_contact) - Update routing table when peers are discovered
/// * [`iterative_find_node`](Self::iterative_find_node) - Perform iterative lookup with adaptive tuning
/// * [`put`](Self::put) - Store a key-value pair with replication
/// * [`handle_find_node_request`](Self::handle_find_node_request) - Handle incoming FIND_NODE RPC
/// * [`handle_find_value_request`](Self::handle_find_value_request) - Handle incoming FIND_VALUE RPC
/// * [`handle_store_request`](Self::handle_store_request) - Handle incoming STORE RPC
///
/// The node is `Arc`-friendly and can be shared between background tasks.
pub struct DhtNode<N: DhtNetwork> {
    /// This node's unique identifier.
    pub id: NodeId,
    /// Contact info for this node (ID + serialized address).
    pub self_contact: Contact,
    /// Kademlia routing table with 256 buckets.
    routing: Arc<Mutex<RoutingTable>>,
    /// Local key-value storage with LRU eviction.
    store: Arc<Mutex<LocalStore>>,
    /// Network transport for sending RPCs.
    network: Arc<N>,
    /// Adaptive parameters (k, alpha) tuned based on network conditions.
    params: Arc<Mutex<AdaptiveParams>>,
    /// Latency-based tiering for prioritizing fast peers.
    tiering: Arc<Mutex<TieringManager>>,
}

impl<N: DhtNetwork> DhtNode<N> {
    /// Create a new DHT node with the given ID, contact info, network, and initial parameters.
    pub fn new(id: NodeId, self_contact: Contact, network: N, k: usize, alpha: usize) -> Self {
        Self {
            id,
            self_contact,
            routing: Arc::new(Mutex::new(RoutingTable::new(id, k))),
            store: Arc::new(Mutex::new(LocalStore::new())),
            network: Arc::new(network),
            params: Arc::new(Mutex::new(AdaptiveParams::new(k, alpha))),
            tiering: Arc::new(Mutex::new(TieringManager::new())),
        }
    }

    /// Observe a contact and update the routing table.
    ///
    /// If the bucket for this contact is full, spawns a background task to ping
    /// the oldest contact and decide whether to evict it.
    pub async fn observe_contact(&self, contact: Contact) {
        if contact.id == self.id {
            return;
        }
        {
            let mut tiering = self.tiering.lock().await;
            tiering.register_contact(&contact.id);
        }
        let k = {
            let params = self.params.lock().await;
            params.current_k()
        };
        let pending = {
            let mut rt = self.routing.lock().await;
            rt.set_k(k);
            rt.update_with_pending(contact)
        };
        if let Some(update) = pending {
            self.spawn_bucket_refresh(update);
        }
    }

    /// Store a key-value pair locally with content verification.
    ///
    /// Verifies that the key matches the BLAKE3 hash of the value before storing.
    /// May trigger pressure-based eviction, offloading spilled entries.
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

    /// Spawn a background task to ping the oldest contact in a full bucket.
    ///
    /// This implements the Kademlia "ping-before-evict" rule.
    fn spawn_bucket_refresh(&self, pending: PendingBucketUpdate) {
        let network = self.network.clone();
        let routing = self.routing.clone();
        tokio::spawn(async move {
            let alive = match network.ping(&pending.oldest).await {
                Ok(_) => true,
                Err(err) => {
                    debug!(
                        peer = ?pending.oldest.id,
                        addr = %pending.oldest.addr,
                        "ping failed: {err:?}"
                    );
                    false
                }
            };
            let mut rt = routing.lock().await;
            rt.apply_ping_result(pending, alive);
        });
    }

    /// Retrieve a value from local storage.
    async fn get_local(&self, key: &Key) -> Option<Vec<u8>> {
        let mut store = self.store.lock().await;
        store.record_request();
        store.get(key)
    }

    /// Override pressure limits for testing or custom configurations.
    async fn override_pressure_limits(
        &self,
        disk_limit: usize,
        memory_limit: usize,
        request_limit: usize,
    ) {
        let mut store = self.store.lock().await;
        store.override_limits(disk_limit, memory_limit, request_limit);
    }

    /// Handle an incoming FIND_NODE RPC request.
    ///
    /// Returns the k closest contacts to the target ID from our routing table.
    pub async fn handle_find_node_request(&self, from: &Contact, target: NodeId) -> Vec<Contact> {
        self.observe_contact(from.clone()).await;
        let k = {
            let params = self.params.lock().await;
            params.current_k()
        };
        let rt = self.routing.lock().await;
        rt.closest(&target, k)
    }

    /// Handle an incoming FIND_VALUE RPC request.
    ///
    /// If we have the value locally, returns it. Otherwise, returns the k closest
    /// contacts to the key for the requester to continue the lookup.
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

    /// Handle an incoming STORE RPC request.
    ///
    /// Verifies the key-value pair and stores it locally if valid.
    pub async fn handle_store_request(&self, from: &Contact, key: Key, value: Vec<u8>) {
        self.observe_contact(from.clone()).await;
        self.store_local(key, value).await;
    }

    /// Check if a node matches the given tier level filter.
    async fn level_matches(&self, node: &NodeId, level_filter: Option<TieringLevel>) -> bool {
        if let Some(level) = level_filter {
            let tiering = self.tiering.lock().await;
            tiering.level_for(node) == level
        } else {
            true
        }
    }

    /// Filter contacts to only those in the specified tier level.
    async fn filter_contacts(
        &self,
        contacts: Vec<Contact>,
        level_filter: Option<TieringLevel>,
    ) -> Vec<Contact> {
        if level_filter.is_none() {
            return contacts;
        }
        let level = level_filter.unwrap();
        let tiering = self.tiering.lock().await;
        contacts
            .into_iter()
            .filter(|c| tiering.level_for(&c.id) == level)
            .collect()
    }

    /// Record an RTT sample for a contact for latency tiering.
    async fn record_rtt(&self, contact: &Contact, elapsed: Duration) {
        if contact.id == self.id {
            return;
        }
        let rtt_ms = (elapsed.as_secs_f64() * 1000.0) as f32;
        let mut tiering = self.tiering.lock().await;
        tiering.record_sample(&contact.id, rtt_ms);
    }

    /// Record a churn observation and adjust k if needed.
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

    /// Get the current k parameter.
    async fn current_k(&self) -> usize {
        let params = self.params.lock().await;
        params.current_k()
    }

    /// Get the current alpha (parallelism) parameter.
    async fn current_alpha(&self) -> usize {
        let params = self.params.lock().await;
        params.current_alpha()
    }

    /// Perform an iterative FIND_NODE lookup for the target ID.
    ///
    /// Returns the k closest contacts to the target found during the lookup.
    /// Automatically adjusts k based on observed churn.
    pub async fn iterative_find_node(&self, target: NodeId) -> Result<Vec<Contact>> {
        self.iterative_find_node_with_level(target, None).await
    }

    /// Perform an iterative FIND_NODE lookup with optional tier filtering.
    ///
    /// The lookup process:
    /// 1. Start with k closest contacts from routing table
    /// 2. Query alpha contacts in parallel, collect responses
    /// 3. Add newly discovered contacts to shortlist
    /// 4. Repeat until no closer contacts are found
    /// 5. Return the k closest contacts found
    async fn iterative_find_node_with_level(
        &self,
        target: NodeId,
        level_filter: Option<TieringLevel>,
    ) -> Result<Vec<Contact>> {
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut queried: HashSet<NodeId> = HashSet::new();
        let mut rpc_success = false;
        let mut rpc_failure = false;
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
            // Select up to alpha unqueried candidates
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

                if contact.id == self.id {
                    continue;
                }

                // Query the contact and measure RTT
                let start = Instant::now();
                let response = match self.network.find_node(&contact, target).await {
                    Ok(nodes) => {
                        rpc_success = true;
                        let elapsed = start.elapsed();
                        self.record_rtt(&contact, elapsed).await;
                        self.observe_contact(contact.clone()).await;
                        for n in &nodes {
                            self.observe_contact(n.clone()).await;
                        }
                        nodes
                    }
                    Err(_) => {
                        rpc_failure = true;
                        continue;
                    }
                };

                // Add new contacts to shortlist
                for n in response {
                    if seen.insert(n.id) && self.level_matches(&n.id, level_filter).await {
                        shortlist.push(n.clone());
                    }
                }

                // Re-sort shortlist by distance to target
                shortlist.sort_by(|a, b| {
                    let da = xor_distance(&a.id, &target);
                    let db = xor_distance(&b.id, &target);
                    distance_cmp(&da, &db)
                });

                // Truncate to k closest
                let k = self.current_k().await;
                if shortlist.len() > k {
                    shortlist.truncate(k);
                }

                // Check if we found any closer contacts
                if let Some(first) = shortlist.first() {
                    let new_best = xor_distance(&first.id, &target);
                    if distance_cmp(&new_best, &best_distance) == std::cmp::Ordering::Less {
                        best_distance = new_best;
                        any_closer = true;
                    }
                }
            }

            // Stop if no progress was made
            if !any_closer {
                break;
            }
        }

        // Adjust k based on lookup success/failure
        if rpc_success {
            self.adjust_k(true).await;
        } else if rpc_failure {
            self.adjust_k(false).await;
        }

        Ok(shortlist)
    }

    /// Offload spilled entries to slower-tier nodes.
    ///
    /// When local storage is under pressure, evicted entries are replicated
    /// to nodes in the slowest tier to preserve data availability.
    async fn offload_spilled(&self, spilled: Vec<(Key, Vec<u8>)>) {
        if spilled.is_empty() {
            return;
        }

        let target_level = {
            let tiering = self.tiering.lock().await;
            tiering.slowest_level()
        };

        for (key, value) in spilled {
            self.replicate_to_level(key, value.clone(), target_level)
                .await;
        }
    }

    /// Replicate a key-value pair to nodes in a specific tier.
    async fn replicate_to_level(&self, key: Key, value: Vec<u8>, level: TieringLevel) {
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

    /// Send a STORE RPC to a contact and record metrics.
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

    /// Store a value in the DHT with distance-based replication.
    ///
    /// The key is derived from the BLAKE3 hash of the value (content-addressed).
    /// The value is stored locally and replicated to the k closest nodes.
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

    /// Get a snapshot of current node state for telemetry.
    /// Get a snapshot of current node state for telemetry.
    pub async fn telemetry_snapshot(&self) -> TelemetrySnapshot {
        let tiering_stats = {
            let tiering = self.tiering.lock().await;
            tiering.stats()
        };
        let (pressure, stored_keys) = {
            let store = self.store.lock().await;
            (store.current_pressure(), store.len())
        };
        let params = self.params.lock().await;
        TelemetrySnapshot {
            tier_centroids: tiering_stats.centroids,
            tier_counts: tiering_stats.counts,
            pressure,
            stored_keys,
            replication_factor: params.current_k(),
            concurrency: params.current_alpha(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Discovery Node (Public API)
// ─────────────────────────────────────────────────────────────────────────────

/// Public-facing DHT node restricted to peer discovery workflows.
///
/// Applications use `DiscoveryNode` instead of `DhtNode` directly, making it
/// explicit that the API is limited to routing and lookup functionality.
/// Storage-oriented APIs (`put`, internal store methods) remain internal so
/// the sDHT cannot be misused as a general key-value store.
///
/// # Example
///
/// ```ignore
/// let node = DiscoveryNode::new(id, contact, network, K_DEFAULT, ALPHA_DEFAULT);
/// node.observe_contact(peer_contact).await;
/// let closest = node.iterative_find_node(target_id).await?;
/// ```
pub struct DiscoveryNode<N: DhtNetwork> {
    inner: Arc<DhtNode<N>>,
}

impl<N: DhtNetwork> Clone for DiscoveryNode<N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<N: DhtNetwork> DiscoveryNode<N> {
    /// Create a new discovery node with the given parameters.
    pub fn new(id: NodeId, self_contact: Contact, network: N, k: usize, alpha: usize) -> Self {
        Self {
            inner: Arc::new(DhtNode::new(id, self_contact, network, k, alpha)),
        }
    }

    /// Get this node's contact information.
    pub fn contact(&self) -> Contact {
        self.inner.self_contact.clone()
    }

    /// Get this node's unique identifier.
    pub fn node_id(&self) -> NodeId {
        self.inner.id
    }

    /// Observe a contact and update the routing table.
    pub async fn observe_contact(&self, contact: Contact) {
        self.inner.observe_contact(contact).await;
    }

    /// Perform an iterative lookup to find the k closest nodes to a target.
    pub async fn iterative_find_node(&self, target: NodeId) -> Result<Vec<Contact>> {
        self.inner.iterative_find_node(target).await
    }

    /// Get a snapshot of current node state for telemetry.
    pub async fn telemetry_snapshot(&self) -> TelemetrySnapshot {
        self.inner.telemetry_snapshot().await
    }

    /// Handle an incoming FIND_NODE RPC request.
    pub async fn handle_find_node_request(&self, from: &Contact, target: NodeId) -> Vec<Contact> {
        self.inner.handle_find_node_request(from, target).await
    }

    /// Handle an incoming FIND_VALUE RPC request.
    pub async fn handle_find_value_request(
        &self,
        from: &Contact,
        key: Key,
    ) -> (Option<Vec<u8>>, Vec<Contact>) {
        self.inner.handle_find_value_request(from, key).await
    }

    /// Handle an incoming STORE RPC request.
    pub async fn handle_store_request(&self, from: &Contact, key: Key, value: Vec<u8>) {
        self.inner.handle_store_request(from, key, value).await;
    }

    /// Override pressure limits for testing or custom configurations.
    ///
    /// Primarily used for large-scale simulations and integration tests
    /// that need to disable pressure-based spilling.
    pub async fn override_pressure_limits(
        &self,
        disk_limit: usize,
        memory_limit: usize,
        request_limit: usize,
    ) {
        self.inner
            .override_pressure_limits(disk_limit, memory_limit, request_limit)
            .await;
    }

    /// Store a value in the DHT, returning the content hash key.
    ///
    /// This exposes the same distance-based replication logic used internally
    /// so large-scale integration tests and diagnostics can probe how data
    /// placement behaves without needing access to private fields.
    pub async fn put(&self, value: Vec<u8>) -> Result<Key> {
        self.inner.put(value).await
    }
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
        assert_ne!(
            hash_one, different_hash,
            "hashes of different data should differ"
        );
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
    fn derive_node_id_matches_blake3_reference() {
        let data = b"public key bytes";
        let expected = iroh_blake3::hash(data);
        let mut expected_bytes = [0u8; 32];
        expected_bytes.copy_from_slice(expected.as_bytes());

        assert_eq!(
            derive_node_id(data),
            expected_bytes,
            "derive_node_id should produce the BLAKE3 digest"
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
