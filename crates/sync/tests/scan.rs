//! Reconcile walker + change detection against a real catalog on temp dirs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use sr_catalog::repo::{chunks, files, folders};
use sr_catalog::{FileState, JobKind, RunTrigger};
use sr_common::error::ErrorCode;
use sr_common::events::FolderStatus;
use sr_common::sandbox::{DangerousRoots, Violation};
use sr_sync::filter::Excluded;
use sr_sync::{AddFolder, EventOutcome, PathEvent, ScanOutcome, ScanStats};
use tokio_util::sync::CancellationToken;

fn scan(svc: &sr_sync::Reconciler, folder: sr_common::ids::FolderId) -> ScanStats {
    match svc
        .scan_blocking(folder, RunTrigger::Manual, &CancellationToken::new())
        .unwrap()
    {
        ScanOutcome::Completed(s) => s,
        other => panic!("scan did not complete: {other:?}"),
    }
}

fn scan_with(
    svc: &sr_sync::Reconciler,
    folder: sr_common::ids::FolderId,
    t: RunTrigger,
) -> ScanStats {
    match svc
        .scan_blocking(folder, t, &CancellationToken::new())
        .unwrap()
    {
        ScanOutcome::Completed(s) => s,
        other => panic!("scan did not complete: {other:?}"),
    }
}

fn index_jobs(fx: &Fx, folder: sr_common::ids::FolderId) -> usize {
    fx.live_jobs(Some(folder))
        .iter()
        .filter(|j| j.kind == JobKind::IndexFile)
        .count()
}

