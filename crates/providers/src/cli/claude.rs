//! Claude Code CLI specifics (M5-C, Amendment 10.2): the chat command line, `auth status --json`
//! and the `stream-json` events.
//!
//! Chat command (verified against `claude --help` of 2.1.303):
//!
//! ```text
//! claude -p --output-format stream-json --verbose --include-partial-messages
//!        --model <model> --tools "" --strict-mcp-config --setting-sources ""
//!        --disable-slash-commands --no-session-persistence --system-prompt <ours>
//!        (prompt on stdin, cwd = empty temp folder)
//! ```
//!
//! * `--tools ""` — no built-in tool at all (help: `Use "" to disable all tools`): no Bash,
//!   PowerShell, Read, Edit, Write, WebFetch, WebSearch, Task, ….
//! * `--strict-mcp-config` with no `--mcp-config` — no MCP server from any config.
//! * `--setting-sources ""` — no user, project or local settings file is loaded, so the user's
//!   hooks, plugins, permissions, `env` and status line never run in our calls (the Agent SDK's
//!   default for "no filesystem settings"). Managed (policy) settings still apply, as they should.
//!   The login is not a setting: the CLI still finds it.
//! * Not `--bare`: it would skip the OAuth login entirely ("OAuth and keychain are never read").
//! * Not `--safe-mode` / `--restricted`: newer flags than some 2.1.x installs know; everything
//!   they would add is already off through the flags above.
//! * `--disable-slash-commands` — no skills; `--no-session-persistence` — no transcript on disk.
//! * `--system-prompt` replaces the coding-agent prompt with ours (one plain line).

use serde_json::Value;
use sr_common::error::{Error, ErrorCode};

use super::status::AuthInfo;
use super::{CliKind, classify};

/// Our system prompt: one line of plain words (it travels as a command-line argument, also
/// through `cmd.exe` for the npm shim, so no quotes, `%`, `^`, `&`, `|`, `<`, `>` or newlines).
pub const SYSTEM_PROMPT: &str = "You are the writing model inside Selene Retrieval, a document search app on this computer. You have no tools and no files: everything you need is in the input. Follow the instructions in the input and reply with the requested output only.";

/// The chat command line (without the executable).
pub fn chat_args(model: &str) -> Vec<String> {
    [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--model",
        model,
        "--tools",
        "",
        "--strict-mcp-config",
        "--setting-sources",
        "",
        "--disable-slash-commands",
        "--no-session-persistence",
        "--system-prompt",
        SYSTEM_PROMPT,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

fn valid_email(s: &str) -> bool {
    s.contains('@') && s.len() <= 320 && !s.chars().any(char::is_control)
}

fn plan_name(sub: &str) -> String {
    match sub.to_ascii_lowercase().as_str() {
        "max" => "Max".into(),
        "pro" => "Pro".into(),
        "team" => "Team".into(),
        "enterprise" => "Enterprise".into(),
        "free" => "Free".into(),
        other => {
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().chain(c).take(40).collect(),
                None => String::new(),
            }
        }
    }
}

/// `claude auth status --json` → signed in / email / plan. `None` = not that JSON.
pub fn parse_auth_status(stdout: &str) -> Option<AuthInfo> {
    let start = stdout.find('{')?;
    let end = stdout.rfind('}')?;
    let v: Value = serde_json::from_str(stdout.get(start..=end)?).ok()?;
    let logged_in = v.get("loggedIn").and_then(Value::as_bool)?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim);
    let plan = match (s("subscriptionType"), s("authMethod"), s("apiProvider")) {
        (Some(sub), _, _) if !sub.is_empty() => Some(plan_name(sub)),
        (_, _, Some(p)) if !p.is_empty() && !p.eq_ignore_ascii_case("firstParty") => {
            Some(plan_name(p))
        }
        (_, Some(m), _) if !m.is_empty() && !m.eq_ignore_ascii_case("claude.ai") => {
            Some("API key".into())
        }
        _ => None,
    };
    Some(AuthInfo {
        logged_in,
        email: if logged_in {
            s("email").filter(|e| valid_email(e)).map(str::to_string)
        } else {
            None
        },
        plan: if logged_in { plan } else { None },
    })
}

/// One parsed `stream-json` line.
#[derive(Debug)]
pub enum Event {
    /// A piece of the answer (`--include-partial-messages`).
    Delta(String),
    /// A whole assistant message (when no partial messages came).
    Message(String),
    /// The model tried to use a tool: never expected (`--tools ""`); the run is stopped.
    ToolUse,
    /// The run finished.
    Done {
        text: Option<String>,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    },
    Fail(Error),
    /// `system` / `user` / anything else.
    Other,
}

