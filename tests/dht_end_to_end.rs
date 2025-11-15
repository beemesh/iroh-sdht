#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use common::{make_node_id, NetworkRegistry, TestNode};
use iroh_sdht::hash_content;

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
async fn iterative_find_value_fetches_remote_payload() {
    let registry = Arc::new(NetworkRegistry::default());
    let main = TestNode::new(registry.clone(), 0x20, 20, 3).await;
    let holder = TestNode::new(registry.clone(), 0x21, 20, 3).await;

    main.node.observe_contact(holder.contact()).await;
    holder.node.observe_contact(main.contact()).await;

    let value = b"hello routing".to_vec();
    let key = hash_content(&value);
    holder
        .node
        .handle_store_request(&main.contact(), key, value.clone())
        .await;

    let (result, _closer) = main
        .node
        .iterative_find_value(key)
        .await
        .expect("value lookup succeeds");
    assert_eq!(result, Some(value));
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
async fn put_and_get_flow_stores_and_retrieves_content() {
    let registry = Arc::new(NetworkRegistry::default());
    let writer = TestNode::new(registry.clone(), 0x40, 20, 3).await;
    let peer = TestNode::new(registry.clone(), 0x41, 20, 3).await;

    writer.node.observe_contact(peer.contact()).await;
    peer.node.observe_contact(writer.contact()).await;

    let payload = b"end-to-end test".to_vec();
    let key = writer
        .node
        .put(payload.clone())
        .await
        .expect("put succeeds");

    let reader = TestNode::new(registry.clone(), 0x42, 20, 3).await;
    reader.node.observe_contact(peer.contact()).await;
    reader.node.observe_contact(writer.contact()).await;

    let retrieved = reader.node.get(key).await.expect("get runs");
    assert_eq!(retrieved, Some(payload.clone()));

    let (value_at_peer, _closer) = peer
        .node
        .handle_find_value_request(&writer.contact(), key)
        .await;
    assert_eq!(value_at_peer, Some(payload));
}
