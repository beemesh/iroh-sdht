#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use common::{make_contact, NetworkRegistry, TestNode};
use iroh_sdht::hash_content;

#[tokio::test]
async fn backpressure_spills_large_values_and_records_pressure() {
    let registry = Arc::new(NetworkRegistry::default());
    let node = TestNode::new(registry.clone(), 0x01, 20, 3).await;

    let peer = make_contact(0x02);
    let value = vec![42u8; 12 * 1024 * 1024];
    let key = hash_content(&value);

    node.node
        .handle_store_request(&peer, key, value.clone())
        .await;

    let snapshot = node.node.telemetry_snapshot().await;
    assert!(snapshot.pressure >= 0.99);
    assert_eq!(snapshot.stored_keys, 0);

    let calls = node.network.store_calls().await;
    assert!(!calls.is_empty());
    let (contact, stored_key, len) = &calls[0];
    assert_eq!(contact.id, peer.id);
    assert_eq!(*stored_key, key);
    assert_eq!(*len, value.len());
}
