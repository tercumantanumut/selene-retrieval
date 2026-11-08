//! Protocol version skew in both directions (architect review D1):
//! older client → `Unavailable{upgrade}` with plain text; N-1 accepted; newer client →
//! drain-restart of the old daemon and attach to the new one.
// Test code: panicking on unexpected errors is the point. Each #[tokio::test] owns its runtime,
// so holding the cross-test serial lock across awaits cannot deadlock.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]

mod common;

use std::time::Duration;

use common::*;
use sr_client::{AppClient, AttachPath, attach};
use sr_common::error::ErrorCode;
use sr_common::protocol::{ConnKind, PROTO_VERSION};

const T: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn older_clients_n_minus_1_accepted_older_refused_with_plain_text() {
    let _s = serial();
    let env = TestEnv::new("skew-old");
    let pid = env.start_daemon(&["--proto", "3"]);

    // N and N-1 are accepted.
    for p in [3u16, 2] {
        let mut o = env.opts(ConnKind::App);
        o.proto = p;
        let c = AppClient::connect(&o)
            .await
            .unwrap_or_else(|e| panic!("proto {p}: {e}"));
        assert_eq!(c.daemon_proto, 3);
        assert_eq!(c.daemon_pid, pid);
    }
    // N-2 is refused: UpgradeRequired, not retried, daemon untouched.
    let mut o = env.opts(ConnKind::App);
    o.proto = 1;
    o.client_id = "claude".into();
    let e = attach(&o).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::UpgradeRequired);
    let m = e.user_message();
    assert_eq!(
        m.body,
        "Selene Retrieval was updated. Restart Claude to finish."
    );

    // The same through a real bridge: the MCP client gets a JSON-RPC error with the plain text
    // on its first request instead of "server disconnected".
    let mut b = Bridge::spawn(&env, "claude", &[("SR_PROTO_OVERRIDE", "1")]);
    let v = b
        .request(
            "initialize",
            serde_json::json!({"protocolVersion": "2025-11-25"}),
            T,
        )
        .unwrap();
    let code = b.wait_exit(Duration::from_secs(5));
    record(&format!(
        "old bridge (proto 1 vs daemon 3): {v} exit {code:?}"
    ));
    assert_eq!(v["error"]["code"], -32002);
    assert_eq!(
        v["error"]["message"],
        "Finish updating. Selene Retrieval was updated. Restart Claude to finish."
    );
    assert_eq!(v["error"]["data"]["code"], "UpgradeRequired");
    assert_eq!(code, Some(1));
    assert!(alive(pid), "daemon unaffected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn newer_client_drains_old_daemon_and_attaches_to_new_one() {
    let _s = serial();
    let env = TestEnv::new("skew-new");
    // The "old" daemon speaks PROTO_VERSION - 1; the launcher will start the current stub.
    let old = env.start_daemon(&["--proto", &(PROTO_VERSION - 1).to_string()]);

    // A newer client that cannot launch (connect-only) is refused and the old daemon keeps running.
    let mut o = env.opts(ConnKind::App);
    o.proto = PROTO_VERSION + 1;
    o.launch = false;
    let e = attach(&o).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::ServiceUnavailable);
    assert!(alive(old));

    // A newer client that can launch asks for a drain-restart: old daemon exits, new one serves.
    // Emulates an update: client and launcher-started daemon at N, the running daemon at N-1.
    let o = env.opts(ConnKind::App);
    let conn = attach(&o).await.unwrap();
    record(&format!(
        "newer client: old daemon {old} (proto {}) → restarts={} path={:?} new daemon {} (proto {}) in {} ms",
        PROTO_VERSION - 1,
        conn.restarts,
        conn.path,
        conn.daemon_pid,
        conn.daemon_proto,
        conn.elapsed.as_millis()
    ));
    assert!(conn.restarts >= 1, "the old daemon asked us to come back");
    assert_ne!(conn.daemon_pid, old);
    assert_eq!(conn.daemon_proto, PROTO_VERSION);
    assert!(
        matches!(conn.path, AttachPath::Launched { .. }),
        "{:?}",
        conn.path
    );
    let dl = std::time::Instant::now() + Duration::from_secs(5);
    while alive(old) && std::time::Instant::now() < dl {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(old), "old daemon drained and exited");
    assert!(env.log("daemon.log").contains("drain"));
}