#[test]
fn ignore_rules_gitignore_selene_defaults_denylist_and_presets() {
    let fx = setup();
    let root = fx.dir("proj");
    write(&root, ".gitignore", "secret_notes/\n*.draft.md\n");
    write(&root, "a.md", "a");
    write(&root, "b.draft.md", "b");
    write(&root, "secret_notes/x.md", "x");
    write(&root, "node_modules/p/index.js", "p");
    write(&root, ".env", "KEY=1");
    write(&root, "keys/server.pem", "pem");
    write(&root, ".ssh/notes.md", "ssh");
    write(&root, "src/main.rs", "fn main(){}");
    write(&root, "image.png", "png");
    write(&root, "build.log", "log");
    write(&root, "sub/.gitignore", "local.txt\n");
    write(&root, "sub/local.txt", "l");
    write(&root, "sub/keep.txt", "k");
    write(&root, "app.exe", "MZ");
    let exec = Arc::new(FakeExec::new(&fx.cat));
    let svc = fx.service(exec, fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    let id = add(rec, &fx, &root);
    let st = scan(rec, id);
    assert_eq!(
        fx.file_paths(id),
        vec!["a.md", "src/main.rs", "sub/keep.txt"],
        "{st:?}"
    );
    // .env + server.pem (denylist), image.png, build.log (defaults), 2× .gitignore + app.exe
    // (not included); pruned directories and gitignored entries are not counted.
    assert_eq!(st.excluded, 7, "{st:?}");
    assert_eq!(index_jobs(&fx, id), 3);

    // Now exclude a directory by user glob and switch the preset: the next scan sweeps.
    let mut s = fx.folder(id).settings;
    s.exclude_globs = vec!["src".into()];
    s.file_types_preset = sr_common::config::FileTypesPreset::Documents;
    rec.update_settings(id, &s).unwrap();
    let st = scan(rec, id);
    assert_eq!(fx.file_paths(id), vec!["a.md", "sub/keep.txt"], "{st:?}");
    assert_eq!(st.deleted, 1);
}

#[test]
fn non_recursive_folder_lists_only_the_root() {
    let fx = setup();
    let root = fx.dir("flat");
    write(&root, "top.md", "t");
    write(&root, "deep/inner.md", "i");
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    let id = add(rec, &fx, &root);
    scan(rec, id);
    assert_eq!(fx.file_paths(id), vec!["deep/inner.md", "top.md"]);
    let mut s = fx.folder(id).settings;
    s.recursive = false;
    rec.update_settings(id, &s).unwrap();
    let st = scan(rec, id);
    assert_eq!(fx.file_paths(id), vec!["top.md"]);
    assert_eq!(st.deleted, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racy_mtime_is_rehashed_and_old_unchanged_metadata_is_trusted() {
    let fx = setup();
    let root = fx.dir("racy");
    let fresh = write(&root, "fresh.md", "one");
    let old = write(&root, "old.md", "aaa");
    set_mtime(&old, hour_ago());
    let exec = Arc::new(FakeExec::new(&fx.cat));
    let svc = fx.service(exec, fx.epoch(), fast_cfg());
    let id = svc
        .add_folder(sr_sync::AddFolder::new(fx.collection, &root))
        .await
        .unwrap();
    run_until_idle(&fx, &svc, Duration::from_secs(20)).await;
    assert_eq!(fx.folder(id).status, FolderStatus::Synced);

    // Same size, mtime restored: only content differs.
    let fresh_mtime = std::fs::metadata(&fresh).unwrap().modified().unwrap();
    std::fs::write(&fresh, "two").unwrap();
    set_mtime(&fresh, fresh_mtime);
    std::fs::write(&old, "bbb").unwrap();
    set_mtime(&old, hour_ago() - Duration::from_secs(0));
    let old_key = sr_common::sandbox::path_key("old.md");
    // Restore old.md's catalog mtime exactly (set_mtime above used a new "hour ago").
    let cat_old = fx
        .cat
        .read(|c| files::file_by_path(c, id, &old_key))
        .unwrap()
        .unwrap();
    let t = std::time::UNIX_EPOCH + Duration::from_nanos(u64::try_from(cat_old.mtime_ns).unwrap());
    set_mtime(&old, t);

    let rec = Arc::clone(svc.reconciler());
    let st = tokio::task::spawn_blocking(move || scan(&rec, id))
        .await
        .unwrap();
    let live: Vec<String> = fx
        .live_jobs(Some(id))
        .iter()
        .filter_map(|j| j.file_id)
        .map(|f| fx.cat.read(|c| files::file(c, f)).unwrap().rel_path)
        .collect();
    assert_eq!(live, vec!["fresh.md"], "racy file re-hashed: {st:?}");
    assert!(st.hashed >= 1);

    // A smart scheduled run deep-verifies everything once a day and catches the old file too.
    let rec = Arc::clone(svc.reconciler());
    let st = tokio::task::spawn_blocking(move || scan_with(&rec, id, RunTrigger::Schedule))
        .await
        .unwrap();
    assert_eq!(st.hashed, 2, "{st:?}");
    assert_eq!(fx.live_jobs(Some(id)).len(), 2);
    assert!(fx.folder(id).stats.get("smart_verify_at").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_keeps_file_id_and_chunk_ids_without_reindexing() {
    let fx = setup();
    let root = fx.dir("ren");
    let a = write(&root, "docs/a.md", "hello world");
    set_mtime(&a, hour_ago());
    let exec = Arc::new(FakeExec::new(&fx.cat));
    let svc = fx.service(exec.clone(), fx.epoch(), fast_cfg());
    let id = svc
        .add_folder(sr_sync::AddFolder::new(fx.collection, &root))
        .await
        .unwrap();
    run_until_idle(&fx, &svc, Duration::from_secs(20)).await;
    let key = sr_common::sandbox::path_key("docs/a.md");
    let before = fx
        .cat
        .read(|c| files::file_by_path(c, id, &key))
        .unwrap()
        .unwrap();
    assert_eq!(before.state, FileState::Indexed);
    let chunks_before = fx.cat.read(|c| chunks::file_chunks(c, before.id)).unwrap();

    std::fs::rename(&a, root.join("docs/renamed.md")).unwrap();
    let rec = Arc::clone(svc.reconciler());
    let st = tokio::task::spawn_blocking(move || scan(&rec, id))
        .await
        .unwrap();
    assert_eq!(st.renamed, 1, "{st:?}");
    assert_eq!(st.deleted, 0);
    let key = sr_common::sandbox::path_key("docs/renamed.md");
    let after = fx
        .cat
        .read(|c| files::file_by_path(c, id, &key))
        .unwrap()
        .unwrap();
    assert_eq!(after.id, before.id, "file_id kept");
    assert_eq!(after.state, FileState::Indexed);
    // CO-1: a pure rename queues exactly one path-only refresh (lexical path tokens; the
    // engine re-applies the file at its current version without re-embedding).
    assert_eq!(st.path_refreshes, 1, "{st:?}");
    let live: Vec<_> = fx
        .live_jobs(Some(id))
        .into_iter()
        .filter(|j| j.kind == JobKind::IndexFile)
        .collect();
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(live[0].file_id, Some(after.id));
    assert_eq!(
        live[0].payload,
        Some(serde_json::json!({ "path_refresh": true }))
    );
    // Editing the renamed file before the refresh ran must not be swallowed by it.
    std::fs::write(root.join("docs/renamed.md"), "hello world, edited").unwrap();
    let rec = Arc::clone(svc.reconciler());
    let st = tokio::task::spawn_blocking(move || scan(&rec, id))
        .await
        .unwrap();
    assert_eq!(st.coalesced, 1, "{st:?}");
    let live: Vec<_> = fx
        .live_jobs(Some(id))
        .into_iter()
        .filter(|j| j.kind == JobKind::IndexFile)
        .collect();
    assert_eq!(live.len(), 1);
    assert_eq!(
        live[0].payload,
        Some(serde_json::json!({ "renamed": true })),
        "full index that also refreshes the path"
    );
    assert_eq!(index_jobs(&fx, id), 1);
    let chunks_after = fx.cat.read(|c| chunks::file_chunks(c, after.id)).unwrap();
    assert_eq!(
        chunks_before.iter().map(|c| c.chunk_id).collect::<Vec<_>>(),
        chunks_after.iter().map(|c| c.chunk_id).collect::<Vec<_>>()
    );
    assert_eq!(fx.file_paths(id), vec!["docs/renamed.md"]);
}

/// CO-1: a rename of a *racy* file (modified since the last trusted scan, so its metadata can't be
/// trusted) is hashed under its new path against the renamed row: unchanged content stays a cheap
/// path refresh, changed content becomes a full job flagged `renamed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racy_rename_is_hashed_and_stays_a_path_refresh_when_unchanged() {
    let fx = setup();
    let root = fx.dir("racyren");
    // Fresh mtimes: both files are racy on every scan in this test.
    write(&root, "same.md", "unchanged body");
    write(&root, "edit.md", "old body");
    let exec = Arc::new(FakeExec::new(&fx.cat));
    let svc = fx.service(exec.clone(), fx.epoch(), fast_cfg());
    let id = svc
        .add_folder(sr_sync::AddFolder::new(fx.collection, &root))
        .await
        .unwrap();
    run_until_idle(&fx, &svc, Duration::from_secs(20)).await;
    assert_eq!(fx.live_jobs(Some(id)).len(), 0);

    std::fs::rename(root.join("same.md"), root.join("same-renamed.md")).unwrap();
    std::fs::rename(root.join("edit.md"), root.join("edit-renamed.md")).unwrap();
    std::fs::write(root.join("edit-renamed.md"), "new body, longer").unwrap();
    let rec = Arc::clone(svc.reconciler());
    let st = tokio::task::spawn_blocking(move || scan(&rec, id))
        .await
        .unwrap();
    assert_eq!(st.renamed, 2, "{st:?}");
    assert_eq!(st.path_refreshes, 1, "{st:?}");
    assert!(st.hashed >= 2, "rename candidates hashed: {st:?}");
    let mut payloads: Vec<(String, Option<serde_json::Value>)> = fx
        .live_jobs(Some(id))
        .into_iter()
        .filter(|j| j.kind == JobKind::IndexFile)
        .map(|j| {
            let f = j.file_id.unwrap();
            (
                fx.cat.read(|c| files::file(c, f)).unwrap().rel_path,
                j.payload,
            )
        })
        .collect();
    payloads.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        payloads,
        vec![
            (
                "edit-renamed.md".into(),
                Some(serde_json::json!({ "renamed": true }))
            ),
            (
                "same-renamed.md".into(),
                Some(serde_json::json!({ "path_refresh": true }))
            ),
        ]
    );
}

/// `icacls` deny of "list folder" for the current user (no admin needed on own files).
fn deny_list(dir: &Path, on: bool) -> bool {
    let user = std::env::var("USERNAME").unwrap_or_default();
    let mut c = Command::new("icacls");
    c.arg(dir);
    if on {
        c.args(["/deny", &format!("{user}:(RD)")]);
    } else {
        c.args(["/remove:d", &user]);
    }
    c.output().map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(windows)]
#[test]
fn mark_and_sweep_never_deletes_from_an_incomplete_subtree() {
    let fx = setup();
    let root = fx.dir("sweep");
    write(&root, "ok/a.md", "a");
    write(&root, "ok/b.md", "b");
    write(&root, "locked/x.md", "x");
    write(&root, "locked/y.md", "y");
    write(&root, "locked/deeper/z.md", "z");
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    let id = add(rec, &fx, &root);
    scan(rec, id);
    assert_eq!(fx.file_paths(id).len(), 5);

    std::fs::remove_file(root.join("ok/a.md")).unwrap();
    std::fs::remove_file(root.join("locked/x.md")).unwrap();
    let locked = root.join("locked");
    if !deny_list(&locked, true) {
        eprintln!("icacls unavailable; skipping");
        return;
    }
    let st = scan(rec, id);
    deny_list(&locked, false);
    assert!(st.partial, "{st:?}");
    assert_eq!(st.deleted, 1, "only ok/a.md: {st:?}");
    assert_eq!(
        fx.file_paths(id),
        vec![
            "locked/deeper/z.md",
            "locked/x.md",
            "locked/y.md",
            "ok/b.md"
        ],
        "files under the unreadable directory are kept"
    );
    rec.refresh_status(id).unwrap();
    assert!(fx.folder(id).last_scan_partial);

    // Once readable again the missing file is swept.
    let st = scan(rec, id);
    assert!(!st.partial);
    assert_eq!(
        fx.file_paths(id),
        vec!["locked/deeper/z.md", "locked/y.md", "ok/b.md"]
    );
}

#[test]
fn cancelled_scan_never_sweeps() {
    let fx = setup();
    let root = fx.dir("cancel");
    write(&root, "a.md", "a");
    write(&root, "b.md", "b");
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    let id = add(rec, &fx, &root);
    scan(rec, id);
    std::fs::remove_file(root.join("a.md")).unwrap();
    let c = CancellationToken::new();
    c.cancel();
    let r = rec.scan_blocking(id, RunTrigger::Manual, &c);
    assert_eq!(r.unwrap_err().code, ErrorCode::Cancelled);
    assert_eq!(fx.file_paths(id), vec!["a.md", "b.md"]);
}

fn mklink_junction(link: &Path, target: &Path) -> bool {
    Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn short_path(p: &Path) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = Command::new("cmd")
        .raw_arg(format!("/C for %I in (\"{}\") do @echo %~sI", p.display()))
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(windows)]
#[test]
fn traversal_symlink_junction_short_name_ads_and_device_names_are_rejected() {
    let fx = setup();
    let root = fx.dir("guard");
    let outside = fx.dir("outside");
    write(&outside, "stolen.md", "outside the root");
    write(&root, "ok.md", "fine");
    // ADS: a stream on ok.md is never a separate file.
    std::fs::write(root.join("ok.md:hidden"), "stream").unwrap();
    // Names Win32 can't normally create, made through the verbatim prefix.
    let verbatim = format!(r"\\?\{}", root.display());
    for bad in ["con.txt", "trail.", "space .md "] {
        let _ = std::fs::write(format!(r"{verbatim}\{bad}"), "x");
    }
    let junction_ok = mklink_junction(&root.join("link"), &outside);
    let symlink_ok =
        std::os::windows::fs::symlink_file(outside.join("stolen.md"), root.join("sym.md")).is_ok();
    write(&root, "credentials-prod.json", "{}");
    assert!(junction_ok, "mklink /J needs no privilege");
    let short = short_path(&root.join("credentials-prod.json"));
    eprintln!("symlink_ok={symlink_ok} short={short:?}");

    for follow in [false, true] {
        let fx2_root = root.clone();
        let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
        let rec = svc.reconciler();
        let mut req = AddFolder::new(fx.collection, &fx2_root);
        req.settings.follow_symlinks = follow;
        req.settings.file_types_preset = sr_common::config::FileTypesPreset::Custom;
        let id = rec.add_folder(&req).unwrap();
        let st = scan(rec, id);
        assert_eq!(
            fx.file_paths(id),
            vec!["ok.md"],
            "follow={follow} junction={junction_ok} symlink={symlink_ok} {st:?}"
        );

        // Watcher-style paths go through the same sandbox.
        let ev = |p: &str| {
            rec.on_path_event(id, &PathEvent::Changed(p.into()))
                .unwrap()
        };
        assert_eq!(
            ev(r"..\outside\stolen.md"),
            EventOutcome::Excluded(Excluded::Rejected(Violation::ParentTraversal))
        );
        assert_eq!(
            ev("ok.md:hidden"),
            EventOutcome::Excluded(Excluded::Rejected(Violation::AlternateDataStream))
        );
        assert_eq!(
            ev("CON"),
            EventOutcome::Excluded(Excluded::Rejected(Violation::ReservedName))
        );
        assert_eq!(
            ev("trail."),
            EventOutcome::Excluded(Excluded::Rejected(Violation::TrailingDotOrSpace))
        );
        if junction_ok {
            assert_eq!(
                ev("link/stolen.md"),
                EventOutcome::Excluded(Excluded::Rejected(Violation::OutsideRoot))
            );
        }
        if symlink_ok {
            assert_eq!(
                ev("sym.md"),
                EventOutcome::Excluded(Excluded::Rejected(Violation::OutsideRoot))
            );
        }
        // 8.3 short name of a denylisted file: expanded to the long name, then denied.
        if let Some(short) = short.clone() {
            let short_name = Path::new(&short)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !short_name.eq_ignore_ascii_case("credentials-prod.json") {
                assert_eq!(ev(&short_name), EventOutcome::Excluded(Excluded::Secret));
            }
        }
        assert_eq!(fx.file_paths(id), vec!["ok.md"]);
        fx.cat
            .write_blocking(move |c| folders::remove_folder(c, id, sr_catalog::now_ms()))
            .unwrap();
    }
}

#[cfg(windows)]
#[test]
fn overlapping_folders_are_refused_even_through_aliases() {
    let fx = setup();
    let a = fx.dir("projects/a");
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    add(rec, &fx, &a);
    for other in [fx.dir("projects/a/sub"), fx.data.path().join("projects")] {
        let e = rec
            .add_folder(&AddFolder::new(fx.collection, &other))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::FolderOverlap, "{other:?}");
    }
    // A junction alias of `a` canonicalises to the same root.
    let alias = fx.data.path().join("alias");
    if mklink_junction(&alias, &a) {
        let e = rec
            .add_folder(&AddFolder::new(fx.collection, &alias))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::FolderOverlap);
    }
    // A different collection may overlap.
    let other_col = fx
        .cat
        .write_blocking(|c| {
            let p = sr_catalog::repo::profiles::collections(c)?[0].embedding_profile_id;
            sr_catalog::repo::profiles::create_collection(c, "Work", p, false, sr_catalog::now_ms())
        })
        .unwrap();
    assert!(rec.add_folder(&AddFolder::new(other_col, &a)).is_ok());
}

#[cfg(windows)]
#[test]
fn retargeted_junction_marks_folder_unavailable_and_deletes_nothing() {
    let fx = setup();
    let forbidden = fx.dir("forbidden");
    write(&forbidden, "system.md", "private");
    let elsewhere = fx.dir("elsewhere");
    write(&elsewhere, "other.md", "other");
    let proj = fx.dir("proj");
    write(&proj, "a.md", "a");
    write(&proj, "b.md", "b");
    let mut cfg = fast_cfg();
    cfg.dangerous = DangerousRoots::none().with_never_inside(&forbidden);
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), cfg);
    let rec = svc.reconciler();
    let id = add(rec, &fx, &proj);
    scan(rec, id);
    assert_eq!(fx.file_paths(id).len(), 2);

    // The folder is replaced by a junction to a protected location.
    std::fs::rename(&proj, fx.data.path().join("proj_moved")).unwrap();
    if !mklink_junction(&proj, &forbidden) {
        eprintln!("mklink unavailable; skipping");
        return;
    }
    let out = rec
        .scan_blocking(id, RunTrigger::Startup, &CancellationToken::new())
        .unwrap();
    assert_eq!(out, ScanOutcome::Unavailable(ErrorCode::PathRefused));
    let f = fx.folder(id);
    assert!(!f.available);
    assert_eq!(f.status, FolderStatus::Unavailable);
    assert_eq!(fx.file_paths(id), vec!["a.md", "b.md"], "nothing deleted");

    // Retargeted to an ordinary folder: the stored root no longer matches → unavailable too.
    std::fs::remove_dir(&proj).unwrap();
    assert!(mklink_junction(&proj, &elsewhere));
    let out = rec
        .scan_blocking(id, RunTrigger::Startup, &CancellationToken::new())
        .unwrap();
    assert_eq!(out, ScanOutcome::Unavailable(ErrorCode::FolderUnavailable));
    assert_eq!(fx.file_paths(id), vec!["a.md", "b.md"]);

    // The real folder comes back: available again, normal scan.
    std::fs::remove_dir(&proj).unwrap();
    std::fs::rename(fx.data.path().join("proj_moved"), &proj).unwrap();
    let st = scan(rec, id);
    assert_eq!(st.deleted, 0);
    assert!(fx.folder(id).available);
}

