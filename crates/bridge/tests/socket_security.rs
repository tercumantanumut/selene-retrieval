//! Unix socket access control (Amendment 18.2, the twin of `pipe_security.rs`): socket 0600 in a
//! 0700 per-user folder, the client's checks before it sends a byte, a planted file that is never
//! replaced, and handshake hardening.
//!
//! Not testable here without a second account: a server or a peer of another uid (`getpeereid`
//! on both sides). Those decisions are unit-tested in `sr_client::unix`.
#![cfg(unix)]
// Test code: panicking on unexpected errors is the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::time::Instant;

use common::*;

fn mode(p: &std::path::Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().mode() & 0o7777
}

/// The daemon's socket is ours with mode 0600 in our 0700 folder; the client refuses a socket
/// opened up to others, and makes our folder private again when its mode was widened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_and_folder_are_private_and_checked_by_the_client() {
    let _s = serial();
    let env = TestEnv::new("sock");
    let pid = env.start_daemon(&[]);
    let sock = env.socket_path();
    let dir = sock.parent().unwrap().to_path_buf();
    let me = sr_client::sys::uid();
    let m = std::fs::symlink_metadata(&sock).unwrap();
    assert!(m.file_type().is_socket());
    assert_eq!(m.uid(), me);
    assert_eq!(mode(&sock), 0o600);
    assert_eq!(std::fs::symlink_metadata(&dir).unwrap().uid(), me);
    assert_eq!(mode(&dir), 0o700);
    let owner = std::fs::read_to_string(env.dir.join(format!("owner-{pid}"))).unwrap();
    record(&format!("socket owner/mode: {owner}"));
    assert!(owner.contains("mode=600"), "{owner}");
    #[cfg(target_os = "macos")]
    assert_eq!(sr_client::unix::extended_acl_entries(&dir).unwrap(), (0, 0));

    // A socket opened to others is refused before a byte is sent.
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666)).unwrap();
    let e = sr_client::sys::connect(&env.pipe_name()).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
    let s = sr_client::sys::connect(&env.pipe_name()).await.unwrap();
    assert_eq!(
        sr_client::sys::pipe_server_is_ours(&s).unwrap(),
        sr_client::sys::ServerIdentity::Ours
    );
    drop(s);

    // Our folder with group/other bits is made private again by the next client.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let s = sr_client::sys::connect(&env.pipe_name()).await.unwrap();
    drop(s);
    assert_eq!(mode(&dir), 0o700);
    assert!(alive(pid));
}

/// A file at the socket's name that is not our socket is never removed: the daemon refuses to
/// start rather than replace it.
#[test]
fn planted_file_at_the_socket_name_is_left_alone() {
    let _s = serial();
    let env = TestEnv::new("plant");
    let sock = env.socket_path();
    std::fs::write(&sock, "not a socket").unwrap();
    let st = std::process::Command::new(STUB)
        .arg("--data-dir")
        .arg(&env.dir)
        .args(["--launched-by", "test", "--idle-exit-secs", "2"])
        .status()
        .unwrap();
    let log = env.log("daemon.log");
    record(&format!("planted file: exit {:?}; {log}", st.code()));
    assert!(log.contains("pipe create failed"), "{log}");
    assert_eq!(std::fs::read_to_string(&sock).unwrap(), "not a socket");
    std::fs::remove_file(&sock).unwrap();
}

/// Handshake hardening over the socket: garbage, unknown kind and oversize lines are refused with
/// `invalid`, and a silent probe connection doesn't disturb the daemon.
#[test]
fn malformed_handshakes_are_refused() {
    let _s = serial();
    let env = TestEnv::new("badhs");
    let pid = env.start_daemon(&[]);
    let sock = env.socket_path();
    let send = |payload: &[u8]| -> String {
        let mut s = UnixStream::connect(&sock).unwrap();
        s.write_all(payload).unwrap();
        let mut line = String::new();
        let _ = BufReader::new(&mut s).read_line(&mut line);
        line
    };
    let garbage = send(b"hello\n");
    let kind = send(b"{\"proto\":1,\"build\":\"x\",\"kind\":\"root\",\"client_id\":\"a\"}\n");
    let mut big = vec![b'a'; 5000];
    big.push(b'\n');
    let oversize = send(&big);
    assert!(garbage.contains(r#""status":"invalid""#), "{garbage}");
    assert!(oversize.contains(r#""status":"invalid""#), "{oversize}");
    assert!(kind.contains(r#""status":"invalid""#), "{kind}");
    let mut s = UnixStream::connect(&sock).unwrap();
    let mut b = [0u8; 1];
    let t = Instant::now();
    let n = s.read(&mut b).unwrap_or(0);
    record(&format!(
        "silent connection closed by daemon after {} ms",
        t.elapsed().as_millis()
    ));
    assert_eq!(n, 0);
    assert!(alive(pid));
}

#[cfg(target_os = "macos")]
async fn challenge(c: &sr_client::AppClient) -> String {
    let v = c
        .request(sr_client::rpc::HOST_CHALLENGE, serde_json::json!({}))
        .await
        .unwrap();
    v["nonce"].as_str().unwrap().to_string()
}

#[cfg(target_os = "macos")]
async fn attest(c: &sr_client::AppClient, nonce: &str) -> bool {
    let r = c
        .request(
            sr_client::rpc::HOST_ATTEST,
            serde_json::json!({ "nonce": nonce }),
        )
        .await
        .unwrap();
    r["trusted"].as_bool().unwrap()
}

/// R50 wiring: with the real caller rules, an app session of a program that is not our app
/// (this test exe) answers the challenge but stays untrusted, and `host.shutdown` is refused.
/// The token rules themselves are unit-tested in `sr_client::unix` (exec, other process).
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unproven_app_session_is_not_trusted() {
    use sr_common::protocol::ConnKind;
    let _s = serial();
    let env = TestEnv::new("attest");
    let pid = env.start_daemon(&["--strict-trust"]);
    let mut o = env.opts(ConnKind::App);
    o.launch = false;
    let c = sr_client::AppClient::connect(&o).await.unwrap();
    // The right nonce from the same process: refused, because the image is not sr-app.
    let nonce = challenge(&c).await;
    assert_eq!(nonce.len(), 64);
    assert!(!attest(&c, &nonce).await);
    // A challenge is answered once; a wrong nonce never passes.
    assert!(!attest(&c, &nonce).await);
    let _ = challenge(&c).await;
    assert!(!attest(&c, &"0".repeat(64)).await);
    let e = c.shutdown(true).await.unwrap_err();
    assert_eq!(e.code, sr_common::error::ErrorCode::PermissionDenied);
    assert!(alive(pid), "the daemon keeps running");
}
