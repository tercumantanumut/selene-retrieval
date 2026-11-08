//! Throughput figures for the report (`cargo test --release -p sr-extract --test throughput --
//! --ignored --nocapture`). Small PDFs, DOCX and source files through the AppContainer pool with
//! 1 and 2 workers, `buffer_unordered` keeping every permit busy.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Instant;

use common::*;
use sr_common::protocol::extract::DocKind;
use sr_extract_client::{ExtractPool, ExtractSource, SandboxMode};

fn bench(pool: &ExtractPool, label: &str, doc: &[u8], kind: DocKind, n: usize) {
    let rt = rt();
    rt.block_on(async {
        // warm-up
        pool.extract(ExtractSource::Bytes(doc.to_vec()), kind)
            .await
            .unwrap();
        let t = Instant::now();
        let mut set = Vec::with_capacity(n);
        for _ in 0..n {
            set.push(pool.extract(ExtractSource::Bytes(doc.to_vec()), kind));
        }
        let res = futures_join_all(set).await;
        let s = t.elapsed().as_secs_f64();
        let ok = res.iter().filter(|r| r.is_ok()).count();
        println!(
            "{label:32} {n} files in {:.2} s = {:.0} files/s ({:.2} ms/file), {ok}/{n} ok",
            s,
            n as f64 / s,
            s * 1000.0 / n as f64
        );
        assert_eq!(ok, n);
    });
}

/// Minimal join_all (no `futures` dev-dependency): polls every pending future on one task.
async fn futures_join_all<F: std::future::Future>(fs: Vec<F>) -> Vec<F::Output> {
    let mut pending: Vec<_> = fs.into_iter().map(|f| Some(Box::pin(f))).collect();
    let mut out = Vec::with_capacity(pending.len());
    std::future::poll_fn(|cx| {
        let mut done = true;
        for slot in pending.iter_mut() {
            if let Some(f) = slot {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => {
                        out.push(v);
                        *slot = None;
                    }
                    std::task::Poll::Pending => done = false,
                }
            }
        }
        if done {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    out
}

#[test]
#[ignore = "benchmark: run in release with --ignored --nocapture"]
fn throughput() {
    let (exe, dll) = stage("bench");
    let prof = Profile::new("bench");
    let pdf3 = sample_pdf(3);
    let pdf1 = sample_pdf(1);
    let docx = sample_docx();
    let code = "fn main() {\n    println!(\"hello\");\n}\n"
        .repeat(100)
        .into_bytes();
    for workers in [1, 2] {
        let mut cfg = config(&exe, &dll, SandboxMode::AppContainer, &prof.0);
        cfg.workers = workers;
        cfg.queue = workers * 4;
        let pool = ExtractPool::start(cfg).unwrap();
        println!(
            "--- {workers} worker(s), sandbox {:?}",
            pool.stats().sandbox
        );
        bench(
            &pool,
            "small PDF (1 page, 0.7 KB)",
            &pdf1,
            DocKind::Pdf,
            2000,
        );
        bench(
            &pool,
            "small PDF (3 pages, 1.5 KB)",
            &pdf3,
            DocKind::Pdf,
            2000,
        );
        bench(&pool, "DOCX (0.9 KB)", &docx, DocKind::Docx, 2000);
        bench(&pool, "source file (3.6 KB)", &code, DocKind::Code, 2000);
        pool.shutdown();
    }
}

#[test]
#[ignore = "diagnostic"]
fn latency_breakdown() {
    let (exe, dll) = stage("bench");
    let prof = Profile::new("bench-lat");
    for mode in [SandboxMode::AppContainer, SandboxMode::LowIntegrity] {
        let mut cfg = config(&exe, &dll, mode, &prof.0);
        cfg.workers = 1;
        let pool = ExtractPool::start(cfg).unwrap();
        let rt = rt();
        let doc = b"hello world".to_vec();
        rt.block_on(pool.extract(ExtractSource::Bytes(doc.clone()), DocKind::Text))
            .unwrap();
        let mut wall = Vec::new();
        let mut inner = Vec::new();
        for _ in 0..50 {
            let t = Instant::now();
            let r = rt
                .block_on(pool.extract(ExtractSource::Bytes(doc.clone()), DocKind::Text))
                .unwrap();
            wall.push(t.elapsed().as_micros());
            inner.push(r.elapsed.as_micros());
        }
        wall.sort();
        inner.sort();
        println!(
            "{mode:?}: wall p50 {} us, slot-side p50 {} us",
            wall[25], inner[25]
        );
        pool.shutdown();
    }
}
