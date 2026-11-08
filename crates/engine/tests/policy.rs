//! M5-E: an administrator's machine policy (`policy.toml` / registry) over a real engine.
//!
//! One test function: the policy is installed process-wide (the sandbox's root rules read it), so
//! this file is its own test binary and nothing else runs next to it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use serde_json::json;
use sr_common::config::policy::{MANAGED_TEXT, Policy, paths};
use sr_common::error::ErrorCode;
use sr_engine::{Engine, ExtractSetup};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_wins_over_settings_and_locks_features() {
    let t = tmp();
    let data = t.path().join("d");
    let allowed = t.path().join("allowed");
    let outside = t.path().join("outside");
    write_files(&allowed.join("work"), &[("a.md", "alpha")]);
    write_files(&outside, &[("b.md", "beta")]);
    // The user's settings ask for HTTP, MCP add_folder and sync_folder; the policy forbids all.
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("settings.toml"),
        "[integrations]\nhttp_server_enabled = true\nmcp_add_folder_enabled = true\nmcp_sync_folder_enabled = true\n",
    )
    .unwrap();
    let mut cfg = config(&data, ExtractSetup::Disabled);
    cfg.policy = Some(Policy {
        allow_remote_providers: Some(false),
        allow_http_server: Some(false),
        allow_mcp_add_folder: Some(false),
        allow_mcp_sync_folder: Some(false),
        allowed_folder_roots: Some(vec![allowed.clone()]),
        allow_model_downloads: Some(false),
        sources: vec!["policy.toml".into()],
        ..Policy::default()
    });
    let e = Engine::start(cfg).await.unwrap();

    // Policy wins over settings.toml.
    assert!(!e.http_status().enabled);
    assert!(!e.mcp_add_folder_enabled());
    assert!(!e.mcp_sync_folder_enabled() && !e.tool_flags().sync_folder);
    let st = e.status().value;
    for p in [
        paths::HTTP_SERVER,
        paths::MCP_ADD_FOLDER,
        paths::MCP_SYNC_FOLDER,
        paths::FOLDER_ROOTS,
    ] {
        assert!(
            st.managed
                .iter()
                .any(|f| f.path == p && f.message == MANAGED_TEXT),
            "{p} not listed: {:?}",
            st.managed
        );
    }
    let ps = e.rpc("policy.status", json!({})).await.unwrap();
    assert_eq!(ps["managed"], true);

    // Folders: inside the allowed roots works, outside is refused with the policy's own words.
    e.add_folder(&allowed.join("work").to_string_lossy(), None)
        .await
        .unwrap();
    let err = e
        .add_folder(&outside.to_string_lossy(), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ManagedByPolicy);
    assert_eq!(err.user_message().title, "Managed by your organisation");

    // Locked switches are refused plainly; turning things off still works.
    assert_eq!(
        e.http_set_enabled(true).await.unwrap_err().code,
        ErrorCode::ManagedByPolicy
    );
    e.http_set_enabled(false).await.unwrap();
    for u in [
        json!({ "mcp_add_folder_enabled": true }),
        json!({ "mcp_sync_folder_enabled": true }),
        json!({ "rerank_enabled": true }),
    ] {
        let u = serde_json::from_value(u).unwrap();
        assert_eq!(
            e.update_advanced(u).await.unwrap_err().code,
            ErrorCode::ManagedByPolicy
        );
    }
    // Cloud AI providers are refused before a key is stored or sent anywhere.
    let r = e
        .rpc(
            "providers.save",
            json!({ "provider": "anthropic", "key": "sk-test-not-real", "check": false }),
        )
        .await
        .unwrap_err();
    assert_eq!(r.code, ErrorCode::ManagedByPolicy);
    // Optional model downloads are off (the built-in model keeps working).
    let r = e
        .rpc("search_model.choose", json!({ "model": "multilingual" }))
        .await
        .unwrap_err();
    assert_eq!(r.code, ErrorCode::ManagedByPolicy, "{r}");

    // A later settings write keeps the locked values in the file too.
    let u = serde_json::from_value(json!({ "use_graphics_card": false })).unwrap();
    e.update_advanced(u).await.unwrap();
    let text = std::fs::read_to_string(data.join("settings.toml")).unwrap();
    assert!(text.contains("http_server_enabled = false"), "{text}");
    assert!(text.contains("mcp_add_folder_enabled = false"), "{text}");
    assert!(text.contains("mcp_sync_folder_enabled = false"), "{text}");
    e.drain().await;
}
