//! Time-bounded reachability checks for network folders (M5-E, R6).
//!
//! A hung SMB share (server gone without a reset, VPN dropped, sleeping NAS) can block a plain
//! `metadata` / `canonicalize` call for a minute or more. Root checks run at startup, before every
//! scan and on every add; one hung share must never hold up the service start, the claim loop or
//! other folders. So a network root is probed first, on its own thread, with a short timeout:
//!
//! * no answer within [`PROBE_TIMEOUT`] → the folder is **unavailable** (never deleted; its files
//!   and index stay, the hourly check tries again);
//! * at most one probe per root is in flight: while an earlier probe of the same root is still
//!   stuck, the root counts as unreachable at once, so stuck threads never pile up.
//!
//! Local roots are not probed (no extra thread on the common path).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::fsid;

/// How long a network root may take to answer a metadata query.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

fn inflight() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(Mutex::default)
}

fn key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// Whether `root` is on the network: a UNC path or a mapped network drive letter.
pub fn is_network_root(root: &Path) -> bool {
    fsid::drive_kind(root) == fsid::DriveKind::Network
}

/// Run `f` on a helper thread and wait at most `timeout`. `None` = no answer in time (the thread
/// is left to finish on its own).
pub fn with_timeout<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("sr-netprobe".into())
        .spawn(move || {
            let _ = tx.send(f());
        });
    if spawned.is_err() {
        return None;
    }
    rx.recv_timeout(timeout).ok()
}

/// Whether the folder at `root` answers within `timeout` (`probe` does the actual check; tests
/// pass a slow one). At most one probe per root runs at a time.
pub fn reachable_with(
    root: &Path,
    timeout: Duration,
    probe: impl FnOnce(PathBuf) -> bool + Send + 'static,
) -> bool {
    let k = key(root);
    {
        let mut set = inflight()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !set.insert(k.clone()) {
            tracing::warn!(target: "sr_sync::netprobe", "an earlier check of a network folder is still waiting; treated as unavailable");
            return false;
        }
    }
    let root = root.to_path_buf();
    let k2 = k.clone();
    let r = with_timeout(timeout, move || {
        let ok = probe(root);
        inflight()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&k2);
        ok
    });
    match r {
        Some(ok) => ok,
        None => {
            tracing::warn!(target: "sr_sync::netprobe", timeout_ms = timeout.as_millis() as u64, "a network folder didn't answer in time; marked unavailable");
            false
        }
    }
}

/// Network roots: answers within [`PROBE_TIMEOUT`] and is a directory. Local roots: `true`
/// (checked by the normal path, which can't hang on a network).
pub fn reachable(root: &Path) -> bool {
    if !is_network_root(root) {
        return true;
    }
    reachable_with(root, PROBE_TIMEOUT, |p| {
        std::fs::metadata(&p).is_ok_and(|m| m.is_dir())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn slow_root_times_out_and_is_not_probed_twice() {
        let root = PathBuf::from(r"\\slow-server\share\m5e-test");
        let t = Instant::now();
        let ok = reachable_with(&root, Duration::from_millis(200), |_| {
            std::thread::sleep(Duration::from_millis(1500));
            true
        });
        assert!(!ok, "a hung share is unavailable");
        assert!(
            t.elapsed() < Duration::from_millis(1000),
            "{:?}",
            t.elapsed()
        );
        // The first probe is still stuck: a second check answers at once, no new thread.
        let t = Instant::now();
        let ok = reachable_with(&root, Duration::from_secs(5), |_| true);
        assert!(!ok);
        assert!(t.elapsed() < Duration::from_millis(100));
        // Once the stuck probe returns, the root is probed normally again.
        std::thread::sleep(Duration::from_millis(1600));
        assert!(reachable_with(&root, Duration::from_secs(5), |_| true));
    }

    #[test]
    fn fast_answers_pass_through() {
        let root = PathBuf::from(r"\\fast-server\share\m5e-test");
        assert!(reachable_with(&root, Duration::from_secs(2), |_| true));
        assert!(!reachable_with(&root, Duration::from_secs(2), |_| false));
        // Local folders are never probed on a thread.
        assert!(reachable(&std::env::temp_dir()));
        assert!(!is_network_root(&std::env::temp_dir()));
        assert!(is_network_root(Path::new(r"\\server\share\x")));
    }
}
