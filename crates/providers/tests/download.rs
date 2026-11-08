//! Model downloader against a local HTTP/1.1 test server (no internet): consent, Range resume,
//! sha256, atomic rename, metered pause, disk pre-flight, retries, error mapping, redirects, and a
//! tier-level end-to-end run that serves the real pinned Standard files (if staged) and loads them.
#![cfg(feature = "download")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sr_common::error::ErrorCode;
use sr_common::events::EngineEvent;
use sr_common::tier::{GIB, MINIMUM, STANDARD, Tier, recommendation};
use sr_providers::download::{
    DownloadConfig, DownloadConsent, DownloadOutcome, DownloadSink, Downloader, FetchOutcome,
    FileSpec, ModelStore,
};
use sr_providers::models::sha256_hex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

// ------------------------------------------------------------------------------- test server

#[derive(Debug, Clone)]
enum Behavior {
    Normal,
    /// Send this many body bytes, then close the connection.
    DropAfter(usize),
    Status(u16),
    /// Answer 200 with the full body even if a Range was asked for.
    IgnoreRange,
    /// Flip one byte of the body.
    Corrupt,
    /// Body in `chunk`-byte pieces with `delay_ms` between them.
    Slow {
        chunk: usize,
        delay_ms: u64,
    },
}

#[derive(Default)]
struct State {
    files: HashMap<String, Vec<u8>>,
    redirects: HashMap<String, String>,
    plan: VecDeque<Behavior>,
    /// (path, Range header, body bytes sent)
    log: Vec<(String, Option<String>, usize)>,
}

#[derive(Clone)]
struct Server {
    base: String,
    st: Arc<Mutex<State>>,
}

impl Server {
    async fn start() -> Self {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let st = Arc::new(Mutex::new(State::default()));
        let s2 = st.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = l.accept().await else {
                    break;
                };
                let st = s2.clone();
                tokio::spawn(async move {
                    let _ = serve(sock, st).await;
                });
            }
        });
        Self { base, st }
    }
    fn put(&self, path: &str, body: Vec<u8>) {
        self.st.lock().unwrap().files.insert(path.into(), body);
    }
    fn redirect(&self, from: &str, to: &str) {
        self.st
            .lock()
            .unwrap()
            .redirects
            .insert(from.into(), to.into());
    }
    fn plan(&self, b: Behavior) {
        self.st.lock().unwrap().plan.push_back(b);
    }
    fn log(&self) -> Vec<(String, Option<String>, usize)> {
        self.st.lock().unwrap().log.clone()
    }
    fn sent(&self) -> usize {
        self.log().iter().map(|l| l.2).sum()
    }
}

