//! Supervisor behaviour against a deliberately misbehaving worker (`--features test-probes`):
//! timeout → kill → restart, crashes, memory cap, protocol violations from a "compromised"
//! worker, parser panics, sandbox denials, backpressure and shutdown without leaks.
#![cfg(feature = "test-probes")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use sr_common::ErrorCode;
use sr_common::protocol::extract::DocKind;
use sr_extract_client::{
    Disposition, ExtractConfig, ExtractPool, ExtractSource, FailureCause, SandboxMode,
};

fn probe(p: &str) -> ExtractSource {
    ExtractSource::Bytes(format!("SR-PROBE:{p}").into_bytes())
}

fn pool_with(
    tag: &str,
    mode: SandboxMode,
    f: impl FnOnce(&mut ExtractConfig),
) -> (ExtractPool, Profile) {
    let (exe, dll) = stage(&format!("sup-{tag}"));
    let prof = Profile::new(&format!("sup-{tag}"));
    let mut cfg = config(&exe, &dll, mode, &prof.0);
    cfg.workers = 1;
    f(&mut cfg);
    (ExtractPool::start(cfg).unwrap(), prof)
}

async fn legit_ok(pool: &ExtractPool) -> bool {
    pool.extract(ExtractSource::Bytes(sample_pdf(1)), DocKind::Pdf)
        .await
        .map(|x| x.text.contains("page 1"))
        .unwrap_or(false)
}

fn assert_no_leaks(pool: &ExtractPool) {
    pool.shutdown();
    let leaked = pool.leaked_workers();
    assert!(leaked.is_empty(), "leaked {leaked:?}");
}

#[test]
fn timeout_kills_and_the_next_job_gets_a_fresh_worker() {
    let (pool, _p) = pool_with("timeout", SandboxMode::AppContainer, |c| {
        quick_timeouts(c, Duration::from_millis(1500));
    });
    rt().block_on(async {
        assert!(legit_ok(&pool).await);
        let first = pool.live_workers();
        for p in ["hang", "spin"] {
            let t = Instant::now();
            let e = pool.extract(probe(p), DocKind::Text).await.unwrap_err();
            let took = t.elapsed();
            assert_eq!(e.code(), ErrorCode::ExtractTimeout, "{p}: {e:?}");
            assert_eq!(e.disposition, Disposition::DeadLetter);
            assert!(
                matches!(
                    e.cause,
                    FailureCause::Timeout { .. } | FailureCause::CpuLimit
                ),
                "{e:?}"
            );
            assert!(took < Duration::from_secs(5), "{p}: kill took {took:?}");
            assert!(legit_ok(&pool).await, "pool recovered after {p}");
        }
        for w in first {
            assert!(!w.is_alive(), "timed-out worker {w:?} still alive");
        }
        let s = pool.stats();
        assert_eq!(s.timeouts + s.cpu_limit_kills, 2);
        assert!(s.spawns >= 3);
    });
    assert_no_leaks(&pool);
}

#[test]
fn crashes_and_memory_cap_are_contained() {
    let (pool, _p) = pool_with("crash", SandboxMode::AppContainer, |c| {
        c.memory_limit = 256 << 20;
    });
    rt().block_on(async {
        for (p, cause) in [
            ("crash", "crash"),
            ("stack", "crash"),
            ("abort", "crash"),
            ("exit", "crash"),
            ("alloc", "memory"),
        ] {
            let e = pool.extract(probe(p), DocKind::Text).await.unwrap_err();
            assert_eq!(e.code(), ErrorCode::FileUnreadable, "{p}: {e:?}");
            assert_eq!(e.disposition, Disposition::DeadLetter);
            match cause {
                "memory" => assert!(
                    matches!(
                        e.cause,
                        FailureCause::MemoryLimit | FailureCause::Crashed { .. }
                    ),
                    "{p}: {e:?}"
                ),
                _ => assert!(
                    matches!(e.cause, FailureCause::Crashed { .. }),
                    "{p}: {e:?}"
                ),
            }
            assert!(legit_ok(&pool).await, "pool recovered after {p}");
        }
    });
    assert_no_leaks(&pool);
}

