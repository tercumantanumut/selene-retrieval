//! Attach-sequence tests through real processes: sr-bridge → sr-launch → sr-test-daemon.
// Test code: panicking on unexpected errors is the point. Each #[tokio::test] owns its runtime,
// so holding the cross-test serial lock across awaits cannot deadlock.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]

mod common;

use std::collections::BTreeSet;
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use common::*;

/// Generous: the product's own budget is ≤ 15 s launcher + 10 s pipe poll, and other builds may
/// hold the CPU at 100 % (the task path then takes several seconds).
const T: Duration = Duration::from_secs(60);

/// 5 bridges start at once with no daemon, ten times: exactly one daemon each time.
#[test]
fn five_bridge_race_x10_gives_exactly_one_daemon() {
    let _s = serial();
    for round in 0..10 {
        let env = TestEnv::new("race");
        let barrier = Arc::new(Barrier::new(5));
        let t0 = Instant::now();
        let handles: Vec<_> = (0..5)
            .map(|i| {
                let b = barrier.clone();
                let dir = env.dir.clone();
                std::thread::spawn(move || -> Result<(u32, u128), String> {
                    b.wait();
                    let t = Instant::now();
                    let child = Command::new(BRIDGE)
                        .args(["--client", &format!("race{i}"), "--data-dir"])
                        .arg(&dir)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(
                            std::fs::File::create(dir.join(format!("bridge{i}.err")))
                                .map_err(|e| e.to_string())?,
                        )
                        .spawn()
                        .map_err(|e| e.to_string())?;
                    let mut br = Bridge::from_child(child);
                    let r = br.initialize(T)?;
                    let ms = t.elapsed().as_millis();
                    // Keep all five sessions open at the same time.
                    std::thread::sleep(Duration::from_millis(300));
                    Ok((r["daemon_pid"].as_u64().unwrap_or(0) as u32, ms))
                })
            })
            .collect();
        let mut pids = BTreeSet::new();
        let mut times = Vec::new();
        for h in handles {
            let (pid, ms) = h.join().unwrap().unwrap_or_else(|e| {
                let errs: Vec<String> =
                    (0..5).map(|i| env.log(&format!("bridge{i}.err"))).collect();
                panic!(
                    "round {round}: {e}\nlaunch.log:\n{}\ndaemon.log:\n{}\nbridge stderr:\n{}",
                    env.log("launch.log"),
                    env.log("daemon.log"),
                    errs.join("\n")
                )
            });
            pids.insert(pid);
            times.push(ms);
        }
        let owners = env.markers("owner");
        let starts = env.markers("start");
        let launches = env.log("launch.log").matches("launch start").count();
        let live = env.live_daemons();
        record(&format!(
            "race round {round}: 5/5 attached, daemon pids {pids:?}, pipe owners {}, daemon starts {}, launcher runs {launches}, live {live:?}, attach ms {times:?}, total {} ms",
            owners.len(),
            starts.len(),
            t0.elapsed().as_millis()
        ));
        assert_eq!(pids.len(), 1, "round {round}: one daemon served everyone");
        assert_eq!(owners.len(), 1, "round {round}: exactly one pipe owner");
        assert_eq!(
            live.len(),
            1,
            "round {round}: exactly one daemon process alive"
        );
        assert_eq!(owners, pids, "round {round}");
        assert!(launches >= 1);
        if launches > 1 || starts.len() > 1 {
            record(&format!(
                "race round {round}: NOTE extra launcher runs/starts (relaunch after 3 s under load); duplicates exited via host.lock"
            ));
        }
    }
}