#[test]
fn policy_skips_too_large() {
    let fx = setup();
    let root = fx.dir("skips");
    write(&root, "small.md", "s");
    write(&root, "big.md", &"x".repeat(4096));
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    let mut req = AddFolder::new(fx.collection, &root);
    req.settings.max_file_bytes = 1024;
    let id = rec.add_folder(&req).unwrap();
    scan(rec, id);
    let skipped = fx
        .cat
        .read(|c| files::files_in_state(c, id, FileState::Skipped))
        .unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].skip_reason.as_deref(), Some("too_large"));
    assert_eq!(skipped[0].error_code.as_deref(), Some("FileTooLarge"));
    assert_eq!(index_jobs(&fx, id), 1);
    // Raising the limit re-evaluates the skip.
    let mut s = fx.folder(id).settings;
    s.max_file_bytes = 1 << 20;
    rec.update_settings(id, &s).unwrap();
    scan(rec, id);
    assert_eq!(index_jobs(&fx, id), 2);
}

// ------------------------------------------------------------------ docs/06 change 5, issue 4

/// `.seleneignore` excludes (highest precedence), `!` re-includes a built-in default, single
/// files below an excluded tree can be re-included (`/**` + `!**/`), and the sandbox and the
/// secret denylist can never be re-included.
#[test]
fn seleneignore_excludes_reincludes_defaults_but_never_secrets() {
    let fx = setup();
    let root = fx.dir("rules");
    write(
        &root,
        ".seleneignore",
        "drafts/\n!vendor/\n!*.min.js\n!.env\n!*.pem\n!.ssh/\ncrate/**\n!crate/**/\n!crate/src/merge.rs\n",
    );
    write(&root, ".gitignore", "kept/\n");
    write(&root, ".seleneignore.d/x.md", "not an ignore file");
    write(&root, "a.md", "a");
    write(&root, "drafts/x.md", "x");
    write(&root, "vendor/lib.md", "v");
    write(&root, "third_party/t.md", "t");
    write(&root, "app.min.js", "m");
    write(&root, "other.min.css", "c");
    write(&root, "data.jsonl", "{}\n");
    write(&root, ".env", "KEY=1");
    write(&root, "k.pem", "pem");
    write(&root, ".ssh/config.md", "ssh");
    write(&root, "crate/src/merge.rs", "patched");
    write(&root, "crate/src/query.rs", "upstream");
    write(&root, "crate/README.md", "upstream");
    write(&root, "kept/a.md", "git-ignored");
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    let mut req = AddFolder::new(fx.collection, &root);
    req.settings.file_types_preset = sr_common::config::FileTypesPreset::Custom;
    let id = rec.add_folder(&req).unwrap();
    let st = scan(rec, id);
    assert_eq!(
        fx.file_paths(id),
        vec![
            ".gitignore",
            ".seleneignore",
            ".seleneignore.d/x.md",
            "a.md",
            "app.min.js",
            "crate/src/merge.rs",
            "vendor/lib.md",
        ],
        "{st:?}"
    );
    // Watcher events see the same rules (docs/06 §2.4: a save in an ignored directory).
    let ev = |p: &str| {
        rec.on_path_event(id, &PathEvent::Changed(p.into()))
            .unwrap()
    };
    write(&root, "kept/b.md", "b");
    write(&root, "drafts/y.md", "y");
    write(&root, "crate/src/rrf.rs", "upstream");
    assert_eq!(ev("kept/b.md"), EventOutcome::Excluded(Excluded::Ignored));
    assert_eq!(ev("drafts/y.md"), EventOutcome::Excluded(Excluded::Ignored));
    assert_eq!(
        ev("crate/src/rrf.rs"),
        EventOutcome::Excluded(Excluded::Ignored)
    );
    assert_eq!(ev(".env"), EventOutcome::Excluded(Excluded::Secret));
    assert_eq!(
        ev("third_party/t.md"),
        EventOutcome::Excluded(Excluded::Default)
    );
    assert!(
        fx.file_paths(id)
            .iter()
            .all(|p| p != "kept/b.md" && p != "drafts/y.md")
    );
}

