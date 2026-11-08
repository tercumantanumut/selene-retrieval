//! "Claude (sign in)" / "Codex (sign in)" over the engine's app-RPC (M5-C, Amendments 10.2 /
//! 10.3) against FAKE `claude.cmd` / `codex.cmd` scripts only: the real CLIs are never started,
//! no prompt is sent anywhere, and no token exists anywhere in Selene Retrieval.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::*;
use serde_json::json;
use sr_common::error::ErrorCode;
use sr_engine::ExtractSetup;
use sr_providers::{MemorySecretStore, SecretStore};

/// `auth status --json` prints `status.json`; `--version` a version. Nothing else is used here.
const FAKE_CLAUDE: &str = "@echo off\r\nif \"%~1\"==\"--version\" (\r\n  echo 2.1.999\r\n  exit /b 0\r\n)\r\nif \"%~1\"==\"auth\" (\r\n  type \"%~dp0status.json\"\r\n  exit /b 0\r\n)\r\nexit /b 1\r\n";

fn fake(dir: &std::path::Path, status: &str) -> PathBuf {
    let exe = dir.join("claude.cmd");
    std::fs::write(&exe, FAKE_CLAUDE).unwrap();
    std::fs::write(dir.join("status.json"), status).unwrap();
    exe
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uses_the_local_claude_code_sign_in() {
    let t = tmp();
    let data = t.path().join("d");
    let cli = t.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let secrets = Arc::new(MemorySecretStore::new());
    // A token left by the removed OAuth design is deleted at start.
    secrets
        .set("claude-signin", "{\"access_token\":\"old\"}")
        .unwrap();
    let mut cfg = config(&data, ExtractSetup::Disabled);
    cfg.secrets = Some(secrets.clone());
    let e = sr_engine::Engine::start(cfg).await.unwrap();
    assert!(secrets.get("claude-signin").unwrap().is_none());

    // Not installed: plain state + the install page, no error.
    e.set_cli_path("claudecode", Some(cli.join("missing.cmd")));
    let st = e
        .rpc("claude_signin.status", json!({ "refresh": true }))
        .await
        .unwrap();
    assert_eq!(st["state"], "not_installed", "{st}");
    assert_eq!(st["cli_found"], false);
    assert_eq!(st["install_url"], "https://claude.com/claude-code");
    // Not installed: [Sign in] opens nothing.
    let s = e.rpc("claude_signin.start", json!({})).await.unwrap();
    assert_eq!(s["started"], false);

    // Installed, signed out.
    let exe = fake(&cli, r#"{"loggedIn":false}"#);
    e.set_cli_path("claudecode", Some(exe.clone()));
    let st = e
        .rpc(
            "cli_signin.status",
            json!({ "provider": "claudecode", "refresh": true }),
        )
        .await
        .unwrap();
    assert_eq!(st["state"], "signed_out", "{st}");
    assert_eq!(st["cli_found"], true);
    assert_eq!(st["cli_version"], "2.1.999");
    let err = e
        .rpc(
            "providers.save",
            json!({ "provider": "claudecode", "key": null, "address": null, "check": true }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ProviderAuth);
    assert_eq!(
        err.user_message().title,
        "Claude Code isn't signed in",
        "plain text, one action"
    );

    // Signed in (in Claude Code, outside Selene Retrieval): [Check again] sees it.
    std::fs::write(
        cli.join("status.json"),
        r#"{"loggedIn":true,"authMethod":"claude.ai","email":"fake@example.com","subscriptionType":"max"}"#,
    )
    .unwrap();
    let st = e
        .rpc("claude_signin.status", json!({ "refresh": true }))
        .await
        .unwrap();
    assert_eq!(st["authenticated"], true, "{st}");
    assert_eq!(st["email"], "fake@example.com");
    assert_eq!(st["plan"], "Max");
    assert_eq!(st["label"], "Claude (sign in)");
    // Already signed in: [Sign in] opens no window.
    let s = e.rpc("claude_signin.start", json!({})).await.unwrap();
    assert_eq!(s["started"], false);
    assert_eq!(s["authenticated"], true);

    // Choosing it needs no key and no address; it connects without sending anything.
    let saved = e
        .rpc(
            "providers.save",
            json!({ "provider": "claudecode", "key": null, "address": "https://evil.example", "check": true }),
        )
        .await
        .unwrap();
    assert_eq!(saved["validated"], true, "{saved}");
    assert_eq!(saved["status"]["provider"], "claudecode");
    assert_eq!(saved["status"]["ready"], true, "{saved}");
    assert_eq!(saved["status"]["key_saved"], false);
    assert_eq!(saved["status"]["address"], serde_json::Value::Null);
    assert_eq!(saved["status"]["models_are_hints"], true);
    // Nothing secret anywhere: there is no token to leak.
    assert!(!e.diagnostics().to_string().contains("fake@example.com"));

    // Signed out in Claude Code: the next check disconnects the provider (tools go).
    std::fs::write(cli.join("status.json"), r#"{"loggedIn":false}"#).unwrap();
    let st = e
        .rpc("claude_signin.status", json!({ "refresh": true }))
        .await
        .unwrap();
    assert_eq!(st["authenticated"], false);
    assert!(e.llm_slot().is_none());

    // Unknown provider ids are refused; codex has its own status.
    let err = e
        .rpc("cli_signin.status", json!({ "provider": "openai" }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput);
    e.set_cli_path("codex", Some(cli.join("no-codex.cmd")));
    let st = e
        .rpc("cli_signin.status", json!({ "provider": "codex" }))
        .await
        .unwrap();
    assert_eq!(st["state"], "not_installed");
    assert_eq!(st["label"], "Codex (sign in)");
    assert_eq!(st["install_url"], "https://developers.openai.com/codex/cli");
    e.drain().await;
}