async fn serve(mut sock: tokio::net::TcpStream, st: Arc<Mutex<State>>) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();
    let range = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
        .map(|l| l[6..].trim().to_owned());
    let (behavior, body, redirect) = {
        let mut g = st.lock().unwrap();
        let b = g.plan.pop_front().unwrap_or(Behavior::Normal);
        (
            b,
            g.files.get(&path).cloned(),
            g.redirects.get(&path).cloned(),
        )
    };
    let log = |sent: usize| {
        st.lock()
            .unwrap()
            .log
            .push((path.clone(), range.clone(), sent));
    };
    if let Some(to) = redirect {
        log(0);
        let r = format!(
            "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        sock.write_all(r.as_bytes()).await?;
        return Ok(());
    }
    if let Behavior::Status(code) = behavior {
        log(0);
        let r = format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        sock.write_all(r.as_bytes()).await?;
        return Ok(());
    }
    let Some(mut body) = body else {
        log(0);
        sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await?;
        return Ok(());
    };
    if matches!(behavior, Behavior::Corrupt) {
        let mid = body.len() / 2;
        body[mid] ^= 0xff;
    }
    let len = body.len();
    let start = match (&range, &behavior) {
        (Some(r), b) if !matches!(b, Behavior::IgnoreRange) => r
            .strip_prefix("bytes=")
            .and_then(|s| s.trim_end_matches('-').parse::<usize>().ok()),
        _ => None,
    };
    let (status, slice) = match start {
        Some(s) if s >= len => {
            log(0);
            let r = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            sock.write_all(r.as_bytes()).await?;
            return Ok(());
        }
        Some(s) => (
            format!(
                "206 Partial Content\r\nContent-Range: bytes {s}-{}/{len}",
                len - 1
            ),
            &body[s..],
        ),
        None => ("200 OK".to_owned(), &body[..]),
    };
    let h = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        slice.len()
    );
    sock.write_all(h.as_bytes()).await?;
    let mut sent = 0usize;
    match behavior {
        Behavior::DropAfter(n) => {
            let n = n.min(slice.len());
            sock.write_all(&slice[..n]).await?;
            sent = n;
            sock.flush().await?;
            log(sent);
            return Ok(()); // drop: fewer bytes than Content-Length
        }
        Behavior::Slow { chunk, delay_ms } => {
            for c in slice.chunks(chunk) {
                if sock.write_all(c).await.is_err() {
                    break;
                }
                sent += c.len();
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
        _ => {
            sock.write_all(slice).await?;
            sent = slice.len();
        }
    }
    log(sent);
    sock.flush().await?;
    Ok(())
}

// ----------------------------------------------------------------------------------- helpers

fn body(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

fn cfg(base: &str) -> DownloadConfig {
    DownloadConfig {
        base_url: base.into(),
        attempts: 3,
        backoff: Duration::from_millis(10),
        progress_every: Duration::from_millis(0),
        metered_check_every: Duration::from_millis(0),
        is_metered: Arc::new(|| false),
        free_disk: Arc::new(|_| Some(1000 * GIB)),
        ..DownloadConfig::default()
    }
}

fn spec(s: &Server, dir: &Path, name: &str, data: &[u8]) -> FileSpec {
    FileSpec {
        model: "test/model".into(),
        url: format!("{}/{name}", s.base),
        sha256: sha256_hex(data),
        bytes: Some(data.len() as u64),
        dest: dir.join(format!("{name}.onnx")),
        part: dir.join(format!("{name}.part")),
    }
}

fn events() -> (DownloadSink, Arc<Mutex<Vec<EngineEvent>>>) {
    let v = Arc::new(Mutex::new(Vec::new()));
    let v2 = v.clone();
    (Arc::new(move |e| v2.lock().unwrap().push(e)), v)
}

async fn fetch(d: &Downloader, sp: &FileSpec) -> sr_common::Result<FetchOutcome> {
    d.fetch(sp, false, &mut |_, _| {}, &CancellationToken::new())
        .await
}

// --------------------------------------------------------------------------------- the tests

#[tokio::test]
async fn fresh_download_verifies_and_renames_atomically() {
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let data = body(100_000);
    s.put("/a", data.clone());
    let d = Downloader::new(cfg(&s.base)).unwrap();
    let sp = spec(&s, dir.path(), "a", &data);
    let mut seen = Vec::new();
    let r = d
        .fetch(
            &sp,
            false,
            &mut |b, t| seen.push((b, t)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(r, FetchOutcome::Done { fetched: 100_000 });
    assert_eq!(std::fs::read(&sp.dest).unwrap(), data);
    assert!(!sp.part.exists(), "no .part left behind");
    assert_eq!(seen.last(), Some(&(100_000, 100_000)));
    // Second call: verified on disk, nothing fetched.
    assert_eq!(fetch(&d, &sp).await.unwrap(), FetchOutcome::AlreadyPresent);
    assert_eq!(s.log().len(), 1);
}

#[tokio::test]
async fn network_drop_resumes_with_range_in_the_same_call() {
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let data = body(10_000);
    s.put("/m", data.clone());
    s.plan(Behavior::DropAfter(4_000));
    let d = Downloader::new(cfg(&s.base)).unwrap();
    let sp = spec(&s, dir.path(), "m", &data);
    assert_eq!(
        fetch(&d, &sp).await.unwrap(),
        FetchOutcome::Done { fetched: 10_000 }
    );
    let log = s.log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0].1, None);
    assert_eq!(
        log[1].1.as_deref(),
        Some("bytes=4000-"),
        "resumed with Range"
    );
    assert_eq!(s.sent(), 10_000, "no byte fetched twice");
    assert_eq!(std::fs::read(&sp.dest).unwrap(), data);
}

#[tokio::test]
async fn interrupted_download_keeps_part_and_a_later_call_fetches_only_the_rest() {
    // M0-E sim: drop at 40 % → search unaffected, retry fetches only the remaining 6 000 bytes.
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let data = body(10_000);
    s.put("/m", data.clone());
    s.plan(Behavior::DropAfter(4_000));
    let mut c = cfg(&s.base);
    c.attempts = 1;
    let d = Downloader::new(c).unwrap();
    let sp = spec(&s, dir.path(), "m", &data);
    let e = fetch(&d, &sp).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelDownloadFailed);
    assert!(e.retryable);
    assert_eq!(
        std::fs::metadata(&sp.part).unwrap().len(),
        4_000,
        "part kept"
    );
    assert!(!sp.dest.exists());
    // Later (new process, same files): resume.
    let d2 = Downloader::new(cfg(&s.base)).unwrap();
    assert_eq!(
        fetch(&d2, &sp).await.unwrap(),
        FetchOutcome::Done { fetched: 6_000 }
    );
    assert_eq!(s.log()[1].1.as_deref(), Some("bytes=4000-"));
    assert_eq!(s.log()[1].2, 6_000);
    assert_eq!(std::fs::read(&sp.dest).unwrap(), data);
}

#[tokio::test]
async fn corrupt_download_is_deleted_and_never_renamed() {
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let data = body(5_000);
    s.put("/c", data.clone());
    s.plan(Behavior::Corrupt);
    let d = Downloader::new(cfg(&s.base)).unwrap();
    let sp = spec(&s, dir.path(), "c", &data);
    let e = fetch(&d, &sp).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelIntegrity);
    assert!(e.ctx.detail.as_deref().unwrap_or("").contains("sha256"));
    assert!(!sp.part.exists(), "a damaged part is deleted");
    assert!(
        !sp.dest.exists(),
        "a damaged file never gets the final name"
    );
    // A retry starts from scratch and succeeds.
    assert_eq!(
        fetch(&d, &sp).await.unwrap(),
        FetchOutcome::Done { fetched: 5_000 }
    );
}