/// Issue 4: a rule added to `.seleneignore` removes already-indexed files through the normal
/// sweep (catalog rows and chunks gone in one transaction, outbox `Delete` ops for the stores);
/// removing the rule brings them back. The ignore-file edit itself triggers the rescan.
#[test]
fn ignore_rule_changes_sweep_and_restore_indexed_files() {
    let fx = setup();
    let root = fx.dir("sweep");
    write(&root, "docs/a.md", "a");
    write(&root, "docs/b.md", "b");
    write(&root, "notes.md", "n");
    write(&root, "bundle.min.js", "generated");
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    let id = add(rec, &fx, &root);
    scan(rec, id);
    index_all_now(&fx, id);
    assert_eq!(
        fx.file_paths(id),
        vec!["docs/a.md", "docs/b.md", "notes.md"]
    );
    let docs: Vec<sr_common::ids::FileId> = fx
        .cat
        .read(move |c| files::files_in_state(c, id, FileState::Indexed))
        .unwrap()
        .into_iter()
        .filter(|f| f.rel_path.starts_with("docs/"))
        .map(|f| f.id)
        .collect();
    assert_eq!(docs.len(), 2);
    let coll = fx.collection;
    let pending = || {
        fx.cat
            .read(move |c| sr_catalog::repo::outbox::pending(c, coll))
            .unwrap()
    };
    let before = pending();

    // Add the rule: the watcher event on `.seleneignore` asks for a settings_change scan.
    write(&root, ".seleneignore", "docs/\n");
    let o = rec
        .on_path_event(id, &PathEvent::Changed(".seleneignore".into()))
        .unwrap();
    assert!(
        matches!(
            o,
            EventOutcome::Enqueued {
                kind: JobKind::ScanFolder,
                ..
            }
        ),
        "{o:?}"
    );
    let job = fx
        .live_jobs(Some(id))
        .into_iter()
        .find(|j| j.kind == JobKind::ScanFolder)
        .unwrap();
    assert_eq!(job.payload.unwrap()["trigger"], "settings_change");
    let st = scan_with(rec, id, RunTrigger::SettingsChange);
    assert_eq!(st.deleted, 2, "{st:?}");
    assert_eq!(fx.file_paths(id), vec!["notes.md"]);
    for f in &docs {
        let f = *f;
        assert!(
            fx.cat
                .read(move |c| chunks::file_chunks(c, f))
                .unwrap()
                .is_empty(),
            "chunks hidden at once"
        );
    }
    assert!(pending() >= before + 2, "Delete ops for Lance and tantivy");

    // Remove the rule: the files come back (new rows, index jobs).
    std::fs::remove_file(root.join(".seleneignore")).unwrap();
    rec.on_path_event(id, &PathEvent::Removed(".seleneignore".into()))
        .unwrap();
    let st = scan_with(rec, id, RunTrigger::SettingsChange);
    assert_eq!(st.new, 2, "{st:?}");
    assert_eq!(
        fx.file_paths(id),
        vec!["docs/a.md", "docs/b.md", "notes.md"]
    );
    assert!(index_jobs(&fx, id) >= 2);

    // A built-in default lifted by `!`, then the lift removed: same two directions.
    write(&root, ".seleneignore", "!*.min.js\n");
    scan_with(rec, id, RunTrigger::SettingsChange);
    assert!(fx.file_paths(id).contains(&"bundle.min.js".to_string()));
    std::fs::remove_file(root.join(".seleneignore")).unwrap();
    let st = scan_with(rec, id, RunTrigger::SettingsChange);
    assert_eq!(st.deleted, 1);
    assert!(!fx.file_paths(id).contains(&"bundle.min.js".to_string()));
}

