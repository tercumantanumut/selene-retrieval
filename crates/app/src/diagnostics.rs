//! "Copy diagnostics": plain JSON for support. **No secrets, no query text, no file contents,
//! no folder names or paths, no client config contents.** Home-folder paths are shortened to
//! `%USERPROFILE%`, and any field whose name looks secret- or query-like is dropped.

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::dto::{AppView, ClaudeState};

/// Field names never copied (case-insensitive substring match).
const DROP_KEYS: &[&str] = &[
    "key",
    "token",
    "secret",
    "password",
    "credential",
    "auth",
    "query",
    "snippet",
    "text",
    "env",
];

#[derive(Debug, Clone, Default)]
pub struct Inputs<'a> {
    pub app_version: &'a str,
    pub os: String,
    pub daemon_pid: Option<u32>,
    pub daemon_proto: Option<u16>,
    pub daemon: Option<Value>,
    pub claude: Option<ClaudeState>,
    pub claude_variants: Vec<&'static str>,
    pub autostart: Option<bool>,
}

fn redact_str(s: &str, home: Option<&str>) -> String {
    let Some(h) = home.filter(|h| h.len() > 3) else {
        return s.to_owned();
    };
    // Case-insensitive replacement anywhere in the string (Windows paths). ASCII folding keeps
    // byte offsets identical between `s` and `lower`.
    let lower = s.to_ascii_lowercase();
    let hl = h.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(pos) = lower[i..].find(&hl) {
        out.push_str(&s[i..i + pos]);
        out.push_str("%USERPROFILE%");
        i += pos + hl.len();
    }
    out.push_str(&s[i..]);
    out
}

/// Recursively drops secret-like keys and shortens home paths.
pub fn redact(v: &Value, home: Option<&str>) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, val) in m {
                let kl = k.to_lowercase();
                if DROP_KEYS.iter().any(|d| kl.contains(d)) {
                    out.insert(k.clone(), Value::String("[removed]".into()));
                } else {
                    out.insert(k.clone(), redact(val, home));
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| redact(x, home)).collect()),
        Value::String(s) => Value::String(redact_str(s, home)),
        other => other.clone(),
    }
}

/// Builds the text placed on the clipboard.
pub fn build(inp: &Inputs<'_>, view: &AppView, home: Option<&Path>) -> String {
    let home = home.map(|h| h.to_string_lossy().into_owned());
    let folders: Vec<Value> = view
        .folders
        .iter()
        .enumerate()
        .map(|(i, f)| {
            json!({
                "folder": i + 1,
                "state": f.state,
                "files_done": f.files_done,
                "files_total": f.files_total,
                "files_skipped": f.files_skipped,
            })
        })
        .collect();
    let v = json!({
        "app": {
            "version": inp.app_version,
            "os": inp.os,
            "start_with_windows": inp.autostart,
        },
        "status": {
            "overall": view.overall,
            "problem": view.problem.as_ref().map(|p| &p.details),
            "reconnects": view.reconnects,
            "tier": view.tier,
            "catching_up": view.catching_up,
            "waiting_for_fullscreen": view.waiting_for_fullscreen,
            "provider_health": view.provider_health,
            // M5: which settings the organisation locks (settings paths only, never values or
            // folder roots) and whether the service waits for a missing index (R34; no path).
            "managed_settings": view.policy.managed.iter().map(|m| &m.path).collect::<Vec<_>>(),
            "index_missing": view.index_missing.is_some(),
            "folders": folders,
        },
        "service": {
            "pid": inp.daemon_pid,
            "protocol": inp.daemon_proto,
            "diagnostics": inp.daemon,
        },
        "claude_desktop": {
            "state": inp.claude,
            "installs": inp.claude_variants,
        },
    });
    let v = redact(&v, home.as_deref());
    format!(
        "Selene Retrieval diagnostics\n{}\n",
        serde_json::to_string_pretty(&v).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::{FolderState, FolderView, Overall};

    #[test]
    fn no_secrets_names_paths_or_queries() {
        let view = AppView {
            overall: Overall::Synced,
            summary: "Up to date".into(),
            folders: vec![FolderView {
                id: "01J".into(),
                name: "Tax returns 2025".into(),
                path: Some(r"C:\Users\alice\Documents\Tax returns 2025".into()),
                state: FolderState::UpToDate,
                line: "Up to date".into(),
                files_done: 3,
                files_total: 3,
                files_skipped: 0,
                eta_s: None,
                note: None,
                group_id: None,
                was_files_only: false,
                online_only_skipped: 0,
                cloud_note: None,
            }],
            ..AppView::default()
        };
        let daemon = json!({
            "data_dir": r"C:\Users\Alice\AppData\Local\SeleneRetrieval",
            "api_key": "sk-live-123",
            "nested": {"Authorization": "Bearer abc", "last_query": "my bank password", "ok": 1},
            "providers": [{"OPENAI_TOKEN": "t"}],
            "extract_error": r"failed on C:\Users\alice\secret-plans.docx",
        });
        let inp = Inputs {
            app_version: "0.1.0",
            os: "Windows 11".into(),
            daemon_pid: Some(42),
            daemon_proto: Some(1),
            daemon: Some(daemon),
            claude: Some(ClaudeState::Connected),
            claude_variants: vec!["msix"],
            autostart: Some(true),
        };
        let t = build(&inp, &view, Some(Path::new(r"C:\Users\alice")));
        for leak in [
            "sk-live-123",
            "Bearer abc",
            "my bank password",
            "Tax returns",
            r"C:\Users\alice",
            r"C:\Users\Alice",
        ] {
            assert!(!t.contains(leak), "leaked {leak:?}:\n{t}");
        }
        assert!(
            t.contains(r"%USERPROFILE%\\AppData\\Local\\SeleneRetrieval"),
            "{t}"
        );
        assert!(t.contains("\"ok\": 1"));
        assert!(t.contains("\"pid\": 42"));
        assert!(t.contains("up_to_date"));
    }
}