#[tokio::test]
async fn server_ignoring_range_restarts_from_zero() {
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let data = body(8_000);
    s.put("/r", data.clone());
    s.plan(Behavior::DropAfter(3_000));
    s.plan(Behavior::IgnoreRange);
    let d = Downloader::new(cfg(&s.base)).unwrap();
    let sp = spec(&s, dir.path(), "r", &data);
    assert_eq!(
        fetch(&d, &sp).await.unwrap(),
        FetchOutcome::Done { fetched: 11_000 }
    );
    assert_eq!(std::fs::read(&sp.dest).unwrap(), data, "no spliced bytes");
}

#[tokio::test]
async fn complete_part_skips_the_network_and_bad_existing_file_is_replaced() {
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let data = body(4_096);
    s.put("/p", data.clone());
    let d = Downloader::new(cfg(&s.base)).unwrap();
    let sp = spec(&s, dir.path(), "p", &data);
    // A complete part from a crash just before the rename.
    std::fs::write(&sp.part, &data).unwrap();
    assert_eq!(
        fetch(&d, &sp).await.unwrap(),
        FetchOutcome::Done { fetched: 0 }
    );
    assert!(s.log().is_empty(), "verified from disk, no request");
    // A final file that does not match its pin (tampered) is deleted and fetched again.
    std::fs::write(&sp.dest, vec![0u8; 4_096]).unwrap();
    assert_eq!(
        fetch(&d, &sp).await.unwrap(),
        FetchOutcome::Done { fetched: 4_096 }
    );
    assert_eq!(std::fs::read(&sp.dest).unwrap(), data);
    // An oversized part (wrong file) is reset.
    std::fs::remove_file(&sp.dest).unwrap();
    std::fs::write(&sp.part, vec![1u8; 9_000]).unwrap();
    assert_eq!(
        fetch(&d, &sp).await.unwrap(),
        FetchOutcome::Done { fetched: 4_096 }
    );
}

#[tokio::test]
async fn retries_5xx_but_not_404_and_maps_proxy_and_offline() {
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let data = body(1_000);
    s.put("/x", data.clone());
    let d = Downloader::new(cfg(&s.base)).unwrap();
    // 503, 429 then OK (attempts = 3).
    s.plan(Behavior::Status(503));
    s.plan(Behavior::Status(429));
    let sp = spec(&s, dir.path(), "x", &data);
    assert!(matches!(
        fetch(&d, &sp).await.unwrap(),
        FetchOutcome::Done { .. }
    ));
    // 404: no retry.
    let missing = spec(&s, dir.path(), "nope", &data);
    let n = s.log().len();
    let e = fetch(&d, &missing).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelDownloadFailed);
    assert_eq!(s.log().len(), n + 1, "404 is not retried");
    // 407 from a proxy → NetworkBlocked (the user's firewall/proxy, "Copy diagnostics").
    s.plan(Behavior::Status(407));
    let y = spec(&s, dir.path(), "x2", &data);
    s.put("/x2", data.clone());
    assert_eq!(
        fetch(&d, &y).await.unwrap_err().code,
        ErrorCode::NetworkBlocked
    );
    // Nothing listening → Offline (after its retries).
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let d2 = Downloader::new(cfg(&dead)).unwrap();
    let z = FileSpec {
        url: format!("{dead}/z"),
        ..spec(&s, dir.path(), "z", &data)
    };
    let e = fetch(&d2, &z).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Offline, "{e:?}");
    assert!(e.ctx.host.as_deref().unwrap_or("").starts_with("127.0.0.1"));
}

