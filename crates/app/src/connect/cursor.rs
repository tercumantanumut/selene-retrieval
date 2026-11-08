//! Cursor: install detection and the `mcpServers` merge writer (M4, page 29).
//!
//! Cursor's global MCP config is `%USERPROFILE%\.cursor\mcp.json`, in the same `mcpServers`
//! format as Claude Desktop (`{"command", "args"}`). Cursor is installed per user (an Inno-style
//! exe under `%LOCALAPPDATA%\Programs\cursor`) or machine-wide; there is no MSIX build. It counts
//! as installed when any of its per-user folders exists: the program folder, its settings folder
//! `%APPDATA%\Cursor` (created on first run) or `%USERPROFILE%\.cursor`.
//!
//! The writer is [`super::jsonfile`]: only our entry changes, a backup is kept next to the file,
//! the write is atomic and verified (restored on mismatch), and nothing from the file is logged.

use std::path::{Path, PathBuf};

use serde_json::Value;

pub use super::jsonfile::{EntryState, Outcome, WriteError};
use super::{ClientRoots, jsonfile};

pub const CONFIG_DIR: &str = ".cursor";
pub const CONFIG_FILE: &str = "mcp.json";
pub const BACKUP_SUFFIX: &str = ".selene-retrieval.bak";
pub const SERVERS_KEY: &str = "mcpServers";
/// `--client` id the bridge reports (sr-common `client_kind_from_id` → Cursor).
pub const CLIENT_ID: &str = "cursor";

/// Cursor's config file for this user, when Cursor is installed.
pub fn detect(roots: &ClientRoots) -> Option<PathBuf> {
    let installed = roots.local.join("Programs").join("cursor").is_dir()
        || roots.roaming.join("Cursor").is_dir()
        || roots.home.join(CONFIG_DIR).is_dir();
    installed.then(|| config_path(roots))
}

pub fn config_path(roots: &ClientRoots) -> PathBuf {
    roots.home.join(CONFIG_DIR).join(CONFIG_FILE)
}

/// Our entry (same shape as Claude's, with Cursor's client id).
pub fn entry(bridge_exe: &Path) -> Value {
    super::server_entry(bridge_exe, CLIENT_ID)
}

/// Copy-paste text in Cursor's format (OQ-44).
pub fn snippet(bridge_exe: &Path) -> String {
    super::snippet(bridge_exe, CLIENT_ID)
}

#[cfg_attr(not(test), allow(dead_code))] // per-app shortcut; the app and sr-setup use `jsonfile` with a target
pub fn entry_state(path: &Path, entry: &Value) -> EntryState {
    jsonfile::entry_state(path, SERVERS_KEY, entry)
}

#[cfg_attr(not(test), allow(dead_code))] // per-app shortcut; the app and sr-setup use `jsonfile` with a target
pub fn connect(path: &Path, entry: &Value) -> Result<Outcome, WriteError> {
    jsonfile::connect(path, SERVERS_KEY, BACKUP_SUFFIX, entry)
}

#[cfg_attr(not(test), allow(dead_code))] // per-app shortcut; the app and sr-setup use `jsonfile` with a target
pub fn disconnect(path: &Path) -> Result<Outcome, WriteError> {
    jsonfile::disconnect(path, SERVERS_KEY, BACKUP_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::SERVER_KEY;
    use std::fs;

    const SECRET: &str = "cursor-SECRET-do-not-leak-789";

    fn roots(t: &tempfile::TempDir) -> ClientRoots {
        let r = ClientRoots {
            roaming: t.path().join("Roaming"),
            local: t.path().join("Local"),
            home: t.path().join("Home"),
        };
        for d in [&r.roaming, &r.local, &r.home] {
            fs::create_dir_all(d).unwrap();
        }
        r
    }

    fn bridge() -> PathBuf {
        PathBuf::from(r"C:\Users\u\AppData\Local\Selene Retrieval\sr-bridge.exe")
    }

    #[test]
    fn detection_needs_one_of_cursors_folders() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(&t);
        assert_eq!(detect(&r), None);
        fs::create_dir_all(r.local.join("Programs").join("cursor")).unwrap();
        assert_eq!(detect(&r), Some(r.home.join(".cursor").join("mcp.json")));
        let t2 = tempfile::tempdir().unwrap();
        let r2 = roots(&t2);
        fs::create_dir_all(r2.home.join(".cursor")).unwrap();
        assert!(detect(&r2).is_some());
    }

    #[test]
    fn connect_merges_keeps_secrets_and_disconnects_only_ours() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(&t);
        let p = config_path(&r);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let user = serde_json::json!({
            "mcpServers": {"gh": {"command": "npx", "env": {"GITHUB_TOKEN": SECRET}}},
            "somethingElse": [1, 2]
        });
        fs::write(&p, serde_json::to_vec_pretty(&user).unwrap()).unwrap();
        let e = entry(&bridge());
        assert_eq!(e["args"], serde_json::json!(["--client", "cursor"]));
        assert_eq!(entry_state(&p, &e), EntryState::NotConnected);
        assert_eq!(connect(&p, &e), Ok(Outcome::Written));
        assert_eq!(connect(&p, &e), Ok(Outcome::Unchanged));
        assert_eq!(entry_state(&p, &e), EntryState::Connected);
        let got: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(got["mcpServers"]["gh"]["env"]["GITHUB_TOKEN"], SECRET);
        assert_eq!(got["mcpServers"][SERVER_KEY], e);
        assert!(jsonfile::sibling(&p, BACKUP_SUFFIX).exists(), "backup kept");
        assert_eq!(disconnect(&p), Ok(Outcome::Written));
        let got: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(got, user);
    }

    #[test]
    fn missing_file_is_created_with_only_our_entry() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(&t);
        let p = config_path(&r);
        let e = entry(&bridge());
        assert_eq!(connect(&p, &e), Ok(Outcome::Written));
        let got: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(got, serde_json::json!({"mcpServers": {SERVER_KEY: e}}));
        assert!(snippet(&bridge()).contains("\"cursor\""));
    }
}
