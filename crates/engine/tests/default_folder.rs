//! Amendment 13 ("Create a folder for me", `docs/07-default-folder-plan.md`) over a real engine:
//! the app's path rules make `Documents\Selene Retrieval` in an injected Documents folder, the
//! existing app-RPC `folders.add` accepts it like any folder, it is indexed and watched (a file
//! dropped in becomes searchable without a sync call), a second click finds the same folder, and
//! the service itself refuses a duplicate add, so two folders can never index the same place.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use sr_client::default_folder::{self, FOLDER_NAME};
use sr_common::error::ErrorCode;
use sr_engine::{Engine, ExtractSetup};

async fn searchable(e: &sr_engine::EngineHandle, token: &str, rel: &str) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let v = e
            .rpc(
                "search",
                json!({ "query": token, "max_results": 20, "min_score": 0.0 }),
            )
            .await
            .unwrap();
        let hit = v["hits"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|h| h["rel_path"].as_str() == Some(rel));
        if hit {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_folder_is_added_indexed_watched_and_idempotent() {
    let t = tmp();
    let data = t.path().join("d");
    // A fake Documents folder (never the real one).
    let docs = t.path().join("Documents");
    std::fs::create_dir_all(&docs).unwrap();
    let p = Profile::new("deffolder");
    let mut cfg = config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &p))),
    );
    cfg.background = true;
    let e = Engine::start(cfg).await.unwrap();

    // Click 1: made, then added through the app-RPC dispatcher like any picked folder.
    let made = default_folder::make(&docs).unwrap();
    assert!(made.created);
    assert_eq!(made.path, docs.join(FOLDER_NAME));
    let info: Value = e
        .rpc(
            "folders.add",
            json!({ "path": made.path.to_string_lossy() }),
        )
        .await
        .unwrap();
    assert_eq!(info["name"], FOLDER_NAME);
    let id = info["folder_id"].as_str().unwrap().to_owned();
    assert!(e.wait_idle(Duration::from_secs(120)).await);

    // A file dropped in right away (before the live watcher armed the new folder, ≤ 2 s) is
    // covered by the app's one catch-up check (`sr-app` `default_folder::CATCH_UP_AFTER`, R45).
    std::fs::write(
        made.path.join("notes.md"),
        "# Notes

The defaultfoldertoken is in the folder.
",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await; // = CATCH_UP_AFTER (the app's timing)
    e.rpc("folders.sync", json!({ "folder_id": id }))
        .await
        .unwrap();
    assert!(
        searchable(&e, "defaultfoldertoken", "notes.md").await,
        "a file dropped in right away wasn't searchable after the catch-up check"
    );
    // From then on the live watcher alone picks up whatever the user puts there (no sync call).
    std::fs::write(
        made.path.join("later.md"),
        "# Later

The laterfoldertoken shows the folder is watched.
",
    )
    .unwrap();
    assert!(
        searchable(&e, "laterfoldertoken", "later.md").await,
        "a file saved later wasn't picked up by the watcher within 30 s"
    );

    // Click 2: the same folder is found (nothing new made); the app sees it in the folder list...
    let again = default_folder::make(&docs).unwrap();
    assert_eq!(again.path, made.path);
    assert!(!again.created);
    let list = e.rpc("folders.list", Value::Null).await.unwrap();
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let listed = std::path::PathBuf::from(rows[0]["path"].as_str().unwrap());
    assert!(default_folder::is_default_folder(&docs, &listed) || listed.ends_with(FOLDER_NAME));
    assert_eq!(rows[0]["folder_id"], id.as_str());
    // ...and even if it sent a second add, the service refuses the duplicate.
    let dup = e
        .rpc(
            "folders.add",
            json!({ "path": made.path.to_string_lossy() }),
        )
        .await
        .unwrap_err();
    assert_eq!(dup.code, ErrorCode::FolderOverlap);
    assert!(
        made.path.join("notes.md").is_file(),
        "the user's file is untouched"
    );

    e.drain().await;
}