#[tokio::test]
async fn redirects_follow_https_or_loopback_only() {
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let data = body(2_000);
    s.put("/cdn/blob", data.clone());
    s.redirect("/resolve/f", &format!("{}/cdn/blob", s.base));
    s.redirect("/resolve/evil", "http://example.invalid/blob");
    let d = Downloader::new(cfg(&s.base)).unwrap();
    let sp = FileSpec {
        url: format!("{}/resolve/f", s.base),
        ..spec(&s, dir.path(), "f", &data)
    };
    assert!(matches!(
        fetch(&d, &sp).await.unwrap(),
        FetchOutcome::Done { .. }
    ));
    // A redirect to plain http on another host is not followed.
    let ev = FileSpec {
        url: format!("{}/resolve/evil", s.base),
        ..spec(&s, dir.path(), "evil", &data)
    };
    let e = fetch(&d, &ev).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelDownloadFailed, "{e:?}");
    assert!(!ev.dest.exists() && !ev.part.exists());
    // Production base must be https.
    assert_eq!(
        Downloader::new(DownloadConfig {
            base_url: "http://huggingface.co".into(),
            ..DownloadConfig::default()
        })
        .unwrap_err()
        .code,
        ErrorCode::InvalidInput
    );
}

#[tokio::test]
async fn cancel_keeps_the_part_for_later() {
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let data = body(50_000);
    s.put("/s", data.clone());
    s.plan(Behavior::Slow {
        chunk: 1_000,
        delay_ms: 20,
    });
    let d = Downloader::new(cfg(&s.base)).unwrap();
    let sp = spec(&s, dir.path(), "s", &data);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let e = d
        .fetch(&sp, false, &mut |_, _| {}, &cancel)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
    let have = std::fs::metadata(&sp.part).unwrap().len();
    assert!(have > 0 && have < 50_000, "{have}");
    assert!(
        matches!(fetch(&d, &sp).await.unwrap(), FetchOutcome::Done { fetched } if fetched == 50_000 - have)
    );
}

#[tokio::test]
async fn metered_connection_pauses_before_and_during_a_download() {
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let data = body(40_000);
    s.put("/mt", data.clone());
    let metered = Arc::new(AtomicBool::new(false));
    let m2 = metered.clone();
    let mut c = cfg(&s.base);
    c.is_metered = Arc::new(move || m2.load(Ordering::SeqCst));
    let d = Downloader::new(c).unwrap();
    let sp = spec(&s, dir.path(), "mt", &data);
    // Becomes metered mid-way (slow server, flag flipped after ~100 ms).
    s.plan(Behavior::Slow {
        chunk: 1_000,
        delay_ms: 10,
    });
    let m3 = metered.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        m3.store(true, Ordering::SeqCst);
    });
    let r = fetch(&d, &sp).await.unwrap();
    let FetchOutcome::PausedMetered { have } = r else {
        panic!("{r:?}")
    };
    assert!(have > 0 && have < 40_000);
    assert_eq!(std::fs::metadata(&sp.part).unwrap().len(), have);
    // "Download anyway" continues from the part.
    let r = d
        .fetch(&sp, true, &mut |_, _| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        r,
        FetchOutcome::Done {
            fetched: 40_000 - have
        }
    );
    assert_eq!(std::fs::read(&sp.dest).unwrap(), data);
}

// ------------------------------------------------------------------- tier-level (download_model)

/// `download_model` holds a process-wide one-download-per-file guard; tests that use the same pinned
/// file must not overlap.
static TIER_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn store_cfg(base: &str, metered: bool, free: u64) -> DownloadConfig {
    DownloadConfig {
        is_metered: Arc::new(move || metered),
        free_disk: Arc::new(move |_| Some(free)),
        ..cfg(base)
    }
}