fn tokens(v: &Value, k: &str) -> Option<u32> {
    v.get("usage")
        .and_then(|u| u.get(k))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// The SDK's assistant `error` kinds.
fn assistant_error(kind: &str, text: &str) -> Error {
    let code = match kind {
        "authentication_failed" => ErrorCode::ProviderAuth,
        "billing_error" => ErrorCode::ProviderQuota,
        "rate_limit" => ErrorCode::RateLimited,
        "invalid_request" => ErrorCode::ProviderRequest,
        _ => return classify(CliKind::Claude, &format!("{kind}: {text}")),
    };
    let mut e = classify(CliKind::Claude, text);
    e.code = code;
    e
}

fn is_tool_block(b: &Value) -> bool {
    matches!(
        b.get("type").and_then(Value::as_str),
        Some("tool_use" | "server_tool_use" | "mcp_tool_use")
    )
}

/// Parses one `stream-json` line.
pub fn parse_line(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Event::Other;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("stream_event") => {
            let ev = v.get("event").unwrap_or(&Value::Null);
            match ev.get("type").and_then(Value::as_str) {
                Some("content_block_delta") => {
                    let d = ev.get("delta").unwrap_or(&Value::Null);
                    match d.get("type").and_then(Value::as_str) {
                        Some("text_delta") => d
                            .get("text")
                            .and_then(Value::as_str)
                            .map_or(Event::Other, |t| Event::Delta(t.to_string())),
                        _ => Event::Other,
                    }
                }
                Some("content_block_start")
                    if ev.get("content_block").is_some_and(is_tool_block) =>
                {
                    Event::ToolUse
                }
                _ => Event::Other,
            }
        }
        Some("assistant") => {
            let msg = v.get("message").unwrap_or(&Value::Null);
            let blocks = msg
                .get("content")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let text: String = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            if let Some(kind) = v.get("error").and_then(Value::as_str) {
                return Event::Fail(assistant_error(kind, &text));
            }
            if blocks.iter().any(is_tool_block) {
                return Event::ToolUse;
            }
            Event::Message(text)
        }
        Some("result") => {
            let text = v.get("result").and_then(Value::as_str).map(str::to_string);
            if v.get("is_error").and_then(Value::as_bool) == Some(true) {
                let sub = v.get("subtype").and_then(Value::as_str).unwrap_or("error");
                return Event::Fail(classify(CliKind::Claude, text.as_deref().unwrap_or(sub)));
            }
            Event::Done {
                text,
                input_tokens: tokens(&v, "input_tokens"),
                output_tokens: tokens(&v, "output_tokens"),
            }
        }
        _ => Event::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_args_switch_everything_off() {
        let a = chat_args("claude-sonnet-4-6");
        let after = |flag: &str| {
            a.iter()
                .position(|x| x == flag)
                .and_then(|i| a.get(i + 1))
                .cloned()
        };
        assert_eq!(after("--tools").as_deref(), Some(""));
        assert_eq!(after("--setting-sources").as_deref(), Some(""));
        assert_eq!(after("--model").as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(after("--output-format").as_deref(), Some("stream-json"));
        for f in [
            "-p",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--disable-slash-commands",
            "--verbose",
        ] {
            assert!(a.iter().any(|x| x == f), "{f}");
        }
        for f in [
            "--mcp-config",
            "--bare",
            "--dangerously-skip-permissions",
            "--add-dir",
        ] {
            assert!(!a.iter().any(|x| x == f), "{f}");
        }
        assert!(
            !SYSTEM_PROMPT.contains(['"', '%', '^', '&', '|', '<', '>', '\n', '\r']),
            "safe for cmd.exe"
        );
    }

    #[test]
    fn auth_status_json() {
        let a = parse_auth_status(
            r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"me@example.com","orgName":"X","subscriptionType":"max"}"#,
        )
        .unwrap();
        assert!(a.logged_in);
        assert_eq!(a.email.as_deref(), Some("me@example.com"));
        assert_eq!(a.plan.as_deref(), Some("Max"));
        let b = parse_auth_status("{\"loggedIn\": false}\n").unwrap();
        assert_eq!(b, AuthInfo::default());
        assert_eq!(parse_auth_status("Not logged in"), None);
        let c = parse_auth_status(r#"{"loggedIn":true,"authMethod":"console"}"#).unwrap();
        assert_eq!(c.plan.as_deref(), Some("API key"));
        assert_eq!(c.email, None);
    }

    #[test]
    fn stream_lines() {
        assert!(matches!(
            parse_line(r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}}"#),
            Event::Delta(t) if t == "Hel"
        ));
        assert!(matches!(
            parse_line(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#),
            Event::Message(t) if t == "Hello"
        ));
        assert!(matches!(
            parse_line(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#
            ),
            Event::ToolUse
        ));
        assert!(matches!(
            parse_line(
                r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use"}}}"#
            ),
            Event::ToolUse
        ));
        assert!(matches!(
            parse_line(r#"{"type":"result","subtype":"success","is_error":false,"result":"Hello","usage":{"input_tokens":12,"output_tokens":3}}"#),
            Event::Done { text: Some(t), input_tokens: Some(12), output_tokens: Some(3) } if t == "Hello"
        ));
        match parse_line(
            r#"{"type":"assistant","error":"authentication_failed","message":{"content":[{"type":"text","text":"Invalid API key · Please run /login"}]}}"#,
        ) {
            Event::Fail(e) => assert_eq!(e.code, ErrorCode::ProviderAuth),
            other => panic!("{other:?}"),
        }
        match parse_line(
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Claude AI usage limit reached|4102444800"}"#,
        ) {
            Event::Fail(e) => assert_eq!(e.code, ErrorCode::RateLimited),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_line(r#"{"type":"system","subtype":"init"}"#),
            Event::Other
        ));
        assert!(matches!(parse_line("not json"), Event::Other));
    }
}