/// VS Code-style `taskkill /T /F` on the bridge tree: the daemon survives (its parent, the
/// launcher, is already gone) and serves the next client.
#[cfg(windows)]
#[test]
fn taskkill_tree_on_client_daemon_survives() {
    let _s = serial();
    let env = TestEnv::new("treekill");
    let mut b = Bridge::spawn(&env, "vscode", &[]);
    let r = b.initialize(T).unwrap();
    let dpid = r["daemon_pid"].as_u64().unwrap() as u32;
    assert_eq!(r["launched_by"], expected_free_method(), "{r}");
    if ambient_allows_breakaway() {
        assert_eq!(
            r["in_job"], false,
            "verified breakaway: daemon is in no job"
        );
    }
    let parent = parent_of(dpid);
    let sys = sr_client::win::system_dir().unwrap();
    let st = Command::new(sys.join("taskkill.exe"))
        .args(["/T", "/F", "/PID", &b.pid().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success());
    assert!(
        b.wait_exit(Duration::from_secs(5)).is_some(),
        "bridge killed"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(alive(dpid), "daemon survived the tree kill");
    let mut b2 = Bridge::spawn(&env, "vscode", &[]);
    let r2 = b2.initialize(T).unwrap();
    assert_eq!(
        r2["daemon_pid"].as_u64().unwrap() as u32,
        dpid,
        "same daemon serves the next client"
    );
    record(&format!(
        "treekill (ambient breakaway allowed: {}): daemon {dpid} launched_by {} parent at kill time = {parent:?}; alive after taskkill /T /F: true; reattached to same pid",
        ambient_allows_breakaway(),
        r["launched_by"]
    ));
}

/// Unix twin of the tree kill: SIGKILL to the bridge's whole process group (what an MCP client
/// does on quit). The daemon runs in its own session (`setsid` by the launcher), survives and
/// serves the next client.
#[cfg(unix)]
#[test]
fn group_kill_on_client_daemon_survives() {
    let _s = serial();
    let env = TestEnv::new("groupkill");
    let mut b = Bridge::spawn(&env, "vscode", &[]);
    let r = b.initialize(T).unwrap();
    let dpid = r["daemon_pid"].as_u64().unwrap() as u32;
    assert_eq!(r["launched_by"], expected_free_method(), "{r}");
    let st = Command::new("/bin/kill")
        .args(["-s", "KILL", "--", &format!("-{}", b.pid())])
        .status()
        .unwrap();
    assert!(st.success());
    assert!(
        b.wait_exit(Duration::from_secs(5)).is_some(),
        "bridge killed"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(alive(dpid), "daemon survived the group kill");
    let mut b2 = Bridge::spawn(&env, "vscode", &[]);
    let r2 = b2.initialize(T).unwrap();
    assert_eq!(
        r2["daemon_pid"].as_u64().unwrap() as u32,
        dpid,
        "same daemon serves the next client"
    );
}

/// Contract: the bridge exits promptly on stdin EOF (VS Code escalates to a tree kill after 20 s).
#[test]
fn bridge_exits_promptly_on_stdin_eof() {
    let _s = serial();
    let env = TestEnv::new("eof");
    let mut warm = Bridge::spawn(&env, "warm", &[]);
    warm.initialize(T).unwrap();
    let mut b = Bridge::spawn(&env, "eof", &[]);
    b.initialize(T).unwrap();
    let t = Instant::now();
    b.close_stdin();
    let code = b.wait_exit(Duration::from_secs(5));
    let ms = t.elapsed().as_millis();
    record(&format!(
        "stdin EOF → bridge exit in {ms} ms (code {code:?})"
    ));
    assert_eq!(code, Some(0));
    assert!(ms < 3000, "{ms} ms");
}

/// A second daemon for the same user + data dir exits at once: host.lock, and with the lock
/// skipped, FIRST_PIPE_INSTANCE.
#[test]
fn second_daemon_is_refused() {
    let _s = serial();
    let env = TestEnv::new("second");
    let first = env.start_daemon(&[]);
    for extra in [vec![], vec!["--skip-host-lock"]] {
        let t = Instant::now();
        let st = Command::new(STUB)
            .arg("--data-dir")
            .arg(&env.dir)
            .args(["--launched-by", "second"])
            .args(&extra)
            .status()
            .unwrap();
        record(&format!(
            "second daemon {extra:?}: exit {:?} after {} ms",
            st.code(),
            t.elapsed().as_millis()
        ));
        assert_eq!(st.code(), Some(0));
    }
    let log = env.log("daemon.log");
    assert!(log.contains("host.lock held"), "{log}");
    assert!(log.contains("pipe create failed"), "{log}");
    assert_eq!(env.markers("owner"), BTreeSet::from([first]));
    assert!(alive(first));
}

/// Bridge cold start (`--version`), cold attach (daemon launched) and warm attach timings.
#[test]
fn bridge_cold_and_warm_timings() {
    let _s = serial();
    let n = 20;
    let mut v = Vec::new();
    for _ in 0..n {
        let t = Instant::now();
        let st = Command::new(BRIDGE)
            .arg("--version")
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
        v.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let (min, med, p95) = stats(v);
    record(&format!(
        "bridge --version process create→exit n={n}: min {min:.1} median {med:.1} p95 {p95:.1} ms"
    ));

    let mut cold = Vec::new();
    for _ in 0..5 {
        let env = TestEnv::new("cold");
        let t = Instant::now();
        let mut b = Bridge::spawn(&env, "cold", &[]);
        let r = b.initialize(T).unwrap();
        cold.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(r["launched_by"], expected_free_method());
    }
    let (min, med, p95) = stats(cold);
    record(&format!(
        "cold attach via {} (spawn bridge → launcher → daemon → initialize result) n=5: min {min:.1} median {med:.1} max {p95:.1} ms",
        expected_free_method()
    ));

    let env = TestEnv::new("warm");
    let mut keep = Bridge::spawn(&env, "keep", &[]);
    keep.initialize(T).unwrap();
    let mut warm = Vec::new();
    for _ in 0..n {
        let t = Instant::now();
        let mut b = Bridge::spawn(&env, "warm", &[]);
        b.initialize(T).unwrap();
        warm.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let (min, med, p95) = stats(warm);
    record(&format!(
        "warm attach (spawn bridge → initialize result) n={n}: min {min:.1} median {med:.1} p95 {p95:.1} ms"
    ));
    assert!(med < 2000.0, "warm median {med} ms");
}
