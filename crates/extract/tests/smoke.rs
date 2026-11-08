//! Basic round trips through the real sandboxed worker, in both sandbox modes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use sr_common::protocol::extract::DocKind;
use sr_extract_client::{ActiveSandbox, ExtractPool, ExtractSource, SandboxMode};

fn round_trips(mode: SandboxMode, stage_name: &str, profile: &str) {
    let (exe, dll) = stage(stage_name);
    let prof = Profile::new(profile);
    let pool = ExtractPool::start(config(&exe, &dll, mode, &prof.0)).unwrap();
    let rt = rt();
    rt.block_on(async {
        let r = pool
            .extract(ExtractSource::Bytes(sample_pdf(3)), DocKind::Pdf)
            .await
            .unwrap();
        assert!(
            r.text.contains("Selene retrieval spike page 1"),
            "{:?}",
            r.text
        );
        assert_eq!(r.pages, Some(3));
        let want = match mode {
            SandboxMode::LowIntegrity => ActiveSandbox::LowIntegrity,
            _ => ActiveSandbox::AppContainer,
        };
        assert_eq!(
            r.sandbox,
            want,
            "fallback: {:?}",
            pool.stats().fallback_reason
        );

        let r = pool
            .extract(ExtractSource::Bytes(sample_docx()), DocKind::Docx)
            .await
            .unwrap();
        assert!(r.text.contains("Hello DOCX world."));
        let r = pool
            .extract(
                ExtractSource::Bytes(b"line 1\r\nline 2".to_vec()),
                DocKind::Code,
            )
            .await
            .unwrap();
        assert_eq!(r.text, "line 1\r\nline 2");
    });
    let pids = pool.spawned_pids();
    assert!(!pids.is_empty());
    pool.shutdown();
    assert!(
        pool.leaked_workers().is_empty(),
        "leaked {:?}",
        pool.leaked_workers()
    );
}

#[test]
fn app_container_round_trips() {
    round_trips(SandboxMode::AppContainer, "smoke-ac", "smoke-ac");
}

#[test]
fn low_integrity_round_trips() {
    round_trips(SandboxMode::LowIntegrity, "smoke-low", "smoke-low");
}
