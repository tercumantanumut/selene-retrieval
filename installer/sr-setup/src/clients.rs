//! MCP client entries written by the app's "Connect" buttons (M2-A request, R-UX15; M4 adds Cursor
//! and VS Code): on uninstall our `selene-retrieval` entry is removed from every Claude Desktop,
//! Cursor and VS Code config with **the app's own writers**: `crates/app/src/connect/` compiled
//! verbatim (`#[path]` in main.rs), so detection (Claude Squirrel + MSIX, VS Code + Insiders),
//! merge, backup and verify rules have one implementation. Only our key is removed; other servers
//! and unknown fields stay; files that aren't plain JSON (e.g. VS Code `mcp.json` with comments)
//! are left untouched. Nothing from the files is ever printed.
//! Our own backups (`<config>.selene-retrieval.bak`) are deleted afterwards: they are copies of the
//! user's configs that we created, and uninstall leaves nothing behind. Files kept by "Repair…"
//! (`<config>.broken-<date>`) are the user's own data and are **kept**.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

use crate::connect::jsonfile::{self, Outcome};
use crate::connect::{ClientRoots, claude, cursor, vscode};

/// A known folder via the shell (never `%APPDATA%` / `%USERPROFILE%` from the environment).
fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    // SAFETY: on success the shell returns a CoTaskMem string that we copy and then free once.
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
        s.map(PathBuf::from)
    }
}

pub fn user_roots() -> Option<ClientRoots> {
    use windows::Win32::UI::Shell::{FOLDERID_Profile, FOLDERID_RoamingAppData};
    // Dev hook (debug builds only, ignored in release like every `dev_env`): the same fake
    // `Roaming` / `Local` / `Home` tree the app's `SR_CLAUDE_ROOTS_DIR` uses, so a live uninstall
    // test never touches a real Claude / Cursor / VS Code config (M4 tester).
    if let Some(d) = sr_client::naming::dev_env("SR_CLAUDE_ROOTS_DIR") {
        let d = PathBuf::from(d);
        return Some(ClientRoots {
            roaming: d.join("Roaming"),
            local: d.join("Local"),
            home: d.join("Home"),
        });
    }
    Some(ClientRoots {
        roaming: known_folder(&FOLDERID_RoamingAppData)?,
        local: sr_client::win::local_appdata().ok()?,
        home: known_folder(&FOLDERID_Profile)?,
    })
}

/// One config file of one app.
struct Target {
    label: String,
    path: PathBuf,
    servers_key: &'static str,
    backup_suffix: &'static str,
}

fn targets(roots: &ClientRoots) -> Vec<Target> {
    let mut v: Vec<Target> = claude::detect(&roots.claude())
        .into_iter()
        .map(|i| Target {
            label: format!("{:?} Claude", i.variant),
            path: i.config_path,
            servers_key: claude::SERVERS_KEY,
            backup_suffix: claude::BACKUP_SUFFIX,
        })
        .collect();
    // Cursor: also checked when only the config file is left (the program may be gone already).
    let cursor_cfg = cursor::config_path(roots);
    if cursor::detect(roots).is_some() || cursor_cfg.exists() {
        v.push(Target {
            label: "Cursor".into(),
            path: cursor_cfg,
            servers_key: cursor::SERVERS_KEY,
            backup_suffix: cursor::BACKUP_SUFFIX,
        });
    }
    for i in vscode::detect(roots) {
        v.push(Target {
            label: format!("VS Code ({:?})", i.variant),
            path: i.config_path,
            servers_key: vscode::SERVERS_KEY,
            backup_suffix: vscode::BACKUP_SUFFIX,
        });
    }
    v
}

fn backup_of(config: &Path, suffix: &str) -> PathBuf {
    jsonfile::sibling(config, suffix)
}

/// One result line of [`disconnect_all`].
#[derive(Debug)]
pub struct Line {
    pub ok: bool,
    pub msg: String,
    /// Set when the user has to remove our entry by hand: the file still mentions
    /// `selene-retrieval` but isn't plain JSON (e.g. a VS Code `mcp.json` with comments; we never
    /// rewrite such a file) or couldn't be written. Named in the uninstall summary (R-UX43).
    pub manual: Option<PathBuf>,
}

