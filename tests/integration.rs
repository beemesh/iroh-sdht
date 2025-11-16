#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use common::{make_contact, make_node_id, NetworkRegistry, TestNode};
use iroh_sdht::hash_content;
use tokio::time::Duration;

#[tokio::test]
async fn iterative_find_node_returns_expected_contacts() {
    let registry = Arc::new(NetworkRegistry::default());
    let main = TestNode::new(registry.clone(), 0x10, 20, 3).await;
    let peer_one = TestNode::new(registry.clone(), 0x11, 20, 3).await;
    let peer_two = TestNode::new(registry.clone(), 0x12, 20, 3).await;

    for peer in [&peer_one, &peer_two] {
        main.node.observe_contact(peer.contact()).await;
        peer.node.observe_contact(main.contact()).await;
    }

    let target = peer_two.contact().id;
    let results = main
        .node
        .iterative_find_node(target)
        .await
        .expect("lookup succeeds");

    assert_eq!(results.first().map(|c| c.id), Some(peer_two.contact().id));
    assert!(results.iter().any(|c| c.id == peer_one.contact().id));
}

#[tokio::test]
async fn adaptive_k_tracks_network_successes_and_failures() {
    let registry = Arc::new(NetworkRegistry::default());
    let main = TestNode::new(registry.clone(), 0x30, 10, 3).await;
    let peer = TestNode::new(registry.clone(), 0x31, 10, 3).await;

    main.node.observe_contact(peer.contact()).await;
    peer.node.observe_contact(main.contact()).await;

    main.network.set_failure(peer.contact().id, true).await;
    let target = make_node_id(0xAA);
    let _ = main
        .node
        .iterative_find_node(target)
        .await
        .expect("lookup tolerates failure");
    let snapshot = main.node.telemetry_snapshot().await;
    assert_eq!(snapshot.replication_factor, 30);

    main.network.set_failure(peer.contact().id, false).await;
    let _ = main
        .node
        .iterative_find_node(target)
        .await
        .expect("lookup succeeds after recovery");
    let snapshot = main.node.telemetry_snapshot().await;
    assert_eq!(snapshot.replication_factor, 20);
}

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

#[tokio::test]
async fn tiering_clusters_contacts_by_latency() {
    let registry = Arc::new(NetworkRegistry::default());
    let main = TestNode::new(registry.clone(), 0x01, 20, 3).await;
    let fast = TestNode::new(registry.clone(), 0x02, 20, 3).await;
    let medium = TestNode::new(registry.clone(), 0x03, 20, 3).await;
    let slow = TestNode::new(registry.clone(), 0x04, 20, 3).await;

    for peer in [&fast, &medium, &slow] {
        main.node.observe_contact(peer.contact()).await;
        peer.node.observe_contact(main.contact()).await;
    }

    main.network
        .set_latency(fast.contact().id, Duration::from_millis(5))
        .await;
    main.network
        .set_latency(medium.contact().id, Duration::from_millis(25))
        .await;
    main.network
        .set_latency(slow.contact().id, Duration::from_millis(50))
        .await;

    let target = make_node_id(0x99);
    let _ = main
        .node
        .iterative_find_node(target)
        .await
        .expect("lookup succeeds");

    let snapshot = main.node.telemetry_snapshot().await;
    assert!(snapshot.tier_centroids.len() >= 2);
    assert_eq!(snapshot.tier_counts.iter().sum::<usize>(), 3);
    assert!(snapshot.tier_centroids.first().unwrap() < snapshot.tier_centroids.last().unwrap());
}