#[test]
fn a_compromised_worker_cannot_hurt_the_supervisor() {
    let (pool, _p) = pool_with("proto", SandboxMode::AppContainer, |c| {
        quick_timeouts(c, Duration::from_secs(3));
    });
    rt().block_on(async {
        for p in [
            "badlen",
            "badid",
            "badutf8",
            "lielen",
            "bigtext",
            "badjson",
            "garbage:\u{1}\u{0}\u{0}\u{0}x{\"id\"",
        ] {
            let before = pool.live_workers();
            let e = pool.extract(probe(p), DocKind::Text).await.unwrap_err();
            assert_eq!(e.code(), ErrorCode::FileUnreadable, "{p}: {e:?}");
            assert!(
                matches!(
                    e.cause,
                    FailureCause::ProtocolViolation | FailureCause::Crashed { .. }
                ),
                "{p}: {e:?}"
            );
            for w in before {
                assert!(!w.is_alive(), "{p}: misbehaving worker {w:?} not killed");
            }
            assert!(legit_ok(&pool).await, "pool recovered after {p}");
        }
        assert!(pool.stats().protocol_violations >= 5);
    });
    assert_no_leaks(&pool);
}

#[test]
fn parser_panic_is_a_typed_failure_and_the_worker_survives() {
    let (pool, _p) = pool_with("panic", SandboxMode::AppContainer, |_| {});
    rt().block_on(async {
        assert!(legit_ok(&pool).await);
        let pid = pool.live_pids();
        let e = pool
            .extract(probe("panic"), DocKind::Text)
            .await
            .unwrap_err();
        assert_eq!(e.code(), ErrorCode::FileUnreadable);
        assert_eq!(
            e.cause,
            FailureCause::Worker(sr_common::protocol::extract::WorkerFailure::ParserPanic)
        );
        assert_eq!(pool.live_pids(), pid, "same worker after a caught panic");
        assert!(legit_ok(&pool).await);
    });
    assert_no_leaks(&pool);
}

#[cfg(windows)]
#[test]
fn app_container_denies_files_network_and_child_processes() {
    let secret = tmp_root().join("sup-secret.txt");
    std::fs::write(&secret, b"user data").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let user_dir = std::env::var("USERPROFILE").unwrap();
    for (mode, tag) in [
        (SandboxMode::AppContainer, "deny-ac"),
        (SandboxMode::LowIntegrity, "deny-low"),
    ] {
        let (pool, _p) = pool_with(tag, mode, |_| {});
        let text = |p: String| {
            rt().block_on(pool.extract(probe(&p), DocKind::Text))
                .unwrap()
                .text
        };
        let read = text(format!("read:{}", secret.display()));
        let write = text(format!("write:{user_dir}\\sr-m1b-probe.txt"));
        let net = text(format!("net:{addr}"));
        let spawn = text("spawn".into());
        println!("{tag}: {read} | {write} | {net} | {spawn}");
        assert!(write.starts_with("WRITE_DENIED"), "{tag}: {write}");
        assert!(spawn.starts_with("SPAWN_DENIED"), "{tag}: {spawn}");
        if mode == SandboxMode::AppContainer {
            assert!(read.starts_with("READ_DENIED"), "{tag}: {read}");
            assert!(net.starts_with("NET_DENIED"), "{tag}: {net}");
        }
        assert_no_leaks(&pool);
    }
    assert!(!std::path::Path::new(&format!("{user_dir}\\sr-m1b-probe.txt")).exists());
}

