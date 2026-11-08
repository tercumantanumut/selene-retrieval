//! Fuzz harness (cargo-fuzz needs nightly + a sanitizer runtime that MSVC stable lacks, so this
//! is proptest over arbitrary and mutated inputs). Case counts scale with `SR_FUZZ_CASES`.
//!
//! * `parsers_never_panic_in_process`: every non-PDF parser on random and mutated documents,
//!   in-process: no panic, output within the cap. (pdfium is native code: it is fuzzed only
//!   behind the sandbox, below.)
//! * `supervisor_survives_arbitrary_files`: random and mutated documents incl. PDFs through the
//!   real sandboxed worker: every result is `Ok` or a typed extract error, the pool keeps working,
//!   no worker leaks.
//! * `supervisor_survives_arbitrary_worker_output` (test-probes): the worker answers with random
//!   bytes; the supervisor must classify, kill and recover every time.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};

use common::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use sr_common::ErrorCode;
use sr_common::protocol::extract::DocKind;
use sr_extract_client::{ExtractPool, ExtractSource, SandboxMode};

const KINDS: [DocKind; 9] = [
    DocKind::Auto,
    DocKind::Text,
    DocKind::Markdown,
    DocKind::Code,
    DocKind::Html,
    DocKind::Docx,
    DocKind::Xlsx,
    DocKind::Pptx,
    DocKind::Pdf,
];

fn cases(default: u32) -> u32 {
    std::env::var("SR_FUZZ_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn seeds() -> Vec<Vec<u8>> {
    vec![
        sample_docx(),
        sample_xlsx(),
        sample_pptx(),
        sample_pdf(2),
        b"<!doctype html><html><body><p>Hi &amp; <b>there</b></p><script>x</script></body></html>"
            .to_vec(),
        "plain text ✓ with é and \r\n lines".as_bytes().to_vec(),
        b"\xFF\xFEh\0i\0".to_vec(),
    ]
}

/// A document built from a seed with byte flips, an optional truncation and an optional splice.
fn mutated() -> impl Strategy<Value = Vec<u8>> {
    let n = seeds().len();
    (
        0..n,
        prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..24),
        prop::option::of(any::<prop::sample::Index>()),
        prop::option::of((
            any::<prop::sample::Index>(),
            prop::collection::vec(any::<u8>(), 1..64),
        )),
    )
        .prop_map(|(i, flips, cut, splice)| {
            let mut d = seeds().swap_remove(i);
            for (at, b) in flips {
                let at = at.index(d.len());
                d[at] = b;
            }
            if let Some((at, bytes)) = splice {
                let at = at.index(d.len());
                d.splice(at..at, bytes);
            }
            if let Some(c) = cut {
                d.truncate(c.index(d.len() + 1));
            }
            d
        })
}

fn input() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..4096),
        mutated(),
        mutated(),
    ]
}

#[test]
fn parsers_never_panic_in_process() {
    let mut runner = TestRunner::new(Config {
        cases: cases(4000),
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(
            &(input(), 0..KINDS.len() - 1, 1usize..4096),
            |(bytes, k, cap)| {
                let kind = KINDS[k];
                let r = catch_unwind(AssertUnwindSafe(|| {
                    sr_extract::extract(kind, &bytes, cap, None)
                }));
                prop_assert!(
                    r.is_ok(),
                    "parser panicked on {kind:?} ({} bytes)",
                    bytes.len()
                );
                if let Ok(Ok(x)) = r {
                    prop_assert!(x.text.len() <= cap, "text {} > cap {cap}", x.text.len());
                }
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn supervisor_survives_arbitrary_files() {
    let (exe, dll) = stage("fuzz-files");
    let prof = Profile::new("fuzz-files");
    let mut cfg = config(&exe, &dll, SandboxMode::AppContainer, &prof.0);
    cfg.workers = 2;
    let pool = ExtractPool::start(cfg).unwrap();
    let rt = rt();
    let mut runner = TestRunner::new(Config {
        cases: cases(300),
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&(input(), 0..KINDS.len()), |(bytes, k)| {
            let r = rt.block_on(pool.extract(ExtractSource::Bytes(bytes), KINDS[k]));
            if let Err(e) = &r {
                prop_assert!(
                    matches!(
                        e.code(),
                        ErrorCode::FileUnreadable | ErrorCode::ExtractTimeout
                    ),
                    "unexpected {e:?}"
                );
            }
            Ok(())
        })
        .unwrap();
    let ok = rt
        .block_on(pool.extract(ExtractSource::Bytes(sample_pdf(1)), DocKind::Pdf))
        .unwrap();
    assert!(ok.text.contains("page 1"));
    let s = pool.stats();
    println!("fuzz files: {s:?}");
    assert_eq!(s.protocol_violations, 0);
    pool.shutdown();
    assert!(
        pool.leaked_workers().is_empty(),
        "{:?}",
        pool.leaked_workers()
    );
}

#[cfg(feature = "test-probes")]
#[test]
fn supervisor_survives_arbitrary_worker_output() {
    let (exe, dll) = stage("fuzz-proto");
    let prof = Profile::new("fuzz-proto");
    let mut cfg = config(&exe, &dll, SandboxMode::AppContainer, &prof.0);
    cfg.workers = 1;
    quick_timeouts(&mut cfg, std::time::Duration::from_secs(3));
    let pool = ExtractPool::start(cfg).unwrap();
    let rt = rt();
    // Random output, and random output behind a plausible length prefix / JSON start.
    let garbage = prop_oneof![
        prop::collection::vec(any::<u8>(), 0..512),
        prop::collection::vec(any::<u8>(), 0..256).prop_map(|mut v| {
            let mut out = (v.len() as u32).to_le_bytes().to_vec();
            out.append(&mut v);
            out
        }),
        "[ -~]{0,200}".prop_map(|s| {
            let body = format!("{{\"id\":1,\"result\":{{\"status\":\"ok\",{s}");
            let mut out = (body.len() as u32).to_le_bytes().to_vec();
            out.extend_from_slice(body.as_bytes());
            out
        }),
    ];
    let mut runner = TestRunner::new(Config {
        cases: cases(150),
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&garbage, |g| {
            let mut payload = b"SR-PROBE:garbage:".to_vec();
            payload.extend_from_slice(&g);
            let r = rt.block_on(pool.extract(ExtractSource::Bytes(payload), DocKind::Text));
            let e = r.expect_err("garbage can never be a valid result");
            prop_assert_eq!(e.code(), ErrorCode::FileUnreadable, "{:?}", e);
            Ok(())
        })
        .unwrap();
    let ok = rt
        .block_on(pool.extract(ExtractSource::Bytes(sample_pdf(1)), DocKind::Pdf))
        .unwrap();
    assert!(ok.text.contains("page 1"));
    println!("fuzz proto: {:?}", pool.stats());
    pool.shutdown();
    assert!(
        pool.leaked_workers().is_empty(),
        "{:?}",
        pool.leaked_workers()
    );
}
