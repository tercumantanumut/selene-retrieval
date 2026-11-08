//! Account sign-in through the user's own CLI (M5-C, Amendments 10.2 / 10.3): "Claude (sign in)"
//! runs the local Claude Code CLI (`claude -p`), "Codex (sign in)" the local Codex CLI
//! (`codex exec`). The CLI owns the login (as Selene's Agent SDK backend does); **Selene Retrieval
//! never reads, holds or stores a token** and never touches `~/.claude` or `~/.codex` files.
//!
//! ```text
//! discover::find(kind)            absolute path only: known install folders, then PATH entries
//!                                 that are absolute local folders (never the current directory)
//! status::check(kind, exe)        `claude auth status --json` / `codex login status` (+ --version),
//!                                 15 s, no window, empty temp working folder
//! status::start_login(kind, exe)  the CLI's own sign-in (`claude auth login` / `codex login`) in
//!                                 its own console window; it opens the browser itself
//! CliLlm (LlmProvider)            one CLI run per chat turn: prompt on stdin, no tools, no MCP,
//!                                 no user settings / hooks, no session files; consent + audit
//!                                 through `Egress`; cancel / timeout kill the whole process tree
//! ```
//!
//! **Why no tools.** Indexed documents are untrusted: a prompt injection inside a file must never
//! reach a shell, an editor, a web fetch or an MCP server. Claude runs with `--tools ""`,
//! `--strict-mcp-config` (and no `--mcp-config`), `--setting-sources ""` (no user / project /
//! local settings, so no hooks or plugins) and our own `--system-prompt`. Codex runs with
//! `--sandbox read-only`, `--ignore-user-config`, `--ignore-rules`, `--ephemeral`, every
//! tool-like feature off (`-c features.<name>=false`; `shell_tool` off is verified before the
//! first call, fail closed) and no MCP servers. Both run in an empty temp folder (no CLAUDE.md /
//! AGENTS.md), and a run that still reports a tool call is stopped at once.

pub mod claude;
pub mod codex;
pub mod discover;
mod job;
mod llm;
pub mod process;
pub mod prompt;
pub mod status;

use sr_common::api::{CLI_SIGNIN_PROVIDERS, CliSigninProvider};
use sr_common::error::{Error, ErrorCode};

pub use discover::{CliPath, SearchEnv};
pub use llm::CliLlm;
pub use status::{check as check_status, start_login};

/// Which CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CliKind {
    /// Claude Code (`claude`), provider `claudecode`.
    Claude,
    /// Codex (`codex`), provider `codex`.
    Codex,
}

impl CliKind {
    pub fn from_provider(id: &str) -> Option<Self> {
        match id {
            sr_common::api::CLAUDE_SIGNIN_PROVIDER => Some(Self::Claude),
            sr_common::api::CODEX_SIGNIN_PROVIDER => Some(Self::Codex),
            _ => None,
        }
    }

    /// The provider row (id, label, tool name, install page).
    pub fn provider(self) -> &'static CliSigninProvider {
        let id = match self {
            Self::Claude => sr_common::api::CLAUDE_SIGNIN_PROVIDER,
            Self::Codex => sr_common::api::CODEX_SIGNIN_PROVIDER,
        };
        // Both ids are in the table (a unit test pins it); the first row is a safe fallback.
        CLI_SIGNIN_PROVIDERS
            .iter()
            .find(|p| p.id == id)
            .unwrap_or(&CLI_SIGNIN_PROVIDERS[0])
    }

    /// The executable's file stem (`claude`, `codex`).
    pub fn stem(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// The host requests end up at, for the egress audit row (the CLI talks to it, not us).
    pub fn audit_host(self) -> &'static str {
        match self {
            Self::Claude => "api.anthropic.com",
            Self::Codex => "chatgpt.com",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        self.provider().label
    }

    /// "{tool} isn't installed on this computer" (rendered by `sr_common::error::messages`).
    pub fn not_installed(self) -> Error {
        Error::new(ErrorCode::ProviderNotConfigured)
            .with_provider(self.label())
            .with_detail(format!("{} was not found on this computer", self.stem()))
    }

    /// "{tool} isn't signed in".
    pub fn not_signed_in(self, detail: impl Into<String>) -> Error {
        Error::new(ErrorCode::ProviderAuth)
            .with_provider(self.label())
            .with_detail(detail.into())
    }
}

/// A plain, short detail line from CLI output: the first non-empty line, words with `@`
/// (an account) removed, at most 200 characters. Shown only under "Details", never logged whole.
pub(crate) fn detail_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    let mut d: String = line
        .split_whitespace()
        .map(|w| if w.contains('@') { "[account]" } else { w })
        .collect::<Vec<_>>()
        .join(" ");
    if d.chars().count() > 200 {
        d = d.chars().take(200).collect::<String>() + "…";
    }
    d
}

