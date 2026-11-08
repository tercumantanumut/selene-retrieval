//! Event subscription over app-RPC: seq order, resume from a cursor, re-snapshot after a daemon
//! restart, and re-snapshot (never a silent gap) for a subscriber that falls behind the ring.
// Test code: panicking on unexpected errors is the point. Each #[tokio::test] owns its runtime,
// so holding the cross-test serial lock across awaits cannot deadlock.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]

mod common;

use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use sr_client::{AppClient, StreamItem, SubItem, Subscription};
use sr_common::protocol::ConnKind;

const T: Duration = Duration::from_secs(20);

async fn next(st: &mut sr_client::EventStream) -> StreamItem {
    tokio::time::timeout(T, st.next())
        .await
        .expect("event within timeout")
        .expect("stream open")
        .expect("no gap")
}

fn seq_of(i: &StreamItem) -> u64 {
    match i {
        StreamItem::Event(e) => e.seq,
        StreamItem::Snapshot(s) => panic!("expected an event, got snapshot at {}", s.seq),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_replays_from_cursor_and_resnapshots_after_restart() {
    let _s = serial();
    let env = TestEnv::new("events");
    let opts = env.opts(ConnKind::App);

    // 1. Fresh subscriber: snapshot first, then events 1..=3 in order.
    let c1 = AppClient::connect(&opts).await.unwrap();
    let pid1 = c1.daemon_pid;
    let mut st = c1.subscribe(None).await.unwrap();
    let StreamItem::Snapshot(s0) = next(&mut st).await else {
        panic!("snapshot first")
    };
    assert_eq!(s0.seq, 0);
    c1.request("test.emit", json!({"n": 3})).await.unwrap();
    let got: Vec<u64> = vec![
        seq_of(&next(&mut st).await),
        seq_of(&next(&mut st).await),
        seq_of(&next(&mut st).await),
    ];
    assert_eq!(got, [1, 2, 3]);
    let cursor = st.cursor();
    assert_eq!(cursor.seq, 3);
    drop(st);
    drop(c1);

    // 2. Events published while disconnected are replayed from the cursor (same daemon run).
    let c2 = AppClient::connect(&opts).await.unwrap();
    c2.request("test.emit", json!({"n": 2})).await.unwrap();
    let c3 = AppClient::connect(&opts).await.unwrap();
    let mut st3 = c3.subscribe(Some(cursor)).await.unwrap();
    let replayed = vec![seq_of(&next(&mut st3).await), seq_of(&next(&mut st3).await)];
    assert_eq!(
        replayed,
        [4, 5],
        "resume from seq 3 replays 4 and 5, no snapshot"
    );
    c3.request("test.emit", json!({"n": 1})).await.unwrap();
    assert_eq!(
        seq_of(&next(&mut st3).await),
        6,
        "then live events continue"
    );
    let cursor6 = st3.cursor();
    drop(st3);
    drop(c3);

    // 3. Resuming Subscription across a daemon restart: Reconnected, then a snapshot of the new
    //    run (seq restarts at 1, so an old cursor must never be mixed in).
    let mut sub = Subscription::start(opts.clone(), Some(cursor6))
        .await
        .unwrap();
    c2.request("test.emit", json!({"n": 1})).await.unwrap();
    match tokio::time::timeout(T, sub.next()).await.unwrap().unwrap() {
        SubItem::Event(e) => assert_eq!(e.seq, 7),
        other => panic!("expected seq 7, got {other:?}"),
    }
    c2.shutdown(true).await.unwrap();
    drop(c2);
    // Old daemon drains and exits; the subscription re-attaches and launches a new one.
    let item = tokio::time::timeout(T, sub.next()).await.unwrap().unwrap();
    let SubItem::Reconnected { daemon_pid: pid2 } = item else {
        panic!("expected Reconnected, got {item:?}")
    };
    assert_ne!(pid2, pid1, "a new daemon run");
    let item = tokio::time::timeout(T, sub.next()).await.unwrap().unwrap();
    let SubItem::Snapshot(snap) = item else {
        panic!("expected a snapshot of the new run, got {item:?}")
    };
    assert_ne!(snap.stream, cursor6.stream, "new stream id");
    assert_eq!(snap.value, json!({"emitted": 0}));
    sub.client()
        .unwrap()
        .request("test.emit", json!({"n": 2}))
        .await
        .unwrap();
    for want in [snap.seq + 1, snap.seq + 2] {
        match tokio::time::timeout(T, sub.next()).await.unwrap().unwrap() {
            SubItem::Event(e) => assert_eq!(e.seq, want),
            other => panic!("expected seq {want}, got {other:?}"),
        }
    }
    assert!(!alive(pid1), "old daemon exited after drain");
    record(&format!(
        "events: snapshot→1..3, resume(3)→4,5,6 replayed/live, restart {pid1}→{pid2}: Reconnected + Snapshot(stream {}) + live {}..{}",
        snap.stream,
        snap.seq + 1,
        snap.seq + 2
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_cursor_and_lagging_subscriber_get_a_resnapshot() {
    let _s = serial();
    let env = TestEnv::new("ring");
    env.start_daemon(&["--ring", "8"]);
    let opts = env.opts(ConnKind::App);
    let c = AppClient::connect(&opts).await.unwrap();
    let ping = c.ping().await.unwrap();
    // A cursor that has fallen out of the 8-event ring.
    c.request("test.emit", json!({"n": 20})).await.unwrap();
    let stale = sr_common::events::Cursor {
        stream: ping.cursor.stream,
        seq: 2,
    };
    let c2 = AppClient::connect(&opts).await.unwrap();
    let mut st = c2.subscribe(Some(stale)).await.unwrap();
    match next(&mut st).await {
        StreamItem::Snapshot(s) => {
            assert_eq!(s.seq, 20);
            assert_eq!(s.value, json!({"emitted": 20}));
        }
        e => panic!("expected resnapshot, got {e:?}"),
    }

    // A subscriber that stops reading while 5000 events are published lags the broadcast buffer;
    // with an 8-event ring it must get `events.resnapshot`, then contiguous events again.
    let c3 = AppClient::connect(&opts).await.unwrap();
    let mut slow = c3.subscribe(None).await.unwrap();
    let StreamItem::Snapshot(first) = next(&mut slow).await else {
        panic!()
    };
    for _ in 0..5 {
        c.request("test.emit", json!({"n": 1000})).await.unwrap();
    }
    let last = first.seq + 5000;
    let mut events = 0u64;
    let mut snapshots = 0u64;
    let mut prev = first.seq;
    while prev < last {
        match next(&mut slow).await {
            StreamItem::Event(e) => {
                assert_eq!(e.seq, prev + 1, "contiguous");
                prev = e.seq;
                events += 1;
            }
            StreamItem::Snapshot(s) => {
                assert!(s.seq >= prev);
                prev = s.seq;
                snapshots += 1;
            }
        }
    }
    record(&format!(
        "ring: stale cursor → snapshot@20; slow subscriber over 5000 events: {events} events + {snapshots} resnapshot(s), no gaps"
    ));
    assert!(snapshots >= 1, "a lagging subscriber is re-snapshotted");
    let v: Value = c.request("test.info", json!({})).await.unwrap();
    assert_eq!(v["cursor"]["seq"], json!(last));
}