/// New built-in defaults apply to existing folders once: a catalog without the defaults stamp
/// (an upgrade) gets one `settings_change` scan per folder at start, then the stamp.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_ignore_defaults_rescan_every_folder_once() {
    let fx = setup();
    let (r1, r2) = (fx.dir("v1"), fx.dir("v2"));
    write(&r1, "a.md", "a");
    write(&r2, "b.md", "b");
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let a = add(svc.reconciler(), &fx, &r1);
    let b = add(svc.reconciler(), &fx, &r2);
    let key = sr_sync::filter::META_IGNORE_DEFAULTS_VERSION;
    let stamp = || {
        fx.cat
            .read(move |c| sr_catalog::repo::meta::get(c, key))
            .unwrap()
    };
    let current = sr_sync::filter::IGNORE_DEFAULTS_VERSION.to_string();
    assert_eq!(
        stamp().as_deref(),
        Some(current.as_str()),
        "a new catalog starts current"
    );
    // Simulate the upgrade: folders from before the stamp existed.
    fx.cat
        .write_blocking(move |c| sr_catalog::repo::meta::delete(c, key))
        .unwrap();
    let r = svc.startup().await.unwrap();
    assert_eq!(r.scans_enqueued, 2);
    for f in [a, b] {
        let job = fx
            .live_jobs(Some(f))
            .into_iter()
            .find(|j| j.kind == JobKind::ScanFolder)
            .unwrap();
        assert_eq!(job.payload.unwrap()["trigger"], "settings_change");
    }
    assert_eq!(
        stamp().as_deref(),
        Some(
            sr_sync::filter::IGNORE_DEFAULTS_VERSION
                .to_string()
                .as_str()
        )
    );
}

