//! VS Code: install detection and the `servers` merge writer (M4, page 29).
//!
//! VS Code keeps user-level MCP servers in `<user data>\User\mcp.json` as
//! `{"servers": {"<name>": {"type": "stdio", "command", "args"}}, "inputs": [...]}`. The user data
//! folder is `%APPDATA%\Code` for VS Code and `%APPDATA%\Code - Insiders` for Insiders (both the
//! per-user and the machine-wide installer use it; there is no MSIX build). A variant counts as
//! installed when its user data folder exists (created on first run) or, for VS Code, when the
//! per-user program folder `%LOCALAPPDATA%\Programs\Microsoft VS Code` exists. Portable installs
//! (a `data` folder next to `Code.exe`) can't be found from the user's folders: they get the
//! copy-paste text.
//!
//! `mcp.json` is JSON with comments (JSONC) as far as VS Code is concerned. A file that uses
//! comments or trailing commas is **not** edited (writing it back would drop the user's
//! comments); it shows the "couldn't be read" state with the setup text instead.

use std::path::{Path, PathBuf};

use serde_json::Value;

pub use super::jsonfile::{EntryState, Outcome, WriteError};
use super::{ClientRoots, SERVER_KEY, jsonfile};

pub const CONFIG_FILE: &str = "mcp.json";
pub const BACKUP_SUFFIX: &str = ".selene-retrieval.bak";
pub const SERVERS_KEY: &str = "servers";
/// `--client` id the bridge reports (sr-common `client_kind_from_id` → VS Code).
pub const CLIENT_ID: &str = "vscode";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Stable,
    Insiders,
}

impl Variant {
    /// User data folder name under `%APPDATA%`.
    pub const fn data_dir(self) -> &'static str {
        match self {
            Variant::Stable => "Code",
            Variant::Insiders => "Code - Insiders",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub variant: Variant,
    pub config_path: PathBuf,
}

/// Every installed VS Code variant and the `mcp.json` it reads.
pub fn detect(roots: &ClientRoots) -> Vec<Install> {
    let mut out = Vec::new();
    for v in [Variant::Stable, Variant::Insiders] {
        let data = roots.roaming.join(v.data_dir());
        let program = match v {
            Variant::Stable => roots.local.join("Programs").join("Microsoft VS Code"),
            Variant::Insiders => roots
                .local
                .join("Programs")
                .join("Microsoft VS Code Insiders"),
        };
        if data.is_dir() || program.is_dir() {
            out.push(Install {
                variant: v,
                config_path: data.join("User").join(CONFIG_FILE),
            });
        }
    }
    out
}

/// Our entry in VS Code's format (`"type": "stdio"` is required there).
pub fn entry(bridge_exe: &Path) -> Value {
    serde_json::json!({
        "type": "stdio",
        "command": bridge_exe.to_string_lossy(),
        "args": ["--client", CLIENT_ID],
    })
}

/// Copy-paste text in VS Code's `mcp.json` format (OQ-44).
pub fn snippet(bridge_exe: &Path) -> String {
    let v = serde_json::json!({ SERVERS_KEY: { SERVER_KEY: entry(bridge_exe) } });
    serde_json::to_string_pretty(&v).unwrap_or_default()
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
    use std::fs;

    const SECRET: &str = "vscode-SECRET-do-not-leak-321";

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
        PathBuf::from(r"C:\SR\sr-bridge.exe")
    }

    #[test]
    fn detects_stable_and_insiders() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(&t);
        assert!(detect(&r).is_empty());
        fs::create_dir_all(r.local.join("Programs").join("Microsoft VS Code")).unwrap();
        fs::create_dir_all(r.roaming.join("Code - Insiders")).unwrap();
        let d = detect(&r);
        assert_eq!(
            d.iter().map(|i| i.variant).collect::<Vec<_>>(),
            [Variant::Stable, Variant::Insiders]
        );
        assert_eq!(
            d[0].config_path,
            r.roaming.join("Code").join("User").join("mcp.json")
        );
    }

    #[test]
    fn connect_writes_servers_with_type_stdio_and_keeps_inputs() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(&t);
        let p = r.roaming.join("Code").join("User").join(CONFIG_FILE);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let user = serde_json::json!({
            "servers": {"gh": {"type": "http", "url": "https://api", "headers": {"Authorization": SECRET}}},
            "inputs": [{"type": "promptString", "id": "k", "password": true}]
        });
        fs::write(&p, serde_json::to_vec_pretty(&user).unwrap()).unwrap();
        let e = entry(&bridge());
        assert_eq!(e["type"], "stdio");
        assert_eq!(connect(&p, &e), Ok(Outcome::Written));
        let got: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(got["servers"]["gh"]["headers"]["Authorization"], SECRET);
        assert_eq!(got["inputs"], user["inputs"]);
        assert_eq!(got["servers"][SERVER_KEY], e);
        assert!(got.get("mcpServers").is_none(), "VS Code's own key only");
        assert_eq!(disconnect(&p), Ok(Outcome::Written));
        let got: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(got, user);
    }

    #[test]
    fn a_file_with_comments_stays_untouched() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(&t);
        let p = r.roaming.join("Code").join("User").join(CONFIG_FILE);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let jsonc = format!(
            "{{\n  // tokens\n  \"servers\": {{ \"x\": {{ \"env\": \"{SECRET}\" }} }}\n}}\n"
        );
        fs::write(&p, &jsonc).unwrap();
        assert_eq!(entry_state(&p, &entry(&bridge())), EntryState::Unreadable);
        let e = connect(&p, &entry(&bridge())).unwrap_err();
        assert!(!format!("{e:?}").contains(SECRET));
        assert_eq!(fs::read_to_string(&p).unwrap(), jsonc);
        assert!(snippet(&bridge()).contains("\"servers\""));
    }
}