#[tokio::test]
async fn download_model_requires_matching_consent_and_skips_bundled() {
    let _g = TIER_SERIAL.lock().await;
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = ModelStore::new(dir.path().join("models"));
    let d = Downloader::new(cfg(&s.base)).unwrap();
    let (sink, ev) = events();
    let c = CancellationToken::new();
    // Consent for the GPU file does not cover the CPU file.
    let consent = DownloadConsent::for_file(&STANDARD, &STANDARD.gpu_file);
    let e = d
        .download_model(&store, &STANDARD, &STANDARD.cpu_file, &consent, &sink, &c)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidInput);
    // A file that is not one of the tier's pinned variants is refused.
    let e = d
        .download_model(&store, &STANDARD, &MINIMUM.gpu_file, &consent, &sink, &c)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelIncompatible);
    // Minimum is bundled: nothing to download.
    let mc = DownloadConsent::for_file(&MINIMUM, &MINIMUM.gpu_file);
    let r = d
        .download_model(&store, &MINIMUM, &MINIMUM.gpu_file, &mc, &sink, &c)
        .await
        .unwrap();
    assert_eq!(r, DownloadOutcome::AlreadyPresent);
    assert!(s.log().is_empty() && ev.lock().unwrap().is_empty());
}

#[tokio::test]
async fn download_model_metered_and_disk_preflight_send_nothing() {
    let _g = TIER_SERIAL.lock().await;
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = ModelStore::new(dir.path().join("models"));
    let rec = recommendation(Tier::Standard, Tier::Minimum, true).unwrap();
    let consent = DownloadConsent::from_recommendation(&rec);
    let (sink, ev) = events();
    let c = CancellationToken::new();
    // Metered: paused before any request; no event (the app shows the metered choice).
    let d = Downloader::new(store_cfg(&s.base, true, 1000 * GIB)).unwrap();
    let r = d
        .download_model(&store, &STANDARD, &rec.file, &consent, &sink, &c)
        .await
        .unwrap();
    assert_eq!(
        r,
        DownloadOutcome::PausedMetered {
            have: 0,
            total: STANDARD.gpu_file.bytes
        }
    );
    // Not enough disk for model + shadow index + 2 GiB: DiskFull with a plain amount.
    let d = Downloader::new(store_cfg(&s.base, false, GIB)).unwrap();
    let e = d
        .download_model(&store, &STANDARD, &rec.file, &consent, &sink, &c)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::DiskFull);
    assert!(
        e.ctx.need.as_deref().unwrap_or("").ends_with("GB"),
        "{:?}",
        e.ctx.need
    );
    assert!(s.log().is_empty(), "no request was sent");
    let evs = ev.lock().unwrap().clone();
    assert!(
        matches!(
            evs.as_slice(),
            [EngineEvent::ModelDownloadFailed {
                code: ErrorCode::DiskFull,
                resumable: true,
                ..
            }]
        ),
        "{evs:?}"
    );
}

fn staged(name: &str) -> Option<PathBuf> {
    let d = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("models")
        .join(name);
    d.join("model_fp16.onnx").is_file().then_some(d)
}