/// 2026-11-09: an existing folder set to Code picks the new defaults up by itself. The first start
/// after the update (an older defaults stamp) rescans it: its GDScript and `project.godot` are
/// cataloged, its data JSON is swept, configuration JSON stays, and Godot's cache and sidecars
/// stay out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn defaults_bump_brings_gdscript_into_an_existing_code_folder() {
    let fx = setup();
    let root = fx.dir("godot");
    write(&root, "game/src/temple_maps.gd", "extends Node\n");
    write(&root, "game/project.godot", "config_version=5\n");
    write(&root, "web/package.json", "{}");
    write(&root, "art/rooms/registry.json", "{\"rooms\": []}");
    write(&root, "game/.godot/editor/cached.gd", "var x = 1\n");
    write(&root, "game/icon.png.import", "[remap]\n");
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    let id = add(rec, &fx, &root);
    // What a Code folder held before the update: its JSON files, no GDScript.
    let mut s = fx.folder(id).settings;
    s.file_types_preset = sr_common::config::FileTypesPreset::Custom;
    s.include_extensions = vec!["json".into()];
    rec.update_settings(id, &s).unwrap();
    scan(rec, id);
    assert_eq!(
        fx.file_paths(id),
        vec!["art/rooms/registry.json", "web/package.json"]
    );
    s.file_types_preset = sr_common::config::FileTypesPreset::Code;
    s.include_extensions = Vec::new();
    rec.update_settings(id, &s).unwrap();
    let key = sr_sync::filter::META_IGNORE_DEFAULTS_VERSION;
    fx.cat
        .write_blocking(move |c| sr_catalog::repo::meta::set(c, key, "2"))
        .unwrap();

    let r = svc.startup().await.unwrap();
    assert_eq!(r.scans_enqueued, 1);
    let job = fx
        .live_jobs(Some(id))
        .into_iter()
        .find(|j| j.kind == JobKind::ScanFolder)
        .unwrap();
    assert_eq!(job.payload.unwrap()["trigger"], "settings_change");
    run_until_idle(&fx, &svc, Duration::from_secs(20)).await;
    assert_eq!(
        fx.file_paths(id),
        vec![
            "game/project.godot",
            "game/src/temple_maps.gd",
            "web/package.json"
        ]
    );
}

