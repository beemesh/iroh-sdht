#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use common::{make_node_id, NetworkRegistry, TestNode};
use tokio::time::Duration;

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