/// Seconds until a Claude usage-limit reset (`…limit reached|<unix seconds>`), capped at 10 min
/// (the limiter cools down for that long; a later retry asks the CLI again).
fn reset_secs(text: &str) -> Option<u32> {
    let (_, tail) = text.rsplit_once('|')?;
    let at: u64 = tail
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(u32::try_from(at.saturating_sub(now).clamp(30, 600)).unwrap_or(600))
}

/// Maps a CLI failure text to the shared error codes (plain text and the one next action come
/// from `sr_common::error::messages`).
pub(crate) fn classify(kind: CliKind, text: &str) -> Error {
    let t = text.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| t.contains(w));
    let code = if any(&[
        "not logged in",
        "please run /login",
        "run /login",
        "invalid api key",
        "authentication_failed",
        "authentication failed",
        "authentication_error",
        "unauthorized",
        " 401",
        "oauth token has expired",
        "token has expired",
        "token expired",
        "refresh token",
        "not signed in",
        "login required",
        "please log in",
        "please login",
        "sign in again",
        "logged out",
    ]) {
        ErrorCode::ProviderAuth
    } else if any(&[
        "credit balance",
        "billing",
        "payment required",
        "out of credit",
    ]) {
        ErrorCode::ProviderQuota
    } else if any(&[
        "usage limit",
        "rate limit",
        "rate_limit",
        "too many requests",
        " 429",
        "limit reached",
        "quota",
    ]) {
        ErrorCode::RateLimited
    } else if any(&[
        "prompt is too long",
        "context window",
        "context length",
        "too many tokens",
        "invalid model",
        "model not found",
        "unknown model",
        "does not exist",
        "not supported",
        "not available for",
        "issue with the selected model",
        "invalid_request",
    ]) {
        ErrorCode::ProviderRequest
    } else {
        ErrorCode::ProviderUnavailable
    };
    let mut e = Error::new(code)
        .with_provider(kind.label())
        .with_detail(detail_line(text));
    if code == ErrorCode::RateLimited
        && let Some(s) = reset_secs(text)
    {
        e = e.with_retry_after(s);
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_kinds_map_to_their_rows() {
        for k in [CliKind::Claude, CliKind::Codex] {
            let p = k.provider();
            assert_eq!(CliKind::from_provider(p.id), Some(k));
            assert!(p.install_url.starts_with("https://"));
        }
        assert_eq!(CliKind::Claude.provider().tool, "Claude Code");
        assert_eq!(CliKind::Codex.provider().label, "Codex (sign in)");
        assert_eq!(CliKind::from_provider("anthropic"), None);
        assert_eq!(
            CliKind::Codex.not_installed().code,
            ErrorCode::ProviderNotConfigured
        );
        assert_eq!(
            CliKind::Claude.not_signed_in("x").ctx.provider.as_deref(),
            Some("Claude (sign in)")
        );
    }

    #[test]
    fn failures_map_to_plain_codes() {
        let c = |t: &str| classify(CliKind::Claude, t).code;
        assert_eq!(
            c("Invalid API key · Please run /login"),
            ErrorCode::ProviderAuth
        );
        assert_eq!(
            c("Not logged in · Please run /login"),
            ErrorCode::ProviderAuth
        );
        assert_eq!(c("OAuth token has expired"), ErrorCode::ProviderAuth);
        assert_eq!(
            c("Claude AI usage limit reached|4102444800"),
            ErrorCode::RateLimited
        );
        assert_eq!(
            c("API Error: 529 Overloaded"),
            ErrorCode::ProviderUnavailable
        );
        assert_eq!(c("Credit balance is too low"), ErrorCode::ProviderQuota);
        assert_eq!(c("Prompt is too long"), ErrorCode::ProviderRequest);
        assert_eq!(
            classify(
                CliKind::Codex,
                "stream error: unexpected status 401 Unauthorized"
            )
            .code,
            ErrorCode::ProviderAuth
        );
        let e = classify(CliKind::Claude, "Claude AI usage limit reached|4102444800");
        assert_eq!(e.retry_after.map(|d| d.as_secs()), Some(600));
        // The account never reaches the detail.
        let e = classify(CliKind::Claude, "Signed in as me@example.com but: boom");
        assert!(!format!("{e:?}").contains("me@example.com"));
        assert_eq!(e.ctx.provider.as_deref(), Some("Claude (sign in)"));
    }
}
