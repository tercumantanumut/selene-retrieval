//! The hostile corpus through the real sandboxed worker, under both sandbox modes (Windows) or
//! the Seatbelt sandbox (macOS).
//!
//! Pass criteria per mode: every case lands in its allowed outcome set, a legit PDF extracts
//! correctly after every case (the pool recovers), no protocol violations or spawn failures,
//! and after shutdown no worker process is left alive. A markdown table is written to
//! `<CARGO_TARGET_TMPDIR>/hostile-<mode>.md` for the report.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fmt::Write as _;
use std::time::Instant;

use common::*;
use sr_common::ErrorCode;
use sr_common::protocol::extract::DocKind;
use sr_extract_client::{ExtractPool, ExtractSource, SandboxMode};

fn run(mode: SandboxMode, tag: &str) {
    let (dir, cases) = corpus();
    let (exe, dll) = stage(&format!("hostile-{tag}"));
    let prof = Profile::new(&format!("hostile-{tag}"));
    let mut cfg = config(&exe, &dll, mode, &prof.0);
    cfg.workers = 1;
    let pool = ExtractPool::start(cfg).unwrap();
    let rt = rt();
    let legit = sample_pdf(3);
    let mut table = String::from(
        "| # | case | KiB | outcome | detail | ms | legit after | pass |\n|---|---|---|---|---|---|---|---|\n",
    );
    let mut fails = Vec::new();

    // Oversize: refused before any worker is involved.
    let big = tmp_root().join(format!("oversize-{tag}.txt"));
    std::fs::File::create(&big)
        .unwrap()
        .set_len(201 << 20)
        .unwrap();
    let mut all: Vec<(String, std::path::PathBuf, DocKind, Expect)> = cases
        .iter()
        .map(|c| (c.name.to_string(), dir.join(c.name), c.kind, c.expect))
        .collect();
    all.push((
        "oversize.txt (201 MiB, sparse)".into(),
        big.clone(),
        DocKind::Text,
        Expect::Code(ErrorCode::FileTooLarge),
    ));

    for (i, (name, path, kind, expect)) in all.into_iter().enumerate() {
        let kib = std::fs::metadata(&path)
            .map(|m| m.len() / 1024)
            .unwrap_or(0);
        let t = Instant::now();
        let r = rt.block_on(pool.extract(ExtractSource::Path(path), kind));
        let ms = t.elapsed().as_millis();
        let (label, detail, pass) = match &r {
            Ok(x) => {
                let sample: String = x
                    .text
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(40)
                    .collect();
                let pass = match expect {
                    Expect::OkContains(s) => x.text.contains(s),
                    Expect::Ok | Expect::Contained => true,
                    Expect::Code(_) => false,
                };
                (
                    "Ok".to_string(),
                    format!(
                        "{} chars{}{} \"{}\"",
                        x.text.chars().count(),
                        x.pages.map(|p| format!(", {p} pages")).unwrap_or_default(),
                        if x.truncated { ", truncated" } else { "" },
                        sample
                    ),
                    pass,
                )
            }
            Err(e) => {
                let pass = match expect {
                    Expect::Code(c) => e.code() == c,
                    Expect::Contained => matches!(
                        e.code(),
                        ErrorCode::FileUnreadable | ErrorCode::ExtractTimeout
                    ),
                    _ => false,
                };
                (
                    format!("{} ({:?})", e.code(), e.disposition),
                    format!(
                        "{:?} — {}",
                        e.cause,
                        e.error.ctx.detail.clone().unwrap_or_default()
                    ),
                    pass,
                )
            }
        };
        let after = rt
            .block_on(pool.extract(ExtractSource::Bytes(legit.clone()), DocKind::Pdf))
            .map(|x| x.text.contains("Selene retrieval spike page 1"))
            .unwrap_or(false);
        let pass = pass && after;
        if !pass {
            fails.push(format!("{name}: {label} {detail}"));
        }
        let detail: String = detail
            .replace(['|', '\n', '\r'], " ")
            .chars()
            .take(110)
            .collect();
        writeln!(
            table,
            "| {} | {name} | {kib} | {label} | {detail} | {ms} | {} | {} |",
            i + 1,
            if after { "yes" } else { "NO" },
            if pass { "ok" } else { "**FAIL**" }
        )
        .unwrap();
    }
    let stats = pool.stats();
    let pids = pool.spawned_pids();
    pool.shutdown();
    let leaked: Vec<u32> = pool.leaked_workers().iter().map(|w| w.pid).collect();
    writeln!(
        table,
        "\nstats: {stats:?}\nworkers started: {}; leaked after shutdown: {leaked:?}",
        pids.len()
    )
    .unwrap();
    std::fs::write(tmp_root().join(format!("hostile-{tag}.md")), &table).unwrap();
    let _ = std::fs::remove_file(&big);
    println!("{table}");
    assert!(fails.is_empty(), "failures: {fails:#?}");
    assert!(leaked.is_empty(), "leaked workers {leaked:?}");
    assert_eq!(stats.protocol_violations, 0);
    assert_eq!(stats.spawn_failures, 0);
    #[cfg(target_os = "macos")]
    assert_eq!(
        stats.sandbox,
        Some(sr_extract_client::ActiveSandbox::Seatbelt)
    );
}

#[cfg(windows)]
#[test]
fn hostile_corpus_app_container() {
    run(SandboxMode::AppContainer, "ac");
}

#[cfg(windows)]
#[test]
fn hostile_corpus_low_integrity() {
    run(SandboxMode::LowIntegrity, "low");
}

#[cfg(target_os = "macos")]
#[test]
fn hostile_corpus_seatbelt() {
    run(SandboxMode::Auto, "seatbelt");
}