/// End to end without internet: the real pinned Standard files are served from the local server
/// at HF's URL layout, downloaded (with one drop + resume), verified, renamed, then loaded and
/// used by the local embedder straight from the store.
#[tokio::test(flavor = "multi_thread")]
async fn standard_files_end_to_end_through_hf_url_layout() {
    let _g = TIER_SERIAL.lock().await;
    let Some(src) = staged("arctic-m-v1.5") else {
        assert!(
            std::env::var_os("SR_REQUIRE_MODELS").is_none(),
            "SR_REQUIRE_MODELS set but crates/providers/models/arctic-m-v1.5 is missing"
        );
        eprintln!("SKIP: Standard model files not staged");
        return;
    };
    let s = Server::start().await;
    let f = STANDARD.gpu_file;
    let pins = sr_providers::models::tokenizer_pins(Tier::Standard);
    s.put(
        &format!("/{}/resolve/{}/{}", f.hf_repo, f.hf_revision, f.path),
        std::fs::read(src.join("model_fp16.onnx")).unwrap(),
    );
    for a in pins.files() {
        s.put(
            &format!("/{}/resolve/{}/{}", pins.hf_repo, pins.hf_revision, a.name),
            std::fs::read(src.join(a.name)).unwrap(),
        );
    }
    // First model request drops at 50 MB (after the 4 tokenizer requests).
    for _ in 0..4 {
        s.plan(Behavior::Normal);
    }
    s.plan(Behavior::DropAfter(50 << 20));
    let dir = tempfile::tempdir().unwrap();
    let store = ModelStore::new(dir.path().join("models"));
    let d = Downloader::new(store_cfg(&s.base, false, 1000 * GIB)).unwrap();
    let (sink, ev) = events();
    let rec = recommendation(Tier::Standard, Tier::Minimum, true).unwrap();
    let consent = DownloadConsent::from_recommendation(&rec);
    let c = CancellationToken::new();
    let r = d
        .download_model(&store, &STANDARD, &f, &consent, &sink, &c)
        .await
        .unwrap();
    assert_eq!(
        r,
        DownloadOutcome::Completed {
            sha256: f.sha256.into(),
            fetched: f.bytes
        }
    );
    let log = s.log();
    assert_eq!(log.len(), 6, "4 tokenizer files + drop + resume: {log:?}");
    assert_eq!(
        log[5].1.as_deref(),
        Some(format!("bytes={}-", 50 << 20).as_str())
    );
    assert_eq!(
        store.state(&STANDARD, &f),
        sr_providers::download::FileState::Present
    );
    let evs = ev.lock().unwrap().clone();
    assert!(
        evs.iter()
            .any(|e| matches!(e, EngineEvent::ModelDownloadProgress { .. }))
    );
    assert!(
        matches!(evs.last(), Some(EngineEvent::ModelDownloadCompleted { sha256, .. }) if sha256 == f.sha256)
    );
    // Progress is monotonic and ends at the total.
    let mut last = 0;
    for e in &evs {
        if let EngineEvent::ModelDownloadProgress { bytes, total, .. } = e {
            assert!(*bytes >= last && *total == f.bytes);
            last = *bytes;
        }
    }
    assert_eq!(last, f.bytes);
    // Again: verified on disk, no request.
    let n = s.log().len();
    let r = d
        .download_model(&store, &STANDARD, &f, &consent, &sink, &c)
        .await
        .unwrap();
    assert_eq!(r, DownloadOutcome::AlreadyPresent);
    assert_eq!(s.log().len(), n);

    #[cfg(feature = "local")]
    {
        use sr_common::tier::EpPlan;
        use sr_providers::EmbedKind;
        use sr_providers::local::{LocalEmbedder, LocalOptions};
        let o = LocalOptions::new(&STANDARD, f, store.paths(&STANDARD, &f), EpPlan::Cpu);
        let e = LocalEmbedder::load(o, sr_providers::MemorySink::new())
            .await
            .expect("downloaded Standard model loads from the store");
        let v = e.embed_blocking(EmbedKind::Doc, &["hello"]).unwrap();
        assert_eq!(v[0].len(), 768);
    }
}

#[tokio::test]
async fn one_download_per_file_at_a_time() {
    let _g = TIER_SERIAL.lock().await;
    let s = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = ModelStore::new(dir.path().join("models"));
    let f = STANDARD.gpu_file;
    // Tokenizer requests stall so the first call is still running when the second starts.
    for _ in 0..4 {
        s.plan(Behavior::Slow {
            chunk: 1,
            delay_ms: 50,
        });
    }
    let pins = sr_providers::models::tokenizer_pins(Tier::Standard);
    for a in pins.files() {
        s.put(
            &format!("/{}/resolve/{}/{}", pins.hf_repo, pins.hf_revision, a.name),
            b"x".repeat(64),
        );
    }
    let d = Arc::new(Downloader::new(store_cfg(&s.base, false, 1000 * GIB)).unwrap());
    let consent = DownloadConsent::for_file(&STANDARD, &f);
    let (sink, _) = events();
    let c = CancellationToken::new();
    let (d2, st2, co2, si2, c2) = (
        d.clone(),
        store.clone(),
        consent.clone(),
        sink.clone(),
        c.clone(),
    );
    let first = tokio::spawn(async move {
        d2.download_model(&st2, &STANDARD, &f, &co2, &si2, &c2)
            .await
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let e = d
        .download_model(&store, &STANDARD, &f, &consent, &sink, &c)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Busy);
    c.cancel();
    let _ = first.await;
}
