//! Codex CLI specifics (M5-C, Amendment 10.3): the `exec` command line, `login status` and the
//! `--json` events. Verified against `codex --help` / `codex exec --help` of codex-cli 0.160.0.
//!
//! ```text
//! codex [-c <override>]... exec --json --ephemeral --skip-git-repo-check --ignore-user-config
//!       --ignore-rules --sandbox read-only --color never -C <empty temp folder>
//!       [-m <model>] [-c model_reasoning_effort=<e>] -        (prompt on stdin)
//! ```
//!
//! * The goal: the model can't run a command or read a file. `shell_tool` off removes the shell
//!   tool whatever `unified_exec` says (in 0.160 `unified_exec` can't be switched off by config and
//!   only picks the shell's flavour); [`TOOL_FEATURES`] switch off everything else tool-like. The
//!   features are set with `-c features.<name>=false`, not `--disable <name>`: `--disable` fails
//!   hard on a name a newer version dropped, `-c` doesn't. Before the first call the effective
//!   state is read back with `codex features list` under the same overrides and the call is
//!   refused unless `shell_tool` is off ([`tools_locked`], fail closed).
//! * `--sandbox read-only` and never an approval or sandbox bypass flag.
//! * `--ignore-user-config` (help: "auth still uses CODEX_HOME"), `--ignore-rules`, no MCP
//!   servers (`-c mcp_servers={}`), no web search, no project docs, `--ephemeral` (no session
//!   files). If a run with `--ignore-user-config` is refused as not signed in while
//!   `codex login status` says signed in (a login kept where only the user's config points), it is
//!   retried once with the user's config but every override above still applied (they win over
//!   `config.toml`).
//! * `codex login status` has no JSON form in 0.160; it prints "Logged in using ChatGPT" (on
//!   stderr) or "Not logged in".

use serde_json::Value;
use sr_common::error::Error;

use super::status::AuthInfo;
use super::{CliKind, classify};

/// Tool-like features switched off in every run (`-c features.<name>=false`). Unknown names
/// are ignored by Codex, so the list may name features newer or older versions have.
pub const TOOL_FEATURES: &[&str] = &[
    "shell_tool",
    "unified_exec",
    "shell_snapshot",
    "apps",
    "plugins",
    "remote_plugin",
    "hooks",
    "browser_use",
    "browser_use_external",
    "in_app_browser",
    "computer_use",
    "image_generation",
    "view_image",
    "multi_agent",
    "multi_agent_v2",
    "code_mode",
    "code_mode_host",
    "memories",
    "skill_mcp_dependency_install",
    "skill_search",
    "tool_suggest",
    "workspace_dependencies",
    "web_search_request",
    "web_search_cached",
    "standalone_web_search",
    "request_permissions_tool",
    "sleep_tool",
    "goals",
];

/// Features that must read back as off before any call (fail closed).
pub const MUST_BE_OFF: &[&str] = &["shell_tool"];

/// The global `-c` overrides (before the subcommand).
pub fn overrides() -> Vec<String> {
    let mut v = Vec::new();
    let mut c = |s: String| {
        v.push("-c".to_string());
        v.push(s);
    };
    for f in TOOL_FEATURES {
        c(format!("features.{f}=false"));
    }
    c("mcp_servers={}".into());
    c("include_apply_patch_tool=false".into());
    c("include_view_image_tool=false".into());
    c("tools.web_search=false".into());
    c("web_search=disabled".into());
    c("project_doc_max_bytes=0".into());
    c("hide_agent_reasoning=true".into());
    v
}

/// Reasoning efforts Selene's Codex ids end with (`-none` … `-ultra`, `lib/auth/codex-models.ts`;
/// `minimal` from older ids). Passed as `-c model_reasoning_effort=<e>`.
pub const EFFORTS: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// Splits Selene's `gpt-5.4-low` / `gpt-6-astra-max` style ids into the model and a reasoning
/// effort, using the provider table's Codex list (see [`split_model_in`]).
pub fn split_model(model: &str) -> (String, Option<&'static str>) {
    let listed: Vec<&str> = crate::registry::manifest()
        .get(CliKind::Codex.provider().id)
        .map(|p| p.models.iter().map(|m| m.id.as_str()).collect())
        .unwrap_or_default();
    split_model_in(model, &listed)
}

