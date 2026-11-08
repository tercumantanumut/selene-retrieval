//! Sign-in status and the CLI's own sign-in (M5-C).
//!
//! Status comes **only** from the CLI: `claude auth status --json` (fields `loggedIn`,
//! `authMethod`, `apiProvider`, `email`, `subscriptionType`, …) or `codex login status` (plain
//! text: "Logged in using ChatGPT" / "Not logged in"; this version has no JSON form). Never a
//! file under `~/.claude` or `~/.codex`, never a token.

use std::path::Path;
use std::time::Duration;

use sr_common::Result;
use sr_common::api::{CliSigninState, CliSigninStatus};
use sr_common::error::ErrorCode;

use super::discover::CliPath;
use super::{CliKind, claude, codex, process};

/// Time limit of one status command.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(15);

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_string()).collect()
}

/// `--version` → "2.1.303" / "0.160.0" (the first token that starts with a digit).
pub fn parse_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|t| t.chars().take(40).collect())
}

/// Asks the CLI at `exe` whether it is signed in (and its version). Never fails: a check that
/// couldn't run reports `signed_out` with `error` set.
pub async fn check(kind: CliKind, exe: Option<&CliPath>) -> CliSigninStatus {
    let p = kind.provider();
    let Some(exe) = exe else {
        return CliSigninStatus::new(p, CliSigninState::NotInstalled);
    };
    let version_args = args(&["--version"]);
    let status_args = match kind {
        CliKind::Claude => args(&["auth", "status", "--json"]),
        CliKind::Codex => args(&["login", "status"]),
    };
    let (ver, st) = tokio::join!(
        process::run(exe, &version_args, None, STATUS_TIMEOUT, p.label),
        process::run(exe, &status_args, None, STATUS_TIMEOUT, p.label),
    );
    let mut out = CliSigninStatus::new(p, CliSigninState::SignedOut);
    out.cli_version = ver
        .ok()
        .and_then(|o| parse_version(&o.stdout).or_else(|| parse_version(&o.stderr)));
    match st {
        Ok(o) => {
            let a = match kind {
                CliKind::Claude => claude::parse_auth_status(&o.stdout),
                CliKind::Codex => codex::parse_login_status(&o.stdout, &o.stderr, o.success()),
            };
            match a {
                Some(a) if a.logged_in => {
                    out.state = CliSigninState::SignedIn;
                    out.authenticated = true;
                    out.email = a.email;
                    out.plan = a.plan;
                }
                Some(_) => {}
                // Ran, but said nothing we understand: report it, don't guess.
                None => out.error = Some(ErrorCode::ProviderRequest),
            }
        }
        Err(e) if e.code == ErrorCode::ProviderNotConfigured => {
            // The file vanished between the search and the run.
            out = CliSigninStatus::new(p, CliSigninState::NotInstalled);
        }
        Err(e) => out.error = Some(e.code),
    }
    out
}

/// What a status command said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthInfo {
    pub logged_in: bool,
    pub email: Option<String>,
    pub plan: Option<String>,
}

/// Starts the CLI's own sign-in (`claude auth login` / `codex login`) in its own console window.
/// Both open the browser themselves and finish by their own local callback; the window shows
/// the address too, in case the browser doesn't open. Returns the launcher process, which exits
/// when the sign-in window closes.
pub fn start_login(kind: CliKind, exe: &CliPath, cwd: &Path) -> Result<tokio::process::Child> {
    let words: &[&str] = match kind {
        CliKind::Claude => &["auth", "login"],
        CliKind::Codex => &["login"],
    };
    process::spawn_console(exe, words, cwd, kind.label())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(
            parse_version("2.1.303 (Claude Code)\n").as_deref(),
            Some("2.1.303")
        );
        assert_eq!(
            parse_version("codex-cli 0.160.0\n").as_deref(),
            Some("0.160.0")
        );
        assert_eq!(parse_version("nothing here"), None);
    }

    #[tokio::test]
    async fn no_cli_is_not_installed() {
        let s = check(CliKind::Codex, None).await;
        assert_eq!(s.state, CliSigninState::NotInstalled);
        assert!(!s.cli_found && !s.authenticated);
        assert_eq!(s.install_url, "https://developers.openai.com/codex/cli");
    }
}
