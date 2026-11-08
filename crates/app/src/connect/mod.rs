//! "Connect" writers for MCP clients (P1.4: one click, or a copy-paste snippet).
//!
//! * [`claude`]: Claude Desktop (Squirrel and MSIX), M2.
//! * [`cursor`]: Cursor (`%USERPROFILE%\.cursor\mcp.json`), M4.
//! * [`vscode`]: VS Code and VS Code Insiders (`%APPDATA%\Code\User\mcp.json`), M4.
//!
//! All three use the one merge writer in [`jsonfile`]. This folder is **Tauri-free**:
//! `installer/sr-setup` compiles it verbatim (`#[path]`), so the uninstaller removes our entries
//! with the same detection and merge rules. Only `std` and `serde_json` may be used here.
//!
//! Privacy rule for every writer: client config files can hold other servers' API keys and
//! tokens. Their contents are parsed, merged and written back, but **never** logged, printed,
//! put into an error message, diagnostics or the UI. Errors carry only a fixed plain sentence.

pub mod claude;
pub mod cursor;
pub mod jsonfile;
pub mod vscode;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Name of our entry in every client's server map.
pub const SERVER_KEY: &str = "selene-retrieval";

/// Per-user known folders the writers need. Production code resolves them with the shell's
/// known-folder API (Tauri path resolver in the app, `SHGetKnownFolderPath` in sr-setup), never
/// from environment variables; tests pass temp dirs.
#[derive(Debug, Clone)]
pub struct ClientRoots {
    /// `FOLDERID_RoamingAppData` (`%APPDATA%`).
    pub roaming: PathBuf,
    /// `FOLDERID_LocalAppData` (`%LOCALAPPDATA%`).
    pub local: PathBuf,
    /// `FOLDERID_Profile` (`%USERPROFILE%`): Cursor keeps its config under `.cursor`.
    pub home: PathBuf,
}

impl ClientRoots {
    pub fn claude(&self) -> claude::Roots {
        claude::Roots {
            roaming: self.roaming.clone(),
            local: self.local.clone(),
        }
    }
}

/// The MCP server entry (an `exec` entry, never a node-type extension: m0-d §5.8). Claude
/// Desktop and Cursor format.
pub fn server_entry(bridge_exe: &Path, client: &str) -> serde_json::Value {
    serde_json::json!({
        "command": bridge_exe.to_string_lossy(),
        "args": ["--client", client],
    })
}

/// Copy-paste snippet for clients we cannot (or may not) write ourselves (Claude format, the
/// most common one: `mcpServers`).
pub fn snippet(bridge_exe: &Path, client: &str) -> String {
    let v = serde_json::json!({ "mcpServers": { SERVER_KEY: server_entry(bridge_exe, client) } });
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

/// Write `bytes` to `path` atomically: temp file in the same directory, flush + fsync, rename
/// over the target (`MoveFileExW(REPLACE_EXISTING)` on Windows). A crash leaves either the old or
/// the new file, never a half-written one.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent dir"))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp: PathBuf = dir.join(format!(".{name}.sr-tmp-{}", std::process::id()));
    let res = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
pub(crate) mod test_guard {
    use std::fs;
    use std::path::Path;

    /// Hard guard: unit tests must never touch a real client config (Claude, Cursor, VS Code).
    /// Every writer calls this first under `cfg(test)`; a path outside the temp dir panics.
    pub fn assert_temp_path(path: &Path) {
        let tmp = std::env::temp_dir();
        let tmp = fs::canonicalize(&tmp).unwrap_or(tmp);
        let mut p = path.to_path_buf();
        // Canonicalise the deepest existing ancestor (the file may not exist yet).
        while !p.exists() {
            match p.parent() {
                Some(parent) => p = parent.to_path_buf(),
                None => break,
            }
        }
        let canon = fs::canonicalize(&p).unwrap_or(p);
        assert!(
            canon.starts_with(&tmp),
            "test tried to write outside the temp dir"
        );
    }

    #[test]
    #[should_panic(expected = "outside the temp dir")]
    fn the_guard_refuses_a_real_profile_path() {
        assert_temp_path(Path::new(r"C:\Windows\System32\drivers\etc\hosts"));
    }
}