/// The sandbox escape tests on macOS: from inside the worker (the probes run in the real,
/// sandboxed process) every file read outside the system fonts, every file write, IP and Unix
/// socket connections, fork and exec must fail, while the parser itself keeps working.
#[cfg(target_os = "macos")]
#[test]
fn seatbelt_denies_files_network_and_child_processes() {
    let secret = tmp_root().join("sup-secret.txt");
    std::fs::write(&secret, b"user data").unwrap();
    let home = std::env::var("HOME").unwrap();
    let home_file = std::path::Path::new(&home).join(".zshrc");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let sock = sock_dir.path().join("s.sock");
    let _ul = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let written = [
        tmp_root().join("sr-p3-probe.txt"),
        std::path::PathBuf::from("/private/tmp/sr-p3-probe.txt"),
        std::path::Path::new(&home).join("sr-p3-probe.txt"),
    ];
    let (pool, _p) = pool_with("deny-seatbelt", SandboxMode::Auto, |_| {});
    let text = |p: String| {
        rt().block_on(pool.extract(probe(&p), DocKind::Text))
            .unwrap()
            .text
    };
    // EPERM ("os error 1") is the sandbox's answer; "not found" would prove nothing.
    let eperm = |r: &str| r.contains("(os error 1)");
    let mut seen = Vec::new();
    let mut reads = vec![
        secret.display().to_string(),
        "/etc/hosts".into(),
        "/private/etc/passwd".into(),
    ];
    if home_file.exists() {
        reads.push(home_file.display().to_string());
    }
    for path in reads {
        let r = text(format!("read:{path}"));
        assert!(
            r.starts_with("READ_DENIED") && eperm(&r),
            "read {path}: {r}"
        );
        seen.push(r);
    }
    // The system fonts are the one thing it may read (pdfium maps non-embedded fonts).
    let font = text("read:/System/Library/Fonts/Helvetica.ttc".into());
    assert!(font.starts_with("READ_ALLOWED"), "{font}");
    for path in &written {
        let r = text(format!("write:{}", path.display()));
        assert!(
            r.starts_with("WRITE_DENIED") && eperm(&r),
            "write {}: {r}",
            path.display()
        );
        seen.push(r);
    }
    for (p, want) in [
        (format!("net:{addr}"), "NET_DENIED"),
        ("net:1.1.1.1:443".to_string(), "NET_DENIED"),
        (format!("unix:{}", sock.display()), "UNIX_DENIED"),
    ] {
        let r = text(p.clone());
        assert!(r.starts_with(want) && eperm(&r), "{p}: {r}");
        seen.push(r);
    }
    // fork/exec: the profile (EPERM) or RLIMIT_NPROC = 0 (EAGAIN) refuses it, whichever the
    // kernel checks first.
    for (p, want) in [("spawn", "SPAWN_DENIED"), ("fork", "FORK_DENIED")] {
        let r = text(p.into());
        assert!(r.starts_with(want), "{p}: {r}");
        seen.push(r);
    }
    println!("seatbelt denials: {seen:#?}");
    // Still a working parser after all of that.
    assert!(rt().block_on(legit_ok(&pool)));
    assert_eq!(
        pool.stats().sandbox,
        Some(sr_extract_client::ActiveSandbox::Seatbelt)
    );
    assert_no_leaks(&pool);
    for path in &written {
        assert!(!path.exists(), "{} was written", path.display());
    }
}

/// Fail closed: a worker that runs without its sandbox, or with a weaker one, is refused by the
/// supervisor's own kernel check (`sandbox_check` on its pid), although it claims to be sandboxed.
/// Such a worker is killed before it gets any work, and no file is ever parsed by it.
#[cfg(target_os = "macos")]
#[test]
fn weak_or_missing_sandbox_is_refused() {
    for variant in ["nosandbox", "weak"] {
        let dir = tmp_root().join("stage").join(format!("sup-{variant}"));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(format!("sr-extract-{variant}-test"));
        let _ = std::fs::remove_file(&exe);
        std::fs::copy(env!("CARGO_BIN_EXE_sr-extract"), &exe).unwrap();
        let dll = pdfium_dll();
        let mut cfg = config(&exe, &dll, SandboxMode::Auto, "unused");
        cfg.workers = 1;
        cfg.spawn_timeout = Duration::from_secs(10);
        let pool = ExtractPool::start(cfg).unwrap();
        let e = rt()
            .block_on(pool.extract(ExtractSource::Bytes(b"hello".to_vec()), DocKind::Text))
            .unwrap_err();
        assert_eq!(e.cause, FailureCause::SpawnFailed, "{variant}: {e:?}");
        assert_eq!(e.disposition, Disposition::Retry);
        let detail = e.error.ctx.detail.clone().unwrap_or_default();
        assert!(
            detail.contains("sandbox is not in force"),
            "{variant}: {detail}"
        );
        let s = pool.stats();
        assert_eq!(s.jobs_ok, 0);
        assert_eq!(s.spawns, 0, "{variant}: an unconfined worker was accepted");
        assert!(s.spawn_failures >= 1);
        assert!(pool.spawned_workers().is_empty());
        pool.shutdown();
        // Every refused worker was killed (they never reach the pool's list, so look them up by
        // their executable).
        let alive = std::process::Command::new("/usr/bin/pgrep")
            .arg("-f")
            .arg(&exe)
            .output()
            .unwrap();
        assert!(
            alive.stdout.is_empty(),
            "{variant}: refused workers still running: {}",
            String::from_utf8_lossy(&alive.stdout)
        );
        println!("{variant}: refused: {detail}");
    }
}