/// [`split_model`] against `listed`: an id that has its own effort variants listed
/// (`gpt-5.1-codex-max` next to `gpt-5.1-codex-max-low`) is a model name and is never split;
/// otherwise a trailing `-<effort>` is split off. The xtask (`sync-providers`) keeps only Codex
/// ids this rule maps to the model Selene's `MODEL_MAP` names.
pub fn split_model_in(model: &str, listed: &[&str]) -> (String, Option<&'static str>) {
    let has_variants = listed.iter().any(|l| {
        l.strip_prefix(model)
            .and_then(|r| r.strip_prefix('-'))
            .is_some_and(|e| EFFORTS.contains(&e))
    });
    if !has_variants {
        for e in EFFORTS {
            if let Some(base) = model.strip_suffix(&format!("-{e}"))
                && !base.is_empty()
            {
                return (base.to_string(), Some(e));
            }
        }
    }
    (model.to_string(), None)
}

/// The `exec` command line (without the executable). `user_config`: see the module docs.
pub fn exec_args(model: &str, cwd: &str, user_config: bool) -> Vec<String> {
    let mut a = overrides();
    a.extend(
        [
            "exec",
            "--json",
            "--ephemeral",
            "--skip-git-repo-check",
            "--ignore-rules",
            "--sandbox",
            "read-only",
            "--color",
            "never",
            "-C",
            cwd,
        ]
        .iter()
        .map(|s| (*s).to_string()),
    );
    if !user_config {
        a.push("--ignore-user-config".into());
    }
    let (m, effort) = split_model(model);
    if !m.is_empty() {
        a.push("-m".into());
        a.push(m);
    }
    if let Some(e) = effort {
        a.push("-c".into());
        a.push(format!("model_reasoning_effort={e}"));
    }
    a.push("-".into());
    a
}

/// `codex <overrides> features list` (the read-back of [`tools_locked`]).
pub fn features_args() -> Vec<String> {
    let mut a = overrides();
    a.push("features".into());
    a.push("list".into());
    a
}

/// Whether every [`MUST_BE_OFF`] feature reads back as present and `false` (a missing name means
/// an unknown version: not locked). Lines look like `shell_tool    stable    false`.
pub fn tools_locked(features_list: &str) -> bool {
    let state = |name: &str| {
        features_list.lines().find_map(|l| {
            let mut it = l.split_whitespace();
            (it.next() == Some(name)).then(|| it.last().map(str::to_string))?
        })
    };
    MUST_BE_OFF
        .iter()
        .all(|f| state(f).as_deref() == Some("false"))
}

/// `codex login status` → signed in / plan. `None` when the text says neither.
pub fn parse_login_status(stdout: &str, stderr: &str, success: bool) -> Option<AuthInfo> {
    let text = format!("{stdout}\n{stderr}");
    let lower = text.to_ascii_lowercase();
    if lower.contains("not logged in") {
        return Some(AuthInfo::default());
    }
    if lower.contains("logged in") && success {
        let plan = if lower.contains("chatgpt") {
            Some("ChatGPT".to_string())
        } else if lower.contains("api key") {
            Some("API key".to_string())
        } else {
            None
        };
        return Some(AuthInfo {
            logged_in: true,
            email: None,
            plan,
        });
    }
    None
}

/// One parsed `--json` line.
#[derive(Debug)]
pub enum Event {
    /// A whole agent message (Codex `exec --json` has no text deltas).
    Message(String),
    /// A tool item (command, file change, MCP call, web search, …): never expected; stopped.
    ToolUse,
    Done {
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    },
    /// `turn.failed`: the run is over.
    Fail(Error),
    /// An `error` event or item: may be a passing notice (a reconnect); kept, and used only if
    /// the run then ends without an answer.
    Warn(Error),
    Other,
}

