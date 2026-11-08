//! Connected apps (page 29): Claude Desktop, Cursor and VS Code, one card each, on top of the
//! Tauri-free writers in `connect/`. Everything here is plain logic over paths (unit-tested in
//! temp dirs); the Tauri commands in `commands.rs` only forward.
//!
//! Safety rules (same as Claude's writer, R-UX3): only our `selene-retrieval` entry changes, the
//! original is backed up next to it, writes are atomic and verified (restored on mismatch), files
//! that aren't plain JSON objects are never touched, and no file content is ever logged or shown.
#![allow(clippy::result_large_err)] // `UiError` is the Tauri IPC error (see commands.rs).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sr_common::error::{Error, ErrorCode};

use crate::connect::jsonfile::{self, EntryState, Outcome, WriteError};
use crate::connect::{self, ClientRoots, claude, cursor, vscode};
use crate::dto::{ClaudeState, ClientApp, ClientStatus, ClientsView, UiAction, UiError};

/// Everything the Connect buttons need: the user's folders and our bridge.
#[derive(Debug, Clone)]
pub struct ConnectCtx {
    pub roots: ClientRoots,
    /// Absolute path of `sr-bridge.exe` (next to `sr-app.exe`).
    pub bridge_exe: PathBuf,
}

/// Where one app keeps its config (one file per installed variant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    pub servers_key: &'static str,
    pub backup_suffix: &'static str,
}

impl ConnectCtx {
    pub fn claude_roots(&self) -> claude::Roots {
        self.roots.claude()
    }

    /// Our entry in `app`'s format.
    pub fn entry(&self, app: ClientApp) -> Value {
        match app {
            ClientApp::ClaudeDesktop => connect::server_entry(&self.bridge_exe, "claude"),
            ClientApp::Cursor => cursor::entry(&self.bridge_exe),
            ClientApp::VsCode => vscode::entry(&self.bridge_exe),
        }
    }

    /// Setup text in `app`'s format (OQ-44).
    pub fn snippet(&self, app: ClientApp) -> String {
        match app {
            ClientApp::ClaudeDesktop => connect::snippet(&self.bridge_exe, "claude"),
            ClientApp::Cursor => cursor::snippet(&self.bridge_exe),
            ClientApp::VsCode => vscode::snippet(&self.bridge_exe),
        }
    }

    /// "Other app": Claude format (the most common), generic client id.
    pub fn other_snippet(&self) -> String {
        connect::snippet(&self.bridge_exe, "other")
    }

    /// Every config file `app` reads on this computer (empty = not installed).
    pub fn targets(&self, app: ClientApp) -> Vec<Target> {
        match app {
            ClientApp::ClaudeDesktop => claude::detect(&self.claude_roots())
                .into_iter()
                .map(|i| Target {
                    path: i.config_path,
                    servers_key: claude::SERVERS_KEY,
                    backup_suffix: claude::BACKUP_SUFFIX,
                })
                .collect(),
            ClientApp::Cursor => cursor::detect(&self.roots)
                .into_iter()
                .map(|path| Target {
                    path,
                    servers_key: cursor::SERVERS_KEY,
                    backup_suffix: cursor::BACKUP_SUFFIX,
                })
                .collect(),
            ClientApp::VsCode => vscode::detect(&self.roots)
                .into_iter()
                .map(|i| Target {
                    path: i.config_path,
                    servers_key: vscode::SERVERS_KEY,
                    backup_suffix: vscode::BACKUP_SUFFIX,
                })
                .collect(),
        }
    }

