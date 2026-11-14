//! Claude Desktop: install detection (Squirrel and MSIX) and the `mcpServers` merge writer.
//!
//! Locations (m0-d §4):
//! * Squirrel (per-user exe installer): app in `%LOCALAPPDATA%\AnthropicClaude`, config in
//!   `%APPDATA%\Claude\claude_desktop_config.json`.
//! * MSIX: the packaged app's `%APPDATA%` writes are virtualised to
//!   `%LOCALAPPDATA%\Packages\<PackageFamilyName>\LocalCache\Roaming\Claude\`. The family name
//!   (`Claude_<13-char publisher id>`) is **resolved at runtime** by listing
//!   `%LOCALAPPDATA%\Packages`; Windows deletes a package's data folder when the package is removed
//!   for the user, so the folder's presence means "installed for this user". Never hard-coded.
//!
//! If both variants are installed, both configs are written (the user may start either).
//!
//! macOS: `~/Library/Application Support/Claude/claude_desktop_config.json` (`roaming` is
//! Application Support there), counted as installed once Claude created that folder on its first
//! start. MSIX is Windows-only and never looked for elsewhere.
//!
//! Writer contract (task 3): parse JSON, keep every unknown field and every other server entry,
//! back up the original next to it, write atomically, verify by re-reading (restore the original
//! if the check fails), and never print or log any part of the file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::jsonfile;
pub use super::jsonfile::{EntryState, Outcome, WriteError};

pub const CONFIG_FILE: &str = "claude_desktop_config.json";
/// Backup of the file as it was before our last change (same folder, same permissions).
pub const BACKUP_SUFFIX: &str = ".selene-retrieval.bak";
/// Claude's server map.
pub const SERVERS_KEY: &str = "mcpServers";

/// The two per-user known folders the writer needs. Production code resolves them with the
/// shell's known-folder API (Tauri path resolver); tests pass temp dirs.
#[derive(Debug, Clone)]
pub struct Roots {
    /// `FOLDERID_RoamingAppData` (`%APPDATA%`).
    pub roaming: PathBuf,
    /// `FOLDERID_LocalAppData` (`%LOCALAPPDATA%`).
    pub local: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Exe installer (Squirrel) or an install we cannot classify; config in `%APPDATA%\Claude`.
    Standard,
    /// Microsoft Store / MSIX package; config in the package's LocalCache.
    Msix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub variant: Variant,
    pub config_path: PathBuf,
}

/// `Claude_` + 13 lower-case alphanumerics (the publisher-id hash of a package family name).
fn is_claude_family(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("Claude_") else {
        return false;
    };
    rest.len() == 13
        && rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// MSIX package families of Claude installed for this user (sorted, usually zero or one).
pub fn msix_families(local: &Path) -> Vec<String> {
    let Ok(rd) = fs::read_dir(local.join("Packages")) else {
        return Vec::new();
    };
    let mut v: Vec<String> = rd
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| is_claude_family(n))
        .collect();
    v.sort();
    v
}

/// Every installed Claude Desktop variant and the config file it reads.
pub fn detect(roots: &Roots) -> Vec<Install> {
    let families = if cfg!(windows) {
        msix_families(&roots.local)
    } else {
        Vec::new()
    };
    let mut out: Vec<Install> = families
        .into_iter()
        .map(|fam| Install {
            variant: Variant::Msix,
            config_path: roots
                .local
                .join("Packages")
                .join(fam)
                .join("LocalCache")
                .join("Roaming")
                .join("Claude")
                .join(CONFIG_FILE),
        })
        .collect();
    let squirrel_app = roots.local.join("AnthropicClaude").is_dir();
    let roaming_dir = roots.roaming.join("Claude");
    // A `%APPDATA%\Claude` folder without the Squirrel app dir counts only when no MSIX package
    // exists: the MSIX app never writes there (virtualised), so with MSIX present it is a leftover.
    if squirrel_app || (out.is_empty() && roaming_dir.is_dir()) {
        out.push(Install {
            variant: Variant::Standard,
            config_path: roaming_dir.join(CONFIG_FILE),
        });
    }
    out
}

