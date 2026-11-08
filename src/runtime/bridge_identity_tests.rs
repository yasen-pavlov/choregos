//! The bridge drawer's node and boot follow only statuses the aether adapter accepted.
use super::loop_tests::{Harness, wait_snapshot};

const STATUS: &str = "aether/cec-bridge/status";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aether_bridge_identity_tracks_accepted_status_and_clears_on_offline() {
    let mut harness = Harness::start_with_bridge(true).await;
    let identity = |boot| (Some("cec-bridge".to_owned()), Some(boot));

    for boot in [7_u32, 8] {
        harness
            .peer
            .publish(
                STATUS,
                true,
                format!(r#"{{"online":true,"v":1,"boot":{boot},"bus":"ok"}}"#),
            )
            .await;
        let snapshot = wait_snapshot(&harness.state, |s| {
            s.bridge_boot == Some(boot) && s.bridge_online
        })
        .await;
        assert_eq!((snapshot.bridge_node, snapshot.bridge_boot), identity(boot));
    }

    // Rejected statuses while online: neither may name a boot. The valid same-boot status
    // after them (same topic, so delivered in order) proves both were processed.
    for rejected in [
        r#"{"online":true,"v":1,"boot":999,"bus":42}"#,
        r#"{"boot":999}"#,
    ] {
        harness.peer.publish(STATUS, true, rejected).await;
    }
    harness
        .peer
        .publish(
            STATUS,
            true,
            r#"{"online":true,"v":1,"boot":8,"bus":"stuck_low"}"#,
        )
        .await;
    let snapshot = wait_snapshot(&harness.state, |s| {
        s.bridge_bus.as_deref() == Some("stuck_low")
    })
    .await;
    assert!(snapshot.bridge_online);
    assert_eq!((snapshot.bridge_node, snapshot.bridge_boot), identity(8));

    harness
        .peer
        .publish(STATUS, true, r#"{"online":false}"#)
        .await;
    let snapshot = wait_snapshot(&harness.state, |s| !s.bridge_online).await;
    assert_eq!((snapshot.bridge_node, snapshot.bridge_boot), (None, None));

    // The same board coming back without rebooting is named again.
    harness
        .peer
        .publish(STATUS, true, r#"{"online":true,"v":1,"boot":8,"bus":"ok"}"#)
        .await;
    let snapshot = wait_snapshot(&harness.state, |s| s.bridge_online).await;
    assert_eq!((snapshot.bridge_node, snapshot.bridge_boot), identity(8));

    harness.broker.shutdown().await;
    let snapshot = wait_snapshot(&harness.state, |s| {
        !s.connections.mqtt_connected && !s.bridge_online
    })
    .await;
    assert_eq!((snapshot.bridge_node, snapshot.bridge_boot), (None, None));
    harness.shutdown().await;
}
