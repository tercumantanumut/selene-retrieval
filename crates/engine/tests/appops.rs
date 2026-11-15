//! App-RPC surfaces wired by the M2 tester: the wizard's pre-scan estimate (M2-A R1),
//! pause / resume / cancel, approvals list, notice dismiss, search-model status, and the status
//! snapshot's folder path (R3) and tier (R2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use serde_json::json;
use sr_common::error::ErrorCode;
use sr_common::events::FolderStatus;
use sr_engine::{Engine, ExtractSetup};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_rpc_estimate_folder_controls_and_snapshot_fields() {
    let t = tmp();
    let data = t.path().join("d");
    let corpus = t.path().join("corpus");
    let words = "word ".repeat(400);
    write_files(
        &corpus,
        &[
            ("notes.md", words.as_str()),
            ("src/main.rs", "fn main() {}\n"),
            (".env", "SECRET=1\n"),
            ("node_modules/pkg/index.js", "module.exports = 1;\n"),
            ("photo.jpg", "not text"),
        ],
    );
    let e = Engine::start(config(&data, ExtractSetup::Disabled))
        .await
        .unwrap();

    // Estimate: the scan's own filters (secret denylist, default excludes, file-type preset).
    let est = e
        .rpc("folders.estimate", json!({ "path": corpus }))
        .await
        .unwrap();
    assert_eq!(est["files"], 2, "{est}");
    assert_eq!(est["capped"], false, "{est}");
    assert!(est["eta_s"].as_u64().unwrap() >= 1, "{est}");
    assert_eq!(est["gpu"], false, "{est}");
    // A refused root gets the add path's code (the app shows "Choose a subfolder").
    #[cfg(windows)]
    let win = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    #[cfg(not(windows))]
    let win = "/System".to_string();
    let err = e
        .rpc("folders.estimate", json!({ "path": win }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PathRefused, "{err}");
    let err = e
        .rpc("folders.estimate", json!({ "path": "relative\\dir" }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput);
    let err = e
        .rpc(
            "folders.estimate",
            json!({ "path": t.path().join("missing") }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::FolderUnavailable, "{err}");
    // Nothing was added by estimating.
    assert!(e.list_folders().await.unwrap().is_empty());

    // Snapshot: folder path + skipped count (R3) and the tier (R2).
    let f = e.add_folder(&corpus.to_string_lossy(), None).await.unwrap();
    let s = e.status();
    assert_eq!(s.value.folders[0].path.as_deref(), Some(f.path.as_str()));
    let tier = s.value.tier.expect("tier in the snapshot");
    assert!(!tier.gpu);

    // Pause / resume / cancel.
    e.rpc("folders.pause", json!({ "folder_id": f.folder_id }))
        .await
        .unwrap();
    assert_eq!(
        e.list_folders().await.unwrap()[0].status,
        FolderStatus::Paused
    );
    let r = e
        .rpc("folders.resume", json!({ "folder_id": f.folder_id }))
        .await
        .unwrap();
    assert_eq!(r["resumed"], true, "{r}");
    assert_ne!(
        e.list_folders().await.unwrap()[0].status,
        FolderStatus::Paused
    );
    let c = e
        .rpc("folders.cancel", json!({ "folder_id": f.folder_id }))
        .await
        .unwrap();
    assert!(c["dropped"].is_u64() && c["stopped"].is_u64(), "{c}");

    // Approvals (MCP add_folder is off by default: nothing pending), notices, search model.
    assert_eq!(e.rpc("approvals.list", json!({})).await.unwrap(), json!([]));
    assert_eq!(
        e.rpc("notices.dismiss", json!({})).await.unwrap()["dismissed"],
        0
    );
    let m = e.rpc("search_model.status", json!({})).await.unwrap();
    assert!(m["active"].is_string() && m["phase"].is_object(), "{m}");

    // Advanced settings: defaults (GPU Auto, add_folder and sync_folder off), then a change is
    // saved to settings.toml and the add_folder and sync_folder switches apply at once.
    let a = e.rpc("settings.advanced", json!({})).await.unwrap();
    assert_eq!(a["use_graphics_card"], true, "{a}");
    assert_eq!(a["mcp_add_folder_enabled"], false, "{a}");
    assert_eq!(a["mcp_sync_folder_enabled"], false, "{a}");
    assert!(!e.tool_flags().sync_folder);
    let a = e
        .rpc(
            "settings.update_advanced",
            json!({ "use_graphics_card": false, "mcp_add_folder_enabled": true,
                    "mcp_sync_folder_enabled": true }),
        )
        .await
        .unwrap();
    assert_eq!(a["use_graphics_card"], false, "{a}");
    assert_eq!(a["restart_needed"], true, "{a}");
    assert_eq!(a["mcp_sync_folder_enabled"], true, "{a}");
    assert!(e.mcp_add_folder_enabled());
    assert!(e.mcp_sync_folder_enabled());
    let f = e.tool_flags();
    assert!(f.add_folder && f.sync_folder, "{f:?}");
    let toml = std::fs::read_to_string(data.join("settings.toml")).unwrap();
    assert!(toml.contains("use_graphics_card = false"), "{toml}");
    assert!(toml.contains("mcp_add_folder_enabled = true"), "{toml}");
    assert!(toml.contains("mcp_sync_folder_enabled = true"), "{toml}");
    e.drain().await;
    drop(e);
    // The saved choice survives a restart.
    let e2 = Engine::start(config(&data, ExtractSetup::Disabled))
        .await
        .unwrap();
    assert!(e2.mcp_add_folder_enabled());
    assert!(e2.mcp_sync_folder_enabled());
    assert!(!e2.advanced_settings().use_graphics_card);
    e2.drain().await;
}

/// M2 tester: a removed folder is unsearchable at once; until its purge (≤ 15 min) its chunks are
/// still in the stores, and their scores must not vouch for unrelated live hits. With the bundled
/// model's floors, a query only the removed folder matched is "no good match", not hits from the
/// other folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_folder_hits_never_count_as_evidence_for_unrelated_results() {
    use sr_core::query::{NoMatchCutoff, NoMatchMode};
    let t = tmp();
    let data = t.path().join("d");
    let gone = t.path().join("gone");
    let kept = t.path().join("kept");
    write_files(
        &gone,
        &[(
            "canary.md",
            "# Canary\n\nThe purgecanarytoken lives only here.\n",
        )],
    );
    write_files(
        &kept,
        &[
            ("a.md", "# Gardening\n\nTomatoes like sun and compost.\n"),
            ("b.md", "# Cooking\n\nBake the bread for forty minutes.\n"),
        ],
    );
    let profile = Profile::new("m2t_stale");
    let mut cfg = config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &profile))),
    );
    cfg.query = Some(sr_core::query::QueryConfig {
        // The real bge-small floors (the test embedder is uncalibrated, so Model mode is off).
        no_match: NoMatchMode::Fixed(NoMatchCutoff::BGE_SMALL),
        ..sr_engine::query_config(&sr_common::config::Settings::default())
    });
    let e = Engine::start(cfg).await.unwrap();
    let g = e.add_folder(&gone.to_string_lossy(), None).await.unwrap();
    e.add_folder(&kept.to_string_lossy(), None).await.unwrap();
    e.run_pending_jobs().await.unwrap();
    e.drain_writer_now().await.unwrap();
    let args = |q: &str| sr_engine::SearchArgs {
        query: q.into(),
        max_results: Some(10),
        min_score: Some(0.0),
        folder_ids: None,
        mode: None,
        collapse_duplicates: None,
    };
    let before = e.search(args("purgecanarytoken")).await.unwrap();
    assert!(
        before
            .hits
            .iter()
            .any(|h| h.rel_path.ends_with("canary.md")),
        "the exact token is found while the folder exists: {:?}",
        before.hits.iter().map(|h| &h.rel_path).collect::<Vec<_>>()
    );
    e.remove_folder(g.folder_id).await.unwrap();
    let after = e.search(args("purgecanarytoken")).await.unwrap();
    assert!(
        after.hits.is_empty() && after.no_good_match,
        "{:?}",
        after.hits.iter().map(|h| &h.rel_path).collect::<Vec<_>>()
    );
    e.drain().await;
}