/// A Cargo workspace keeps its build-script modules (`crates/common/build/*.rs`, `include!`d by
/// the crates' `build.rs`) and modules named like output directories (`src/env/`, `src/out/`);
/// `target/`, a `dist/` next to the manifest and a JavaScript app's `build/` stay ignored. A saved
/// file in such a directory is indexed by its watcher event too.
#[test]
fn cargo_sources_named_like_build_output_are_indexed() {
    let fx = setup();
    let root = fx.dir("ws");
    for (f, body) in [
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        ("crates/common/Cargo.toml", "[package]\nname = \"common\"\n"),
        (
            "crates/common/build.rs",
            "include!(\"build/static_crt.rs\");",
        ),
        ("crates/common/build/static_crt.rs", "fn link() {}"),
        ("crates/common/src/lib.rs", "mod env;"),
        ("crates/common/src/env/mod.rs", "pub fn vars() {}"),
        ("crates/common/src/out/mod.rs", "pub fn emit() {}"),
        ("crates/common/target/debug/build/x/out/gen.rs", "generated"),
        ("target/release/build/y/out/gen.rs", "generated"),
        ("dist/index.js", "trunk output"),
        ("web/package.json", "{}"),
        ("web/build/app.js", "bundle"),
        ("docs/build/index.md", "sphinx output"),
        ("tools/build/gen.rs", "fn gen() {}"),
    ] {
        write(&root, f, body);
    }
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    let id = add(rec, &fx, &root);
    let st = scan(rec, id);
    assert_eq!(
        fx.file_paths(id),
        vec![
            "Cargo.toml",
            "crates/common/Cargo.toml",
            "crates/common/build.rs",
            "crates/common/build/static_crt.rs",
            "crates/common/src/env/mod.rs",
            "crates/common/src/lib.rs",
            "crates/common/src/out/mod.rs",
            "web/package.json",
        ],
        "{st:?}"
    );
    write(&root, "crates/common/build/delayload.rs", "fn more() {}");
    let o = rec
        .on_path_event(
            id,
            &PathEvent::Changed("crates/common/build/delayload.rs".into()),
        )
        .unwrap();
    assert!(
        matches!(
            o,
            EventOutcome::Enqueued {
                kind: JobKind::IndexFile,
                ..
            }
        ),
        "{o:?}"
    );
    assert_eq!(
        rec.on_path_event(id, &PathEvent::Changed("web/build/app.js".into()))
            .unwrap(),
        EventOutcome::Excluded(Excluded::Default)
    );

    // A `Cargo.toml` added next to an existing `build/` makes it a Cargo source: the manifest's
    // event asks for one scan, which indexes the directory; removing the manifest sweeps it.
    let scan_job = |o: &EventOutcome| {
        matches!(
            o,
            EventOutcome::Enqueued {
                kind: JobKind::ScanFolder,
                ..
            }
        )
    };
    write(&root, "tools/Cargo.toml", "[package]\nname = \"tools\"\n");
    let o = rec
        .on_path_event(id, &PathEvent::Changed("tools/Cargo.toml".into()))
        .unwrap();
    assert!(scan_job(&o), "{o:?}");
    scan(rec, id);
    assert!(
        fx.file_paths(id)
            .contains(&"tools/build/gen.rs".to_string())
    );
    std::fs::remove_file(root.join("tools/Cargo.toml")).unwrap();
    let o = rec
        .on_path_event(id, &PathEvent::Removed("tools/Cargo.toml".into()))
        .unwrap();
    assert!(scan_job(&o), "{o:?}");
    let st = scan(rec, id);
    assert!(
        !fx.file_paths(id).iter().any(|p| p.starts_with("tools/")),
        "{st:?}"
    );
    // Editing a manifest that was already there leaves the rules as they are: no scan.
    write(
        &root,
        "crates/common/Cargo.toml",
        "[package]\nname = \"common2\"\n",
    );
    let o = rec
        .on_path_event(id, &PathEvent::Changed("crates/common/Cargo.toml".into()))
        .unwrap();
    assert!(!scan_job(&o), "{o:?}");
}

