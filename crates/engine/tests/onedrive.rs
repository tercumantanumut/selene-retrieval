//! M5-E: online-only (cloud placeholder) files are never downloaded by the service.
//!
//! A real OneDrive placeholder needs a sync root; `FILE_ATTRIBUTE_OFFLINE` (`attrib +O`, one of the
//! three attributes the service treats as online-only) gives the same metadata without one. The
//! test skips itself on a file system that refuses the attribute.
// Windows only: OneDrive placeholders and `attrib.exe`.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::*;
use sr_common::config::policy::Policy;
use sr_common::error::ErrorCode;
use sr_engine::files::{GrepRequest, ReadFileRequest};
use sr_engine::{Engine, ExtractSetup};

fn attrib(flag: &str, file: &Path) -> bool {
    let sys = std::env::var_os("SystemRoot").map_or_else(
        || std::path::PathBuf::from(r"C:\Windows"),
        std::path::PathBuf::from,
    );
    std::process::Command::new(sys.join("System32").join("attrib.exe"))
        .arg(flag)
        .arg(file)
        .status()
        .is_ok_and(|s| s.success())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn online_only_files_are_skipped_not_downloaded() {
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("onedrive");
    write_files(
        &root,
        &[
            ("here.md", "the quokka needle is on this device"),
            ("cloud.md", "the quokka needle is only in the cloud"),
        ],
    );
    let cloud = root.join("cloud.md");
    if !attrib("+O", &cloud) || !sr_engine::win::is_online_only(&cloud) {
        eprintln!("skipped: this file system doesn't keep FILE_ATTRIBUTE_OFFLINE");
        return;
    }
    let p = Profile::new("m5onedrive");
    let mut cfg = config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &p))),
    );
    cfg.policy = Some(Policy::unmanaged());
    let e = Engine::start(cfg).await.unwrap();
    let f = e.add_folder(&root.to_string_lossy(), None).await.unwrap();
    e.run_pending_jobs().await.unwrap();

    // Indexing skipped it (skip reason online_only) and the folder says what to do.
    let info = e.list_folders().await.unwrap();
    let folder = info.iter().find(|x| x.folder_id == f.folder_id).unwrap();
    assert_eq!(folder.files_indexed, 1, "{folder:?}");
    assert_eq!(
        folder.cloud_note.as_deref(),
        Some(sr_common::api::ONLINE_ONLY_NOTE)
    );
    assert_eq!(folder.online_only_skipped, 1, "{folder:?}");

    // read_file refuses it plainly instead of downloading it.
    let err = e
        .read_file(ReadFileRequest {
            path: cloud.to_string_lossy().into_owned(),
            start_line: None,
            end_line: None,
            head: None,
            tail: None,
            folder_ids: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::FileOnlineOnly);
    assert!(
        err.user_message()
            .agent_text()
            .contains("Always keep on this device"),
        "{}",
        err.user_message().agent_text()
    );
    // grep searches the local file and reports the skipped one.
    let g = e
        .grep(GrepRequest {
            pattern: "quokka needle".into(),
            path: None,
            folder_ids: None,
            glob: None,
            case_insensitive: false,
            fixed_strings: true,
            max_results: None,
        })
        .await
        .unwrap();
    assert_eq!(g.matches.len(), 1, "{g:?}");
    assert!(g.matches[0].rel_path.ends_with("here.md"));
    assert_eq!(g.online_only_skipped, 1);
    assert!(g.note.is_some());
    // The attribute is still set: nothing downloaded it (a recall would clear it).
    assert!(sr_engine::win::is_online_only(&cloud));

    // "Always keep on this device": once it is local, the next check indexes it.
    assert!(attrib("-O", &cloud));
    e.sync_folder(Some(f.folder_id)).await.unwrap();
    e.run_pending_jobs().await.unwrap();
    let info = e.list_folders().await.unwrap();
    let folder = info.iter().find(|x| x.folder_id == f.folder_id).unwrap();
    assert_eq!(folder.files_indexed, 2, "{folder:?}");
    assert_eq!(folder.cloud_note, None);
    assert_eq!(folder.online_only_skipped, 0);
    e.drain().await;
}