    /// Folders to watch for config changes (existing ones only; re-armed after a connect).
    pub fn watch_dirs(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = ClientApp::ALL
            .iter()
            .flat_map(|a| self.targets(*a))
            .filter_map(|t| t.path.parent().map(Path::to_path_buf))
            .filter(|d| d.is_dir())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// DS-5.5 state of `app` across its installed variants.
    pub fn state(&self, app: ClientApp) -> ClaudeState {
        let entry = self.entry(app);
        let states: Vec<EntryState> = self
            .targets(app)
            .iter()
            .map(|t| jsonfile::entry_state(&t.path, t.servers_key, &entry))
            .collect();
        combine(&states)
    }
}

/// One state for several config files (an app with two variants, e.g. MSIX + Squirrel Claude).
pub fn combine(states: &[EntryState]) -> ClaudeState {
    if states.is_empty() {
        ClaudeState::NotInstalled
    } else if states.contains(&EntryState::Unreadable) {
        ClaudeState::CantRead
    } else if states.iter().all(|s| *s == EntryState::Connected) {
        ClaudeState::Connected
    } else if states.contains(&EntryState::Outdated) {
        ClaudeState::NeedsReconnect
    } else {
        ClaudeState::NotConnected
    }
}

/// Plain errors for a failed write, in the app's words (adapted from Claude's [Code] copy).
pub fn write_error(app: ClientApp, e: WriteError) -> UiError {
    let short = app.short();
    match e {
        WriteError::Unreadable => UiError::new(
            &format!("{short}'s settings file couldn't be read"),
            &format!(
                "We didn't change it. Copy the setup text and paste it into {short}'s settings instead."
            ),
            UiAction::CopySnippet,
            "Copy setup text",
            "ClientConfigUnreadable",
        ),
        WriteError::VerifyFailed => UiError::new(
            &format!("Couldn't save {short}'s settings"),
            "Another program changed the file at the same moment. Nothing was lost.",
            UiAction::Retry,
            "Retry",
            "ClientConfigVerifyFailed",
        ),
        WriteError::Io(kind) => UiError::new(
            &format!("Couldn't save {short}'s settings"),
            &format!("Windows didn't allow the change. Quit {short} and try again."),
            UiAction::Retry,
            "Retry",
            &format!("ClientConfigIo · {kind:?}"),
        ),
    }
}

fn not_installed(app: ClientApp) -> UiError {
    UiError::new(
        &format!("{} isn't installed", app.name()),
        &format!(
            "Install {}, then connect it here. For other apps, copy the setup text.",
            app.name()
        ),
        UiAction::CopySnippet,
        "Copy setup text",
        "ClientNotInstalled",
    )
}

fn bridge_missing() -> UiError {
    UiError::from(
        Error::new(ErrorCode::Internal).with_detail("sr-bridge.exe missing next to the app"),
    )
}

/// [Connect {app}]: writes our entry into every installed variant's config. Returns the plain
/// next step.
pub fn connect_app(c: &ConnectCtx, app: ClientApp) -> Result<String, UiError> {
    if !c.bridge_exe.is_file() {
        return Err(bridge_missing());
    }
    let targets = c.targets(app);
    if targets.is_empty() {
        return Err(not_installed(app));
    }
    let entry = c.entry(app);
    let mut wrote = false;
    for t in &targets {
        match jsonfile::connect(&t.path, t.servers_key, t.backup_suffix, &entry) {
            Ok(Outcome::Written) => wrote = true,
            Ok(Outcome::Unchanged) => {}
            Err(e) => {
                tracing::warn!(?app, err = ?e, "connect failed");
                return Err(write_error(app, e));
            }
        }
    }
    tracing::info!(?app, variants = targets.len(), wrote, "app connected");
    let short = app.short();
    Ok(if wrote {
        format!("Connected. If {short} is open, quit it and open it again to finish.")
    } else {
        format!("{short} is already connected.")
    })
}

/// [Disconnect]: removes only our entry from every variant's config.
pub fn disconnect_app(c: &ConnectCtx, app: ClientApp) -> Result<String, UiError> {
    for t in c.targets(app) {
        jsonfile::disconnect(&t.path, t.servers_key, t.backup_suffix)
            .map_err(|e| write_error(app, e))?;
    }
    Ok(format!(
        "Disconnected. Quit {} and open it again to finish.",
        app.short()
    ))
}

/// `.broken-YYYYMMDD-HHMMSS` (UTC) for the kept copy of an unreadable file (OQ-46).
pub fn broken_suffix(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(i64::try_from(days).unwrap_or(0));
    format!(
        ".broken-{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 → (year, month, day) (H. Hinnant's civil-from-days).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// [Repair…] → [Repair] (OQ-46, R-UX28): every unreadable config of `app` is moved aside as
/// `<file>.broken-<date>` and a fresh file with only our entry is written. Readable files are
/// left alone (and get our entry the normal way).
pub fn repair_app(c: &ConnectCtx, app: ClientApp, now: SystemTime) -> Result<String, UiError> {
    if !c.bridge_exe.is_file() {
        return Err(bridge_missing());
    }
    let targets = c.targets(app);
    if targets.is_empty() {
        return Err(not_installed(app));
    }
    let entry = c.entry(app);
    let suffix = broken_suffix(now);
    for t in &targets {
        let r = if jsonfile::entry_state(&t.path, t.servers_key, &entry) == EntryState::Unreadable {
            jsonfile::repair(&t.path, t.servers_key, &suffix, &entry).map(|_| ())
        } else {
            jsonfile::connect(&t.path, t.servers_key, t.backup_suffix, &entry).map(|_| ())
        };
        r.map_err(|e| {
            tracing::warn!(?app, err = ?e, "repair failed");
            write_error(app, e)
        })?;
    }
    tracing::info!(?app, "app settings repaired");
    Ok(format!(
        "Repaired. The old file was kept next to it. If {} is open, quit it and open it again to finish.",
        app.short()
    ))
}

/// Our own writes are ignored by the external-change check for this long (the watcher sees them
/// a moment later).
pub const OWN_WRITE_GRACE: Duration = Duration::from_secs(3);

/// Tracks each app's last seen state so the watcher can tell "Claude removed our entry" (R-UX23)
/// from our own Disconnect.
#[derive(Debug, Default)]
pub struct ChangeTracker {
    last: HashMap<ClientApp, ClaudeState>,
    own_write: HashMap<ClientApp, Instant>,
    changed_externally: HashMap<ClientApp, bool>,
}

impl ChangeTracker {
    /// Call right before our own write to `app`'s config.
    pub fn own_write(&mut self, app: ClientApp, now: Instant) {
        self.own_write.insert(app, now);
        self.changed_externally.insert(app, false);
    }

    /// A fresh state for `app`. Returns `true` when it went from connected to not connected
    /// without our doing (notify once).
    pub fn observe(&mut self, app: ClientApp, state: ClaudeState, now: Instant) -> bool {
        let prev = self.last.insert(app, state);
        let ours = self
            .own_write
            .get(&app)
            .is_some_and(|t| now.saturating_duration_since(*t) < OWN_WRITE_GRACE);
        if state == ClaudeState::Connected {
            self.changed_externally.insert(app, false);
            return false;
        }
        let lost = prev == Some(ClaudeState::Connected) && state == ClaudeState::NotConnected;
        if lost && !ours {
            self.changed_externally.insert(app, true);
            return true;
        }
        false
    }

    pub fn changed_externally(&self, app: ClientApp) -> bool {
        self.changed_externally.get(&app).copied().unwrap_or(false)
    }
}

/// The page 29 cards. `last_used` comes from the service (contract), `None` until known.
pub fn clients_view(
    c: &ConnectCtx,
    tracker: &ChangeTracker,
    last_used: &HashMap<ClientApp, f64>,
) -> ClientsView {
    ClientsView {
        apps: ClientApp::ALL
            .iter()
            .map(|a| ClientStatus {
                app: *a,
                name: a.name().to_owned(),
                state: c.state(*a),
                snippet: c.snippet(*a),
                changed_externally: tracker.changed_externally(*a),
                last_used_ms: last_used.get(a).copied(),
            })
            .collect(),
        other_snippet: c.other_snippet(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn ctx(t: &tempfile::TempDir) -> ConnectCtx {
        let bridge = t.path().join("app").join("sr-bridge.exe");
        fs::create_dir_all(bridge.parent().unwrap()).unwrap();
        fs::write(&bridge, b"MZ").unwrap();
        let roots = ClientRoots {
            roaming: t.path().join("Roaming"),
            local: t.path().join("Local"),
            home: t.path().join("Home"),
        };
        for d in [&roots.roaming, &roots.local, &roots.home] {
            fs::create_dir_all(d).unwrap();
        }
        ConnectCtx {
            roots,
            bridge_exe: bridge,
        }
    }

    #[test]
    fn nothing_installed_offers_setup_text_for_every_app() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(&t);
        let v = clients_view(&c, &ChangeTracker::default(), &HashMap::new());
        assert_eq!(v.apps.len(), 3);
        assert!(v.apps.iter().all(|a| a.state == ClaudeState::NotInstalled));
        assert!(v.other_snippet.contains("\"other\""));
        for app in [ClientApp::Cursor, ClientApp::VsCode] {
            let e = connect_app(&c, app).unwrap_err();
            assert_eq!(e.action, UiAction::CopySnippet);
            assert_eq!(e.title, format!("{} isn't installed", app.name()));
        }
        assert!(!c.roots.home.join(".cursor").exists(), "nothing written");
    }

    #[test]
    fn cursor_and_vscode_connect_and_disconnect() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(&t);
        fs::create_dir_all(c.roots.local.join("Programs").join("cursor")).unwrap();
        fs::create_dir_all(c.roots.roaming.join("Code")).unwrap();
        for app in [ClientApp::Cursor, ClientApp::VsCode] {
            assert_eq!(c.state(app), ClaudeState::NotConnected);
            let r = connect_app(&c, app).unwrap();
            assert!(r.starts_with("Connected."), "{r}");
            assert_eq!(c.state(app), ClaudeState::Connected);
            assert!(
                connect_app(&c, app)
                    .unwrap()
                    .ends_with("is already connected.")
            );
            disconnect_app(&c, app).unwrap();
            assert_eq!(c.state(app), ClaudeState::NotConnected);
        }
        // The VS Code file uses VS Code's own key and entry shape.
        connect_app(&c, ClientApp::VsCode).unwrap();
        let p = c.roots.roaming.join("Code").join("User").join("mcp.json");
        let v: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["servers"]["selene-retrieval"]["type"], "stdio");
        assert!(c.watch_dirs().contains(&p.parent().unwrap().to_path_buf()));
    }

    #[test]
    fn repair_keeps_the_broken_file() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(&t);
        fs::create_dir_all(c.roots.local.join("AnthropicClaude")).unwrap();
        let p = c.roots.roaming.join("Claude").join(claude::CONFIG_FILE);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "{ broken").unwrap();
        assert_eq!(c.state(ClientApp::ClaudeDesktop), ClaudeState::CantRead);
        let now = UNIX_EPOCH + Duration::from_secs(1_792_368_000); // 2026-10-19 00:00:00 UTC
        repair_app(&c, ClientApp::ClaudeDesktop, now).unwrap();
        assert_eq!(c.state(ClientApp::ClaudeDesktop), ClaudeState::Connected);
        let kept = jsonfile::sibling(&p, ".broken-20261019-000000");
        assert_eq!(fs::read_to_string(kept).unwrap(), "{ broken");
    }