/// State of our entry in `path` compared with `entry`.
#[cfg_attr(not(test), allow(dead_code))] // per-app shortcut; the app and sr-setup use `jsonfile` with a target
pub fn entry_state(path: &Path, entry: &Value) -> EntryState {
    jsonfile::entry_state(path, SERVERS_KEY, entry)
}

/// Adds (or updates) our entry. Every other key and server entry is kept as is.
#[cfg_attr(not(test), allow(dead_code))] // per-app shortcut; the app and sr-setup use `jsonfile` with a target
pub fn connect(path: &Path, entry: &Value) -> Result<Outcome, WriteError> {
    jsonfile::connect(path, SERVERS_KEY, BACKUP_SUFFIX, entry)
}

/// Removes our entry (Settings "Disconnect", uninstaller cleanup). Other entries are kept.
#[cfg_attr(not(test), allow(dead_code))] // per-app shortcut; the app and sr-setup use `jsonfile` with a target
pub fn disconnect(path: &Path) -> Result<Outcome, WriteError> {
    jsonfile::disconnect(path, SERVERS_KEY, BACKUP_SUFFIX)
}

/// Read → parse → `edit(mcpServers)` → backup → write → verify (restore on mismatch).
/// `write` is injectable so tests can simulate a bad write.
#[cfg_attr(not(test), allow(dead_code))]
fn change(
    path: &Path,
    write: impl Fn(&Path, &[u8]) -> io::Result<()>,
    edit: impl FnOnce(&mut Map<String, Value>) -> bool,
) -> Result<Outcome, WriteError> {
    jsonfile::change(path, SERVERS_KEY, BACKUP_SUFFIX, write, edit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::{SERVER_KEY, atomic_write, server_entry};

    const SECRET: &str = "sk-test-SECRET-do-not-leak-123";

    fn roots(t: &tempfile::TempDir) -> Roots {
        let r = Roots {
            roaming: t.path().join("Roaming"),
            local: t.path().join("Local"),
        };
        fs::create_dir_all(&r.roaming).unwrap();
        fs::create_dir_all(&r.local).unwrap();
        r
    }

    fn entry() -> Value {
        server_entry(Path::new(r"C:\Program Files\SR\sr-bridge.exe"), "claude")
    }

    fn user_config() -> Value {
        serde_json::json!({
            "globalShortcut": "Ctrl+Space",
            "someFutureField": {"nested": [1, 2, {"x": null}]},
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": SECRET},
                    "unknownPerServer": true
                },
                "fs": {"command": "node", "args": ["fs.js"]}
            }
        })
    }

    fn write_json(path: &Path, v: &Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn family_names() {
        assert!(is_claude_family("Claude_pzs8sxrjxfjjc"));
        assert!(!is_claude_family("Claude_pzs8sxrjxfjj")); // 12
        assert!(!is_claude_family("Claude_PZS8SXRJXFJJC"));
        assert!(!is_claude_family("ClaudeHelper_pzs8sxrjxfjjc"));
        assert!(!is_claude_family("claude_pzs8sxrjxfjjc"));
    }

    /// macOS: the Application Support folder Claude creates on its first start; no MSIX.
    #[cfg(not(windows))]
    #[test]
    fn detect_macos_folder_and_never_msix() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(&t);
        fs::create_dir_all(r.local.join("Packages").join("Claude_abcdefghij123")).unwrap();
        assert!(detect(&r).is_empty());
        fs::create_dir_all(r.roaming.join("Claude")).unwrap();
        let d = detect(&r);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].variant, Variant::Standard);
        assert_eq!(d[0].config_path, r.roaming.join("Claude").join(CONFIG_FILE));
    }

    #[test]
    fn detect_nothing_installed() {
        let t = tempfile::tempdir().unwrap();
        assert!(detect(&roots(&t)).is_empty());
    }

    #[test]
    fn detect_squirrel() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(&t);
        fs::create_dir_all(r.local.join("AnthropicClaude").join("app-1.1.280")).unwrap();
        let d = detect(&r);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].variant, Variant::Standard);
        assert_eq!(d[0].config_path, r.roaming.join("Claude").join(CONFIG_FILE));
    }

    #[cfg(windows)]
    #[test]
    fn detect_msix_family_resolved_at_runtime() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(&t);
        let pk = r.local.join("Packages");
        fs::create_dir_all(pk.join("Claude_abcdefghij123")).unwrap();
        fs::create_dir_all(pk.join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")).unwrap();
        fs::create_dir_all(pk.join("Claude_bad")).unwrap();
        // Leftover roaming folder must not count as a second install while MSIX exists.
        fs::create_dir_all(r.roaming.join("Claude")).unwrap();
        let d = detect(&r);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].variant, Variant::Msix);
        assert_eq!(
            d[0].config_path,
            pk.join("Claude_abcdefghij123")
                .join("LocalCache")
                .join("Roaming")
                .join("Claude")
                .join(CONFIG_FILE)
        );
    }

    #[cfg(windows)]
    #[test]
    fn detect_both_variants() {
        let t = tempfile::tempdir().unwrap();
        let r = roots(&t);
        fs::create_dir_all(r.local.join("Packages").join("Claude_abcdefghij123")).unwrap();
        fs::create_dir_all(r.local.join("AnthropicClaude")).unwrap();
        let v: Vec<_> = detect(&r).into_iter().map(|i| i.variant).collect();
        assert_eq!(v, [Variant::Msix, Variant::Standard]);
    }

    #[test]
    fn connect_creates_missing_folder_and_file() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("Roaming").join("Claude").join(CONFIG_FILE);
        assert_eq!(entry_state(&p, &entry()), EntryState::NotConnected);
        assert_eq!(connect(&p, &entry()), Ok(Outcome::Written));
        assert_eq!(
            read_json(&p),
            serde_json::json!({"mcpServers": {SERVER_KEY: entry()}})
        );
        assert_eq!(entry_state(&p, &entry()), EntryState::Connected);
        let mut bak = p.as_os_str().to_owned();
        bak.push(BACKUP_SUFFIX);
        assert!(!Path::new(&bak).exists(), "no original, no backup");
    }

    #[test]
    fn merge_keeps_unknown_fields_and_other_servers() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(CONFIG_FILE);
        write_json(&p, &user_config());
        let original = fs::read(&p).unwrap();
        assert_eq!(connect(&p, &entry()), Ok(Outcome::Written));

        let mut want = user_config();
        want["mcpServers"][SERVER_KEY] = entry();
        assert_eq!(read_json(&p), want);

        let mut bak = p.as_os_str().to_owned();
        bak.push(BACKUP_SUFFIX);
        assert_eq!(
            fs::read(Path::new(&bak)).unwrap(),
            original,
            "backup = original bytes"
        );
        assert!(
            fs::read_dir(t.path()).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("sr-tmp")),
            "temp file cleaned up"
        );
    }

    #[test]
    fn connect_is_idempotent_and_updates_outdated_entry() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(CONFIG_FILE);
        let mut cfg = user_config();
        cfg["mcpServers"][SERVER_KEY] = server_entry(Path::new(r"D:\old\sr-bridge.exe"), "claude");
        write_json(&p, &cfg);
        assert_eq!(entry_state(&p, &entry()), EntryState::Outdated);
        assert_eq!(connect(&p, &entry()), Ok(Outcome::Written));
        let after = fs::read(&p).unwrap();
        assert_eq!(connect(&p, &entry()), Ok(Outcome::Unchanged));
        assert_eq!(
            fs::read(&p).unwrap(),
            after,
            "no rewrite when already connected"
        );
    }

    #[test]
    fn bom_and_empty_files_are_accepted() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(CONFIG_FILE);
        fs::write(&p, b"   \r\n").unwrap();
        assert_eq!(connect(&p, &entry()), Ok(Outcome::Written));
        let mut b = b"\xEF\xBB\xBF".to_vec();
        b.extend(serde_json::to_vec(&user_config()).unwrap());
        fs::write(&p, &b).unwrap();
        assert_eq!(connect(&p, &entry()), Ok(Outcome::Written));
        assert_eq!(
            read_json(&p)["mcpServers"]["github"]["env"]["GITHUB_PERSONAL_ACCESS_TOKEN"],
            SECRET
        );
    }

    #[test]
    fn invalid_files_are_never_touched() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(CONFIG_FILE);
        for bad in [
            format!("{{\"mcpServers\": {{\"x\": \"{SECRET}\" "), // truncated JSON
            "[1, 2, 3]".to_owned(),                              // not an object
            format!("{{\"mcpServers\": \"{SECRET}\"}}"),         // mcpServers not an object
        ] {
            fs::write(&p, &bad).unwrap();
            let e = connect(&p, &entry()).unwrap_err();
            assert_eq!(e, WriteError::Unreadable);
            assert_eq!(fs::read_to_string(&p).unwrap(), bad, "untouched");
            assert_eq!(entry_state(&p, &entry()), EntryState::Unreadable);
            assert!(!format!("{e:?}").contains(SECRET));
            assert_eq!(disconnect(&p), Err(WriteError::Unreadable));
            assert_eq!(fs::read_to_string(&p).unwrap(), bad);
        }
        let mut bak = p.as_os_str().to_owned();
        bak.push(BACKUP_SUFFIX);
        assert!(!Path::new(&bak).exists());
    }

    #[test]
    fn failed_verification_restores_the_original() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(CONFIG_FILE);
        write_json(&p, &user_config());
        let original = fs::read(&p).unwrap();
        // Simulate a writer that loses data (e.g. another program rewrote the file in between).
        let lossy = |path: &Path, _: &[u8]| atomic_write(path, b"{\"mcpServers\":{}}");
        let r = change(&p, lossy, |s| {
            s.insert(SERVER_KEY.into(), entry());
            true
        });
        assert_eq!(r, Err(WriteError::VerifyFailed));
        assert_eq!(fs::read(&p).unwrap(), original);

        // Same for a file we created: removed again.
        let q = t.path().join("new").join(CONFIG_FILE);
        let r = change(&q, lossy, |s| {
            s.insert(SERVER_KEY.into(), entry());
            true
        });
        assert_eq!(r, Err(WriteError::VerifyFailed));
        assert!(!q.exists());
    }

    #[test]
    fn disconnect_removes_only_our_entry() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(CONFIG_FILE);
        write_json(&p, &user_config());
        connect(&p, &entry()).unwrap();
        assert_eq!(disconnect(&p), Ok(Outcome::Written));
        assert_eq!(read_json(&p), user_config());
        assert_eq!(disconnect(&p), Ok(Outcome::Unchanged));
        assert_eq!(
            disconnect(&t.path().join("missing.json")),
            Ok(Outcome::Unchanged)
        );
    }

    #[test]
    fn locked_or_unwritable_dir_reports_io_kind_only() {
        let t = tempfile::tempdir().unwrap();
        // A *file* where the Claude folder should be: create_dir_all fails.
        let blocker = t.path().join("Claude");
        fs::write(&blocker, SECRET).unwrap();
        let e = connect(&blocker.join(CONFIG_FILE), &entry()).unwrap_err();
        assert!(matches!(e, WriteError::Io(_)), "{e:?}");
        assert!(!format!("{e:?}").contains(SECRET));
    }
}
