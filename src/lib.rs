//! # Iroh sDHT
//!
//! This crate exposes a lightweight experimental distributed hash table (DHT)
//! built on top of the [`iroh`](https://crates.io/crates/iroh) transport stack.
//! The implementation combines a Kademlia-inspired routing table with adaptive
//! tiering and backpressure controls so that it can be embedded in services
//! that need a self-healing peer-to-peer key/value store.
//!
//! The crate is split into a handful of modules that can be reused
//! independently:
//!
//! - [`core`]: the transport-agnostic Kademlia logic, including the routing
//!   table, local storage engine, and [`DhtNode`] state machine.
//! - [`net`]: an [`iroh`] based [`DhtNetwork`] implementation that knows how to
//!   exchange RPC messages over QUIC.
//! - [`protocol`]: the JSON serialisable wire messages exchanged between peers.
//! - [`framing`]: helpers for length-prefixed frames so RPCs can be multiplexed
//!   over a single stream.
//! - [`server`]: utilities for hosting an RPC server on top of the network
//!   transport.
//!
//! ## Getting started
//!
//! The simplest way to embed the DHT is to construct an [`IrohNetwork`], build a
//! [`DhtNode`] with the desired replication factor (`k`) and concurrency (`α`),
//! and then drive the async methods from your application:
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use anyhow::Result;
//! use iroh::net::{MagicEndpoint, NodeAddr};
//! use iroh_sdht::{Contact, DhtNode, IrohNetwork, DHT_ALPN};
//!
//! # async fn launch(endpoint: MagicEndpoint, addr: NodeAddr) -> Result<()> {
//! let self_id = iroh_sdht::derive_node_id(endpoint.node_id().as_bytes());
//! let self_contact = Contact {
//!     id: self_id,
//!     addr: serde_json::to_string(&addr)?,
//! };
//! let network = IrohNetwork {
//!     endpoint,
//!     self_contact: self_contact.clone(),
//! };
//! let node = Arc::new(DhtNode::new(self_id, self_contact, network, 20, 3));
//! // The node can now observe peers and perform lookups.
//! # let _ = node.iterative_find_node(self_id).await?;
//! # Ok(())
//! # }
//! ```
//!
//! The binary in `src/main.rs` demonstrates how to wire these pieces together to
//! run a self-contained DHT node that discovers peers via mDNS with relay
//! fallback.

pub mod core;
pub mod framing;
pub mod net;
pub mod protocol;
pub mod server;

pub use core::{
    derive_node_id, hash_content, verify_key_value_pair, Contact, DhtNetwork, DhtNode, Key, NodeId,
};
pub use net::{IrohNetwork, DHT_ALPN};
pub use server::handle_connection;