    #[test]
    fn broken_suffix_is_a_plain_utc_date() {
        assert_eq!(broken_suffix(UNIX_EPOCH), ".broken-19700101-000000");
        let t = UNIX_EPOCH + Duration::from_secs(951_782_400 + 3_661); // 2000-02-29 01:01:01
        assert_eq!(broken_suffix(t), ".broken-20000229-010101");
    }

    #[test]
    fn only_an_outside_removal_counts_as_disconnected_by_the_app() {
        let mut tr = ChangeTracker::default();
        let t0 = Instant::now();
        let app = ClientApp::ClaudeDesktop;
        assert!(!tr.observe(app, ClaudeState::Connected, t0));
        // Claude rewrote its file without our entry.
        assert!(tr.observe(app, ClaudeState::NotConnected, t0));
        assert!(tr.changed_externally(app));
        assert!(!tr.observe(app, ClaudeState::NotConnected, t0), "once");
        // Reconnect clears it.
        assert!(!tr.observe(app, ClaudeState::Connected, t0));
        assert!(!tr.changed_externally(app));
        // Our own Disconnect never notifies.
        tr.own_write(app, t0);
        assert!(!tr.observe(
            app,
            ClaudeState::NotConnected,
            t0 + Duration::from_millis(200)
        ));
        assert!(!tr.changed_externally(app));
        // ...but a removal long after our last write does.
        tr.observe(app, ClaudeState::Connected, t0);
        assert!(tr.observe(
            app,
            ClaudeState::NotConnected,
            t0 + OWN_WRITE_GRACE + Duration::from_secs(1)
        ));
    }
}