#[test]
fn backpressure_bounds_admitted_jobs() {
    let (pool, _p) = pool_with("bp", SandboxMode::AppContainer, |c| {
        c.queue = 1;
        quick_timeouts(c, Duration::from_millis(1500));
    });
    let rt = rt();
    rt.block_on(async {
        assert_eq!(pool.available_permits(), 2);
        // Two hanging jobs take both permits (1 running + 1 queued).
        let a = pool.extract(probe("hang"), DocKind::Text);
        let b = pool.extract(probe("hang"), DocKind::Text);
        let c = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(pool.available_permits(), 0);
            let busy = pool
                .try_extract(ExtractSource::Bytes(b"x".to_vec()), DocKind::Text)
                .await;
            assert_eq!(busy.unwrap_err().code(), ErrorCode::Busy);
        };
        let (ra, rb, ()) = tokio::join!(a, b, c);
        assert_eq!(ra.unwrap_err().code(), ErrorCode::ExtractTimeout);
        assert_eq!(rb.unwrap_err().code(), ErrorCode::ExtractTimeout);
        assert_eq!(pool.available_permits(), 2);
        assert!(legit_ok(&pool).await);
    });
    assert_no_leaks(&pool);
}

#[test]
fn shutdown_kills_running_workers_and_cancels_waiters() {
    let (pool, _p) = pool_with("shut", SandboxMode::AppContainer, |c| {
        c.workers = 2;
        quick_timeouts(c, Duration::from_secs(60));
    });
    let pool = std::sync::Arc::new(pool);
    let rt = rt();
    rt.block_on(async {
        let p2 = std::sync::Arc::clone(&pool);
        let h = tokio::spawn(async move { p2.extract(probe("hang"), DocKind::Text).await });
        let t0 = Instant::now();
        while pool.live_pids().len() < 2 && t0.elapsed() < Duration::from_secs(20) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(pool.live_pids().len(), 2);
        tokio::time::sleep(Duration::from_millis(200)).await; // the hang job is running
        let t = Instant::now();
        pool.shutdown();
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "shutdown took {:?}",
            t.elapsed()
        );
        let r = h.await.unwrap();
        assert!(r.is_err());
        assert!(
            pool.leaked_workers().is_empty(),
            "{:?}",
            pool.leaked_workers()
        );
        let after = pool
            .extract(ExtractSource::Bytes(b"x".to_vec()), DocKind::Text)
            .await;
        assert_eq!(after.unwrap_err().code(), ErrorCode::Cancelled);
    });
}

#[test]
fn source_errors_are_retryable_and_never_reach_a_worker() {
    let (pool, _p) = pool_with("src", SandboxMode::AppContainer, |c| {
        c.max_file_bytes = 1024
    });
    rt().block_on(async {
        let e = pool
            .extract(
                ExtractSource::Path(tmp_root().join("does-not-exist.pdf")),
                DocKind::Pdf,
            )
            .await
            .unwrap_err();
        assert_eq!(e.code(), ErrorCode::NotFound);
        assert_eq!(e.disposition, Disposition::Retry);
        let e = pool
            .extract(ExtractSource::Bytes(vec![b'a'; 2048]), DocKind::Text)
            .await
            .unwrap_err();
        assert_eq!(e.code(), ErrorCode::FileTooLarge);
        let um = e.error.user_message();
        assert!(um.body.contains("1 KB"), "{}", um.body);
        // ExtractSource::File is read from its current position (the SandboxFs handle path).
        let f = tmp_root().join("sup-src.txt");
        std::fs::write(&f, b"skip-this|keep this").unwrap();
        let mut h = std::fs::File::open(&f).unwrap();
        std::io::Seek::seek(&mut h, std::io::SeekFrom::Start(10)).unwrap();
        let r = pool
            .extract(ExtractSource::File(h), DocKind::Text)
            .await
            .unwrap();
        assert_eq!(r.text, "keep this");
    });
    assert_no_leaks(&pool);
}
