//! M4-E engine features (docs/research/m4-rpc-contract.md): folder groups and per-client scopes,
//! folder detail (skipped list, retry, settings), activity, HTTP tokens, request cancel, the
//! index move (check, plan, copy + switch + reopen), and the R31 trusted-image rule.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde_json::json;
use sr_catalog::repo::files;
use sr_common::api::{ActivityKind, ClientScope, MoveProblem};
use sr_common::error::{ClientKind, ErrorCode};
use sr_engine::{Engine, ExtractSetup, Scope};
use sr_providers::{MemorySecretStore, SecretStore};

async fn engine(data: &std::path::Path) -> sr_engine::EngineHandle {
    Engine::start(config(data, ExtractSetup::Disabled))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn groups_scope_clients_and_fail_closed() {
    let t = tmp();
    let data = t.path().join("d");
    let (a, b) = (t.path().join("a"), t.path().join("b"));
    write_files(&a, &[("one.md", "alpha")]);
    write_files(&b, &[("two.md", "beta")]);
    let e = engine(&data).await;
    let fa = e.add_folder(&a.to_string_lossy(), None).await.unwrap();
    let fb = e.add_folder(&b.to_string_lossy(), None).await.unwrap();
    let def = e.default_group();
    assert_eq!(fa.group_id, def, "new folders go to the default group");

    // A group, and folder b moved into it (no re-index: only a label).
    let g = e
        .rpc("groups.create", json!({ "name": "Work" }))
        .await
        .unwrap();
    let gid: sr_common::ids::GroupId = serde_json::from_value(g["group_id"].clone()).unwrap();
    assert_eq!(
        e.rpc("groups.create", json!({ "name": "work" }))
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
    e.rpc(
        "groups.move_folder",
        json!({ "folder_id": fb.folder_id, "group_id": gid }),
    )
    .await
    .unwrap();
    let list = e.list_folders().await.unwrap();
    assert_eq!(
        list.iter()
            .find(|f| f.folder_id == fb.folder_id)
            .unwrap()
            .group_id,
        gid
    );
    assert_eq!(
        e.status()
            .value
            .folders
            .iter()
            .find(|f| f.folder_id == fb.folder_id)
            .unwrap()
            .group_id,
        gid
    );

    // A pipe app appears on its first session, with "All my folders".
    let id = e.client_session_opened("Claude").await;
    assert_eq!(id, "claude");
    assert_eq!(e.client_scope("claude").await.unwrap(), Scope::All);
    let clients = e.clients_list().await.unwrap();
    let c = clients
        .clients
        .iter()
        .find(|c| c.client_id == "claude")
        .unwrap();
    assert_eq!(c.kind, ClientKind::ClaudeDesktop);
    assert_eq!(c.display_name, "Claude Desktop");
    assert_eq!(c.sessions_open, 1);
    // An app never seen gets "All my folders" too (decision 6).
    assert_eq!(e.client_scope("cursor").await.unwrap(), Scope::All);

    // Limit Claude to the group: it sees folder b only.
    e.rpc(
        "clients.set_scope",
        json!({ "client_id": "claude", "scope": { "mode": "groups", "group_ids": [gid] } }),
    )
    .await
    .unwrap();
    let s = e.client_scope("claude").await.unwrap();
    assert!(s.allows(fb.folder_id) && !s.allows(fa.folder_id));
    assert_eq!(
        s.restrict(Some(vec![fa.folder_id])).unwrap_err().code,
        ErrorCode::NotFound
    );
    // An unknown group is refused, never stored.
    assert_eq!(
        e.set_client_scope(
            "claude".into(),
            ClientScope::Groups {
                group_ids: vec![sr_common::ids::GroupId::new()]
            }
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InvalidInput
    );

    // Deleting the group: folder b goes back to "My folders", Claude loses access (no widening).
    let r = e
        .rpc("groups.delete", json!({ "group_id": gid }))
        .await
        .unwrap();
    assert_eq!(r["moved_folders"], 1, "{r}");
    assert_eq!(r["clients_without_access"][0]["client_id"], "claude", "{r}");
    assert!(e.client_scope("claude").await.unwrap().is_empty());
    assert_eq!(
        e.list_folders().await.unwrap().len(),
        2,
        "nothing left search"
    );
    assert_eq!(
        e.rpc("groups.delete", json!({ "group_id": def }))
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput,
        "the default group stays"
    );

    e.client_session_closed("claude");
    let c = e.clients_list().await.unwrap();
    assert_eq!(
        c.clients
            .iter()
            .find(|c| c.client_id == "claude")
            .unwrap()
            .sessions_open,
        0
    );
    // Snippets: stdio ones never carry a token.
    let sn = e
        .rpc("clients.snippet", json!({ "kind": "cursor" }))
        .await
        .unwrap();
    assert!(sn["text"].as_str().unwrap().contains("--client"), "{sn}");
    assert!(!sn["text"].as_str().unwrap().contains("Bearer"));
    e.drain().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folder_detail_skipped_retry_and_settings() {
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("docs");
    write_files(&root, &[("ok.md", "fine"), ("bad.pdf", "%PDF broken")]);
    let e = engine(&data).await;
    let f = e.add_folder(&root.to_string_lossy(), None).await.unwrap();
    // Put one dead and one too-large file in the catalog directly (no extract worker here).
    let fid = f.folder_id;
    let (bad, big) = e
        .catalog()
        .write_blocking(move |c| {
            let scan = files::begin_scan(c, fid, 1)?;
            let mut o = files::FileObservation {
                rel_path: "bad.pdf".into(),
                path_key: "bad.pdf".into(),
                ext: "pdf".into(),
                volume_serial: None,
                ntfs_file_id: None,
                size: 11,
                mtime_ns: 1,
            };
            let bad = files::observe(c, fid, scan, &o, 1)?.file_id;
            o.rel_path = "big.bin".into();
            o.path_key = "big.bin".into();
            o.ext = "bin".into();
            let big = files::observe(c, fid, scan, &o, 1)?.file_id;
            files::mark_dead(c, bad, ErrorCode::FileUnreadable, 2)?;
            files::mark_skipped(c, big, "too_large", Some(ErrorCode::FileTooLarge), 3)?;
            Ok((bad, big))
        })
        .unwrap();
    let d = e
        .rpc("folders.detail", json!({ "folder_id": fid }))
        .await
        .unwrap();
    assert_eq!(d["skipped_total"], 2, "{d}");
    assert_eq!(d["group_name"], "My folders", "{d}");
    let rows = d["skipped"].as_array().unwrap();
    let bad_row = rows
        .iter()
        .find(|r| r["file_id"] == json!(bad.to_string()))
        .unwrap();
    assert_eq!(bad_row["reason"], "unreadable");
    assert_eq!(bad_row["retryable"], true);
    let big_row = rows
        .iter()
        .find(|r| r["file_id"] == json!(big.to_string()))
        .unwrap();
    assert_eq!(big_row["reason"], "too_large");
    assert_eq!(big_row["retryable"], false);
    let page = e
        .rpc(
            "folders.skipped",
            json!({ "folder_id": fid, "offset": 1, "limit": 1 }),
        )
        .await
        .unwrap();
    assert_eq!(page["total"], 2);
    assert_eq!(page["files"].as_array().unwrap().len(), 1);

    let r = e
        .rpc("folders.retry_files", json!({ "folder_id": fid }))
        .await
        .unwrap();
    assert_eq!(r["queued"], 1, "only the dead file: {r}");

    // Settings: out of bounds is refused; a name change doesn't re-index.
    let mut s = e.folder_detail(fid).await.unwrap().settings;
    s.max_file_bytes = 10;
    assert_eq!(
        e.update_folder_settings(fid, s.clone())
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
    s.max_file_bytes = 10 * 1024 * 1024;
    s.display_name = "Docs".into();
    let (reindex, detail) = e.update_folder_settings(fid, s).await.unwrap();
    assert!(!reindex);
    assert_eq!(detail.folder.name, "Docs");
    assert_eq!(
        e.rpc(
            "folders.detail",
            json!({ "folder_id": sr_common::ids::FolderId::new() })
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::NotFound
    );
    e.drain().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn activity_lines_are_recorded_and_paged() {
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("docs");
    write_files(&root, &[("a.md", "x")]);
    let e = engine(&data).await;
    let mut rx = e.bus().subscribe();
    let f = e.add_folder(&root.to_string_lossy(), None).await.unwrap();
    let page = e.activity_list(None, None).await.unwrap();
    assert!(
        page.entries
            .iter()
            .any(|a| a.kind == ActivityKind::FolderAdded && a.folder_id == Some(f.folder_id)),
        "{page:?}"
    );
    // Pushed as `activity.added`.
    let mut pushed = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        if matches!(
            ev.payload,
            sr_common::events::EngineEvent::ActivityAdded { .. }
        ) {
            pushed = true;
            break;
        }
    }
    assert!(pushed);
    e.remove_folder(f.folder_id).await.unwrap();
    let mut removed = false;
    for _ in 0..50 {
        let p = e.activity_list(None, None).await.unwrap();
        if p.entries
            .iter()
            .any(|a| a.kind == ActivityKind::FolderRemoved)
        {
            removed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(removed);
    let id = e
        .rpc(
            "activity.note",
            json!({ "kind": "client_connected", "client_kind": "cursor" }),
        )
        .await
        .unwrap();
    assert!(id["id"].as_i64().unwrap() > 0);
    assert_eq!(
        e.rpc(
            "activity.note",
            json!({ "kind": "folder_added", "client_kind": "cursor" })
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::InvalidInput,
        "the app only notes Connect / Disconnect"
    );
    let first = e.activity_list(None, Some(1)).await.unwrap();
    assert_eq!(first.entries.len(), 1);
    let next = first.next_before_id.unwrap();
    let second = e.activity_list(Some(next), Some(50)).await.unwrap();
    assert!(second.entries.iter().all(|a| a.id < next));
    // No provider used: the egress card is hidden.
    let eg = e.activity_egress(None, None, None).await.unwrap();
    assert!(eg.rows.is_empty() && eg.providers.is_empty());
    e.drain().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_tokens_create_rotate_revoke() {
    let t = tmp();
    let data = t.path().join("d");
    let secrets = Arc::new(MemorySecretStore::new());
    let mut cfg = config(&data, ExtractSetup::Disabled);
    cfg.secrets = Some(secrets.clone());
    let e = Engine::start(cfg).await.unwrap();
    assert!(!e.http_status().enabled, "off by default");
    let st = e
        .rpc("http.set_enabled", json!({ "enabled": true }))
        .await
        .unwrap();
    assert_eq!(st["enabled"], true);
    assert!(e.http_wanted().borrow().enabled);
    let saved = std::fs::read_to_string(data.join("settings.toml")).unwrap();
    assert!(saved.contains("http_server_enabled = true"), "{saved}");

    let created = e.http_create_token("My script".into()).await.unwrap();
    assert_eq!(created.token.len(), 43);
    assert_eq!(created.client.kind, ClientKind::Http);
    assert_eq!(
        created.client.token_last4.as_deref(),
        Some(&created.token[39..])
    );
    assert!(created.snippet.text.contains(&created.token));
    let id = created.client.client_id.clone();
    assert!(id.starts_with("http-"));
    assert_eq!(
        e.http_client_for_token(&created.token).as_deref(),
        Some(id.as_str())
    );
    assert_eq!(e.http_client_for_token("nope"), None);
    assert!(
        secrets
            .get(&sr_engine::httpctl::token_account(&id))
            .unwrap()
            .is_some()
    );
    // The token never shows in clients.list or events (only the hint).
    let list = serde_json::to_string(&e.clients_list().await.unwrap()).unwrap();
    assert!(!list.contains(&created.token));

    let gen0 = e.http_wanted().borrow().generation;
    let rotated = e.http_rotate_token(id.clone()).await.unwrap();
    assert_ne!(rotated.token, created.token);
    assert_eq!(
        e.http_client_for_token(&created.token),
        None,
        "old token stops"
    );
    assert_eq!(
        e.http_client_for_token(&rotated.token).as_deref(),
        Some(id.as_str())
    );
    assert!(e.http_wanted().borrow().generation > gen0, "sessions end");

    assert!(e.http_revoke(id.clone()).await.unwrap());
    assert_eq!(e.http_client_for_token(&rotated.token), None);
    assert!(
        secrets
            .get(&sr_engine::httpctl::token_account(&id))
            .unwrap()
            .is_none()
    );
    assert!(
        e.clients_list()
            .await
            .unwrap()
            .clients
            .iter()
            .all(|c| c.client_id != id)
    );
    // Pipe clients can never claim an HTTP id.
    assert_eq!(e.client_session_opened(&id).await, "other");
    e.drain().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_cancel_by_id_and_by_session() {
    let t = tmp();
    let data = t.path().join("d");
    let e = engine(&data).await;
    let g = e.begin_request(Some("r-1"), Some(7)).unwrap();
    assert_eq!(
        e.begin_request(Some("r-1"), None).err().unwrap().code,
        ErrorCode::InvalidInput,
        "ids are unique while running"
    );
    assert_eq!(
        e.begin_request(Some("bad id"), None).err().unwrap().code,
        ErrorCode::InvalidInput
    );
    let r = e
        .rpc("requests.cancel", json!({ "request_id": "r-1" }))
        .await
        .unwrap();
    assert_eq!(r["cancelled"], true);
    assert!(g.token().is_cancelled());
    drop(g);
    assert!(!e.cancel_request("r-1"), "gone once finished");
    let g2 = e.begin_request(None, Some(9)).unwrap();
    assert_eq!(e.cancel_session_requests(9), 1);
    assert!(g2.token().is_cancelled());
    e.drain().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn index_move_check_plan_copy_and_reopen() {
    let t = tmp();
    let data = t.path().join("d");
    let docs = t.path().join("docs");
    let target = t.path().join("other");
    std::fs::create_dir_all(&target).unwrap();
    write_files(&docs, &[("a.md", "x")]);
    let e = engine(&data).await;
    let f = e.add_folder(&docs.to_string_lossy(), None).await.unwrap();
    let st = e.index_location_status().await.unwrap();
    assert!(st.is_default);
    assert!(st.size_bytes > 0, "catalog.db exists");
    assert!(st.moving.is_none());
    let bad = e
        .index_location_check(docs.to_string_lossy().into_owned())
        .await
        .unwrap();
    assert_eq!(bad.problem, Some(MoveProblem::InsideFolder));
    let ok = e
        .index_location_check(target.to_string_lossy().into_owned())
        .await
        .unwrap();
    assert!(ok.ok && ok.enough_space, "{ok:?}");
    let to = e
        .index_location_move(target.to_string_lossy().into_owned())
        .await
        .unwrap();
    assert_eq!(to, target.join(sr_common::api::INDEX_DIR_NAME));
    assert!(sr_engine::indexmove::read_plan(&data).is_some());
    assert_eq!(
        e.index_location_move(target.to_string_lossy().into_owned())
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict,
        "one saved move at a time"
    );
    // The daemon is asked to drain and exit.
    tokio::time::timeout(Duration::from_secs(5), e.restart_requested())
        .await
        .unwrap();
    assert!(e.index_location_status().await.unwrap().moving.is_some());
    e.drain().await;
    drop(e);
    // Let the dropped engine's file handles close (catalog writer thread, tantivy) first.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The next start runs the move first (sr-daemon does this before Engine::start).
    let ctl = sr_engine::indexmove::MoveControl::default();
    let out = sr_engine::indexmove::run_pending(&data, &ctl, &|_| {});
    assert!(
        matches!(out, sr_engine::indexmove::MoveOutcome::Switched { .. }),
        "{out:?}"
    );
    let e = engine(&data).await;
    assert_eq!(e.data_dir(), to.as_path());
    let folders = e.list_folders().await.unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].folder_id, f.folder_id);
    assert!(
        data.join(sr_common::config::SETTINGS_FILE).is_file(),
        "settings stay"
    );
    let done = sr_engine::indexmove::cleanup_after_start(&data, e.data_dir()).unwrap();
    assert_eq!(done.to, to);
    assert!(!data.join("catalog.db").exists(), "old copy deleted");
    let st = e.index_location_status().await.unwrap();
    assert!(!st.is_default);
    e.drain().await;
}

#[cfg(windows)]
#[test]
fn trusted_image_rule() {
    use sr_engine::pipe::is_trusted_image;
    use std::path::Path;
    let daemon = Path::new(r"C:\Program Files\SR\sr-daemon.exe");
    assert!(is_trusted_image(
        Path::new(r"C:\Program Files\SR\sr-app.exe"),
        daemon
    ));
    assert!(is_trusted_image(
        Path::new(r"c:\program files\sr\SR-APP.EXE"),
        daemon
    ));
    assert!(is_trusted_image(daemon, daemon), "the admin CLI");
    assert!(!is_trusted_image(Path::new(r"C:\Temp\sr-app.exe"), daemon));
    assert!(!is_trusted_image(
        Path::new(r"C:\Program Files\SR\node.exe"),
        daemon
    ));
    assert!(!is_trusted_image(
        Path::new(r"C:\Program Files\SR\sub\sr-app.exe"),
        daemon
    ));
}

/// macOS port: same rule without `.exe`; names and folders compare exactly.
#[cfg(unix)]
#[test]
fn trusted_image_rule_unix() {
    use sr_engine::pipe::{host_call_allowed, is_trusted_image};
    use std::path::Path;
    let daemon = Path::new("/Applications/SR/sr-daemon");
    assert!(is_trusted_image(
        Path::new("/Applications/SR/sr-app"),
        daemon
    ));
    assert!(is_trusted_image(daemon, daemon), "the admin CLI");
    assert!(!is_trusted_image(
        Path::new("/Applications/SR/SR-APP"),
        daemon
    ));
    assert!(!is_trusted_image(
        Path::new("/Applications/sr/sr-app"),
        daemon
    ));
    assert!(!is_trusted_image(
        Path::new("/Applications/SR/sr-app.exe"),
        daemon
    ));
    assert!(!is_trusted_image(Path::new("/tmp/sr-app"), daemon));
    assert!(!is_trusted_image(
        Path::new("/Applications/SR/sub/sr-app"),
        daemon
    ));
    let setup = Path::new("/private/tmp/x/sr-setup");
    assert!(host_call_allowed("host.shutdown", false, Some(setup)));
    assert!(!host_call_allowed("host.restart", false, Some(setup)));
}

/// S-05 (R31): stopping the service and drain-restarts are for our own programs only.
#[cfg(windows)]
#[test]
fn host_calls_and_drain_restart_need_our_programs() {
    use sr_engine::pipe::{host_call_allowed, is_install_image};
    use std::path::Path;
    let setup = Path::new(r"C:\Users\me\AppData\Local\Temp\nsx1.tmp\sr-setup.exe");
    let other = Path::new(r"C:\Users\me\Downloads\tool.exe");
    // Our app: both.
    assert!(host_call_allowed("host.shutdown", true, None));
    assert!(host_call_allowed("host.restart", true, Some(other)));
    // The installer may ask for a drained shutdown, nothing else.
    assert!(host_call_allowed("host.shutdown", false, Some(setup)));
    assert!(!host_call_allowed("host.restart", false, Some(setup)));
    // Anyone else: neither.
    assert!(!host_call_allowed("host.shutdown", false, Some(other)));
    assert!(!host_call_allowed("host.restart", false, Some(other)));
    assert!(!host_call_allowed("host.shutdown", false, None));

    let daemon = Path::new(r"C:\Program Files\SR\sr-daemon.exe");
    let names = ["sr-app.exe", "sr-bridge.exe", "sr-daemon.exe"];
    assert!(is_install_image(
        Path::new(r"C:\Program Files\SR\SR-BRIDGE.exe"),
        daemon,
        &names
    ));
    assert!(!is_install_image(
        Path::new(r"C:\Temp\sr-bridge.exe"),
        daemon,
        &names
    ));
    assert!(!is_install_image(
        Path::new(r"C:\Program Files\SR\sr-launch.exe"),
        daemon,
        &names
    ));
}