fn usage(v: &Value, k: &str) -> Option<u32> {
    v.get("usage")
        .and_then(|u| u.get(k))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// Parses one `codex exec --json` line.
pub fn parse_line(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Event::Other;
    };
    let message = |v: &Value| {
        v.get("message")
            .and_then(Value::as_str)
            .or_else(|| {
                v.get("error")
                    .and_then(|e| e.get("message").and_then(Value::as_str).or(e.as_str()))
            })
            .unwrap_or("error")
            .to_string()
    };
    match v.get("type").and_then(Value::as_str) {
        Some("item.started" | "item.updated" | "item.completed") => {
            let item = v.get("item").unwrap_or(&Value::Null);
            let kind = item
                .get("type")
                .or_else(|| item.get("item_type"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            match kind {
                "agent_message" | "assistant_message" => {
                    if v.get("type").and_then(Value::as_str) == Some("item.completed") {
                        Event::Message(
                            item.get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        )
                    } else {
                        Event::Other
                    }
                }
                "reasoning" | "todo_list" => Event::Other,
                "error" => Event::Warn(classify(CliKind::Codex, &message(item))),
                // command_execution, file_change, mcp_tool_call, web_search, …
                _ => Event::ToolUse,
            }
        }
        Some("turn.completed") => Event::Done {
            input_tokens: usage(&v, "input_tokens"),
            output_tokens: usage(&v, "output_tokens"),
        },
        Some("turn.failed") => Event::Fail(classify(CliKind::Codex, &message(&v))),
        Some("error") => Event::Warn(classify(CliKind::Codex, &message(&v))),
        _ => Event::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sr_common::error::ErrorCode;

    #[test]
    fn exec_args_lock_the_tools() {
        let a = exec_args("gpt-5.4-low", r"C:\Temp\sr-cli-1", false);
        let i = a.iter().position(|x| x == "exec").unwrap();
        // Global overrides come before the subcommand.
        assert!(a[..i].iter().any(|x| x == "features.shell_tool=false"));
        assert!(a[..i].iter().any(|x| x == "mcp_servers={}"));
        for f in [
            "--json",
            "--ephemeral",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
        ] {
            assert!(a[i..].iter().any(|x| x == f), "{f}");
        }
        let after = |flag: &str| a.iter().position(|x| x == flag).map(|j| a[j + 1].clone());
        assert_eq!(after("--sandbox").as_deref(), Some("read-only"));
        assert_eq!(after("-C").as_deref(), Some(r"C:\Temp\sr-cli-1"));
        assert_eq!(after("-m").as_deref(), Some("gpt-5.4"));
        assert!(a.iter().any(|x| x == "model_reasoning_effort=low"));
        assert_eq!(a.last().map(String::as_str), Some("-"), "prompt from stdin");
        for bad in [
            "--dangerously-bypass-approvals-and-sandbox",
            "--approve-for-me",
            "--dangerously-bypass-hook-trust",
            "--add-dir",
            "--search",
            "danger-full-access",
            "workspace-write",
        ] {
            assert!(!a.iter().any(|x| x == bad), "{bad}");
        }
        // No quotes / `%` reach cmd.exe through the npm shim.
        assert!(a.iter().all(|x| !x.contains(['"', '%', '\n'])));
        let u = exec_args("gpt-5.4", "C:\\T", true);
        assert!(!u.iter().any(|x| x == "--ignore-user-config"));
        assert!(!u.iter().any(|x| x.starts_with("model_reasoning_effort")));
    }

    #[test]
    fn model_suffixes() {
        assert_eq!(split_model("gpt-5.4-low"), ("gpt-5.4".into(), Some("low")));
        assert_eq!(split_model("gpt-5.4"), ("gpt-5.4".into(), None));
        assert_eq!(split_model("-high"), ("-high".into(), None));
        assert_eq!(
            split_model("gpt-6-astra-max"),
            ("gpt-6-astra".into(), Some("max"))
        );
        assert_eq!(
            split_model("gpt-6-astra-ultra"),
            ("gpt-6-astra".into(), Some("ultra"))
        );
        assert_eq!(
            split_model("gpt-5.2-none"),
            ("gpt-5.2".into(), Some("none"))
        );
        assert_eq!(
            split_model("gpt-5.1-codex-mini-medium"),
            ("gpt-5.1-codex-mini".into(), Some("medium"))
        );
        assert_eq!(split_model("o4-minimal"), ("o4".into(), Some("minimal")));
        // A model whose name ends like an effort stays whole when its own variants are listed.
        assert_eq!(
            split_model("gpt-5.1-codex-max"),
            ("gpt-5.1-codex-max".into(), None)
        );
        assert_eq!(
            split_model("gpt-5.1-codex-max-xhigh"),
            ("gpt-5.1-codex-max".into(), Some("xhigh"))
        );
        assert_eq!(
            split_model_in("foo-max", &["foo-max", "foo-max-low"]),
            ("foo-max".into(), None)
        );
        assert_eq!(split_model_in("foo-max", &[]), ("foo".into(), Some("max")));
    }

    /// Amendment 11: every Codex id the picker offers runs as a listed model (+ an effort).
    #[test]
    fn every_listed_codex_id_splits_to_a_listed_model() {
        let row = crate::registry::manifest().get("codex");
        let ids: Vec<&str> = row
            .map(|p| p.models.iter().map(|m| m.id.as_str()).collect())
            .unwrap_or_default();
        assert!(ids.len() > 2, "Codex model list");
        for id in &ids {
            let (base, effort) = split_model(id);
            assert!(ids.contains(&base.as_str()), "{id} -> {base}");
            assert_eq!(effort.is_some(), base != *id, "{id}");
            let a = exec_args(id, "C:\\T", false);
            let after = |flag: &str| a.iter().position(|x| x == flag).map(|j| a[j + 1].clone());
            assert_eq!(after("-m").as_deref(), Some(base.as_str()), "{id}");
            assert_eq!(
                a.iter().any(|x| x.starts_with("model_reasoning_effort=")),
                effort.is_some(),
                "{id}"
            );
        }
    }

    #[test]
    fn feature_read_back() {
        let ok = "apps                stable   false\nshell_tool          stable   false\nunified_exec        stable   true\n";
        assert!(tools_locked(ok));
        let on = "shell_tool          stable   true\n";
        assert!(!tools_locked(on));
        assert!(!tools_locked("apps stable false\n"), "missing = not locked");
    }

    #[test]
    fn login_status() {
        let a = parse_login_status("", "Logged in using ChatGPT\n", true).unwrap();
        assert!(a.logged_in);
        assert_eq!(a.plan.as_deref(), Some("ChatGPT"));
        assert!(
            !parse_login_status("Not logged in\n", "", false)
                .unwrap()
                .logged_in
        );
        assert_eq!(parse_login_status("", "", false), None);
    }

    #[test]
    fn json_lines() {
        assert!(matches!(
            parse_line(r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"Hi"}}"#),
            Event::Message(t) if t == "Hi"
        ));
        assert!(matches!(
            parse_line(
                r#"{"type":"item.started","item":{"id":"item_2","type":"command_execution","command":"dir"}}"#
            ),
            Event::ToolUse
        ));
        assert!(matches!(
            parse_line(
                r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":2}}"#
            ),
            Event::Done {
                input_tokens: Some(10),
                output_tokens: Some(2)
            }
        ));
        match parse_line(
            r#"{"type":"turn.failed","error":{"message":"unexpected status 401 Unauthorized"}}"#,
        ) {
            Event::Fail(e) => assert_eq!(e.code, ErrorCode::ProviderAuth),
            other => panic!("{other:?}"),
        }
        match parse_line(
            r#"{"type":"error","message":"You've hit your usage limit. Try again later."}"#,
        ) {
            Event::Warn(e) => assert_eq!(e.code, ErrorCode::RateLimited),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_line(r#"{"type":"thread.started","thread_id":"t"}"#),
            Event::Other
        ));
    }
}