impl Line {
    fn ok(msg: String) -> Self {
        Line {
            ok: true,
            msg,
            manual: None,
        }
    }
}

/// Whether the raw bytes of a config still name our server key (checked only when the file
/// can't be parsed; nothing from the file is printed or kept).
fn mentions_us(path: &Path) -> bool {
    let needle = format!("\"{}\"", crate::connect::SERVER_KEY);
    std::fs::read(path)
        .map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
        .unwrap_or(true) // can't even read it: say so rather than stay silent
}

/// One line per client config.
pub fn disconnect_all(roots: &ClientRoots) -> Vec<Line> {
    let mut out = Vec::new();
    for t in targets(roots) {
        let where_ = &t.label;
        match jsonfile::disconnect(&t.path, t.servers_key, t.backup_suffix) {
            Ok(Outcome::Written) => out.push(Line::ok(format!("{where_}: Selene Retrieval removed"))),
            Ok(Outcome::Unchanged) => out.push(Line::ok(format!("{where_}: was not connected"))),
            Err(jsonfile::WriteError::Unreadable) if !mentions_us(&t.path) => out.push(Line::ok(
                format!("{where_}: settings file has comments or isn't plain JSON; it doesn't list Selene Retrieval, left as is"),
            )),
            Err(e) => out.push(Line {
                ok: false,
                msg: format!("{where_}: settings not updated ({e:?}); remove the \"selene-retrieval\" entry by hand: {}", t.path.display()),
                manual: Some(t.path.clone()),
            }),
        }
        let bak = backup_of(&t.path, t.backup_suffix);
        if bak.exists() {
            match std::fs::remove_file(&bak) {
                Ok(()) => out.push(Line::ok(format!("{where_}: our settings backup removed"))),
                Err(e) => out.push(Line {
                    ok: false,
                    msg: format!("{where_}: backup not removed ({:?})", e.kind()),
                    manual: None,
                }),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn roots(t: &tempfile::TempDir) -> ClientRoots {
        ClientRoots {
            roaming: t.path().join("Roaming"),
            local: t.path().join("Local"),
            home: t.path().join("Home"),
        }
    }

    fn write(p: &Path, v: &Value) {
        std::fs::create_dir_all(p.parent().unwrap_or(Path::new(".")))
            .unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(p, serde_json::to_vec(v).unwrap_or_default())
            .unwrap_or_else(|e| panic!("{e}"));
    }

    fn read(p: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(p).unwrap_or_default()).unwrap_or(Value::Null)
    }

    #[test]
    fn removes_only_our_entry_and_our_backup() {
        let t = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let roots = roots(&t);
        std::fs::create_dir_all(roots.local.join("AnthropicClaude"))
            .unwrap_or_else(|e| panic!("{e}"));
        let cfg = roots.roaming.join("Claude").join(claude::CONFIG_FILE);
        let before = json!({
            "theme": "dark",
            "mcpServers": {
                "other": { "command": "x.exe", "env": { "TOKEN": "keep-me" } },
                "selene-retrieval": { "command": "C:\\gone\\sr-bridge.exe", "args": ["--client", "claude"] }
            }
        });
        write(&cfg, &before);
        std::fs::write(backup_of(&cfg, claude::BACKUP_SUFFIX), b"{}")
            .unwrap_or_else(|e| panic!("{e}"));

        let lines = disconnect_all(&roots);
        assert!(
            lines.iter().all(|l| l.ok && l.manual.is_none()),
            "{lines:?}"
        );
        let after = read(&cfg);
        assert_eq!(after["theme"], "dark");
        assert_eq!(after["mcpServers"]["other"]["env"]["TOKEN"], "keep-me");
        assert!(after["mcpServers"].get("selene-retrieval").is_none());
        assert!(
            !backup_of(&cfg, claude::BACKUP_SUFFIX).exists(),
            "the writer's backup must not be left behind"
        );
        // Second run: nothing to do, nothing written.
        let again = disconnect_all(&roots);
        assert!(
            again.iter().any(|l| l.msg.contains("was not connected")),
            "{again:?}"
        );
    }

    #[test]
    fn removes_cursor_and_vscode_entries_and_keeps_theirs() {
        let t = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let roots = roots(&t);
        let cur = roots.home.join(".cursor").join(cursor::CONFIG_FILE);
        write(
            &cur,
            &json!({"mcpServers": {
                "gh": {"command": "npx", "env": {"GITHUB_TOKEN": "keep-cursor"}},
                "selene-retrieval": {"command": "C:\\SR\\sr-bridge.exe", "args": ["--client", "cursor"]}
            }}),
        );
        let code = roots
            .roaming
            .join("Code")
            .join("User")
            .join(vscode::CONFIG_FILE);
        write(
            &code,
            &json!({"inputs": [], "servers": {
                "x": {"type": "http", "url": "https://x", "headers": {"A": "keep-vscode"}},
                "selene-retrieval": {"type": "stdio", "command": "C:\\SR\\sr-bridge.exe", "args": ["--client", "vscode"]}
            }}),
        );
        std::fs::write(backup_of(&code, vscode::BACKUP_SUFFIX), b"{}")
            .unwrap_or_else(|e| panic!("{e}"));
        let kept = jsonfile::sibling(&cur, ".broken-20261019-000000");
        std::fs::write(&kept, b"{ user data").unwrap_or_else(|e| panic!("{e}"));

        let lines = disconnect_all(&roots);
        assert!(lines.iter().all(|l| l.ok), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.msg.starts_with("Cursor:")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.msg.starts_with("VS Code")),
            "{lines:?}"
        );
        let c = read(&cur);
        assert_eq!(c["mcpServers"]["gh"]["env"]["GITHUB_TOKEN"], "keep-cursor");
        assert!(c["mcpServers"].get("selene-retrieval").is_none());
        let v = read(&code);
        assert_eq!(v["servers"]["x"]["headers"]["A"], "keep-vscode");
        assert!(v["servers"].get("selene-retrieval").is_none());
        assert_eq!(v["inputs"], json!([]));
        assert!(!backup_of(&code, vscode::BACKUP_SUFFIX).exists());
        assert!(kept.exists(), "a Repair copy is the user's data and stays");
    }

    #[test]
    fn a_vscode_file_with_comments_is_left_alone() {
        let t = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let roots = roots(&t);
        let code = roots
            .roaming
            .join("Code")
            .join("User")
            .join(vscode::CONFIG_FILE);
        std::fs::create_dir_all(code.parent().unwrap_or(Path::new(".")))
            .unwrap_or_else(|e| panic!("{e}"));
        let jsonc = "{\n // mine\n \"servers\": {\"selene-retrieval\": {}}\n}\n";
        std::fs::write(&code, jsonc).unwrap_or_else(|e| panic!("{e}"));
        let lines = disconnect_all(&roots);
        assert!(
            lines.iter().any(|l| !l.ok
                && l.msg.contains("not updated")
                && l.manual.as_deref() == Some(code.as_path())),
            "{lines:?}"
        );
        assert_eq!(std::fs::read_to_string(&code).unwrap_or_default(), jsonc);
    }

    #[test]
    fn a_commented_file_without_our_entry_needs_no_hand_edit() {
        let t = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let roots = roots(&t);
        let code = roots
            .roaming
            .join("Code")
            .join("User")
            .join(vscode::CONFIG_FILE);
        std::fs::create_dir_all(code.parent().unwrap_or(Path::new(".")))
            .unwrap_or_else(|e| panic!("{e}"));
        let jsonc = "{
 // mine
 \"servers\": {\"other\": {}}
}
";
        std::fs::write(&code, jsonc).unwrap_or_else(|e| panic!("{e}"));
        let lines = disconnect_all(&roots);
        assert!(
            lines.iter().all(|l| l.ok && l.manual.is_none()),
            "{lines:?}"
        );
        assert_eq!(std::fs::read_to_string(&code).unwrap_or_default(), jsonc);
    }

    #[test]
    fn no_client_installed_is_a_no_op() {
        let t = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        assert!(disconnect_all(&roots(&t)).is_empty());
    }
}
