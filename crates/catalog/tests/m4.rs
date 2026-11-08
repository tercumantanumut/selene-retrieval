//! M4-E catalog: skipped-files paging and retry (page 25), folder groups over real folders
//! (page 31), activity paging (page 30).
// Test crate: helpers outside #[test] fns may unwrap (clippy.toml covers only #[test] fns).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use sr_catalog::SKIP_REASON_FILES_ONLY;
use sr_catalog::repo::activity::{self, NewActivity};
use sr_catalog::repo::{files, groups, jobs};
use sr_common::api::ActivityKind;
use sr_common::error::ErrorCode;

#[test]
fn skipped_list_hides_listed_files_and_retry_requeues_dead_files() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("docs"));
    let bad = add_file(&fx.cat, folder, "bad.pdf", 1);
    let big = add_file(&fx.cat, folder, "big.bin", 2);
    let listed = add_file(&fx.cat, folder, "list.md", 3);
    let ok = add_file(&fx.cat, folder, "ok.md", 4);
    fx.cat
        .write_blocking(move |c| {
            files::mark_dead(c, bad, ErrorCode::FileUnreadable, NOW)?;
            files::mark_skipped(c, big, "too_large", Some(ErrorCode::FileTooLarge), NOW + 1)?;
            files::mark_skipped(c, listed, SKIP_REASON_FILES_ONLY, None, NOW + 2)?;
            Ok(())
        })
        .unwrap();
    let _ = ok;
    let (total, page) = fx
        .cat
        .read(|c| files::skipped_page(c, folder, 0, 50))
        .unwrap();
    assert_eq!(total, 2, "listed files are never skipped");
    let ids: Vec<_> = page.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![big, bad], "newest first");
    let (_, second) = fx
        .cat
        .read(|c| files::skipped_page(c, folder, 1, 1))
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].id, bad);

    // Retry: only the dead file is re-queued (a size limit won't change by retrying).
    let queued = fx
        .cat
        .write_blocking(move |c| files::retry_dead(c, folder, None, NOW + 5))
        .unwrap();
    assert_eq!(queued, vec![bad]);
    let rec = fx.cat.read(|c| files::file(c, bad)).unwrap();
    assert_eq!(rec.state, sr_catalog::FileState::PendingIndex);
    assert_eq!(rec.attempts, 0);
    let live = fx.cat.read(|c| jobs::live_jobs(c, Some(folder))).unwrap();
    assert!(
        live.iter()
            .any(|j| j.file_id == Some(bad) && j.kind == sr_catalog::JobKind::IndexFile),
        "{live:?}"
    );
    // Retrying a named file that isn't dead does nothing.
    let none = fx
        .cat
        .write_blocking(move |c| files::retry_dead(c, folder, Some(&[big]), NOW + 6))
        .unwrap();
    assert!(none.is_empty());
}

#[test]
fn groups_over_real_folders_and_activity_paging() {
    let fx = setup();
    let a = add_folder(&fx.cat, fx.collection, &fx.root("a"));
    let b = add_folder(&fx.cat, fx.collection, &fx.root("b"));
    let (def, work) = fx
        .cat
        .write_blocking(move |c| {
            let d = groups::ensure_default(c, NOW)?;
            let w = groups::create(c, "Work", NOW)?;
            groups::move_folder(c, b, w, NOW)?;
            Ok((d, w))
        })
        .unwrap();
    let views = fx.cat.read(groups::views).unwrap();
    assert_eq!(views.len(), 2);
    assert_eq!(views[0].group_id, def);
    assert_eq!(views[0].folder_ids, vec![a]);
    assert_eq!(views[1].group_id, work);
    assert_eq!(views[1].folder_ids, vec![b]);
    let moved = fx
        .cat
        .write_blocking(move |c| groups::delete(c, work))
        .unwrap();
    assert_eq!(moved, 1);
    assert_eq!(fx.cat.read(move |c| groups::group_of(c, b)).unwrap(), def);

    let e = fx
        .cat
        .write_blocking(move |c| {
            activity::append(
                c,
                &NewActivity::new(ActivityKind::FolderAdded).folder(a, "a"),
                NOW,
            )
        })
        .unwrap();
    let (page, next) = fx
        .cat
        .read(|c| activity::page(c, None, 10, NOW - 1))
        .unwrap();
    assert_eq!(page, vec![e]);
    assert!(next.is_none());
}