/// This repository's own `.seleneignore` (decision 8): spike copies and eval dumps out, the
/// vendored lancedb crate out except the one patched file.
#[test]
fn repo_seleneignore_keeps_only_the_patched_lancedb_file() {
    let fx = setup();
    let root = fx.dir("repo");
    write(
        &root,
        ".seleneignore",
        include_str!("../../../.seleneignore"),
    );
    for f in [
        "crates/core/src/query/mod.rs",
        "patches/lancedb-0.40.0/src/job.rs",
        "patches/lancedb-0.40.0/src/query.rs",
        "patches/lancedb-0.40.0/src/table/merge.rs",
        "patches/lancedb-0.40.0/README.md",
        "spikes/native-stack/patches/lancedb-0.40.0/src/job.rs",
        "spikes/native-stack/src/main.rs",
        "eval/baselines/v2.md",
        "eval/golden.md",
    ] {
        write(&root, f, "x");
    }
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    let id = add(rec, &fx, &root);
    scan(rec, id);
    assert_eq!(
        fx.file_paths(id),
        vec![
            "crates/core/src/query/mod.rs",
            "eval/golden.md",
            "patches/lancedb-0.40.0/src/job.rs",
            "spikes/native-stack/src/main.rs",
        ]
    );
}

/// docs/08 §4: the repo `.seleneignore` drops the spikes' machine-written result dumps and the
/// vendored STL source, and nothing else of a spike.
#[test]
fn repo_seleneignore_drops_spike_result_dumps_and_the_vendored_stl() {
    let fx = setup();
    let root = fx.dir("repo");
    write(
        &root,
        ".seleneignore",
        include_str!("../../../.seleneignore"),
    );
    for f in [
        "spikes/gpu-tier/results/detect.txt",
        "spikes/gpu-tier/results/fallback.txt",
        "spikes/ipc-bridge/results/latency.txt",
        "spikes/gpu-tier/stlshim/vector_algorithms.cpp",
        "spikes/gpu-tier/src/main.rs",
        "spikes/gpu-tier/README.md",
        "docs/results.md",
        "crates/core/src/results/mod.rs",
    ] {
        write(&root, f, "x");
    }
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    let id = add(rec, &fx, &root);
    scan(rec, id);
    assert_eq!(
        fx.file_paths(id),
        vec![
            "crates/core/src/results/mod.rs",
            "docs/results.md",
            "spikes/gpu-tier/README.md",
            "spikes/gpu-tier/src/main.rs",
        ]
    );
}
