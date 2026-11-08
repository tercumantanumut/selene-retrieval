//! The one JSON-config merge writer behind every "Connect" button (Claude Desktop, Cursor,
//! VS Code). Tauri-free on purpose: `installer/sr-setup` compiles this folder verbatim, so the
//! uninstaller removes our entries with exactly the same rules.
//!
//! Contract (R-UX3, task 3 of M2-A, M4 page 29):
//! * parse the file as a JSON object (empty file = `{}`, a UTF-8 BOM is accepted); anything else
//!   (JSONC comments, trailing commas, an array, a non-object server map) is **never** touched;
//! * change only our own entry under the client's server map (`mcpServers` / `servers`); every
//!   other key and server entry is kept as is;
//! * back up the original next to it, write atomically, re-read and compare, and put the
//!   original back if the check fails;
//! * never log, print or return any part of the file: errors carry only a kind.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::{SERVER_KEY, atomic_write};

/// State of our entry in one config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    Connected,
    /// Our entry exists but points somewhere else (moved install, older version).
    Outdated,
    NotConnected,
    /// The file exists but isn't a JSON object we can safely edit.
    Unreadable,
}

/// Why a write did not happen. Holds no file content, ever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// The file isn't valid JSON or the server map isn't an object. The file was not touched.
    Unreadable,
    /// Reading or writing failed (permission, disk, locked file).
    Io(io::ErrorKind),
    /// The re-read didn't match what was written; the original was restored.
    VerifyFailed,
}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> Self {
        WriteError::Io(e.kind())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// File written and verified.
    Written,
    /// Nothing to do (already in the requested state); the file was not rewritten.
    Unchanged,
}

/// Parses config bytes into a top-level object. Empty or whitespace-only files count as `{}`
/// (apps create the file lazily). A UTF-8 BOM is accepted.
pub fn parse_object(bytes: &[u8]) -> Result<Map<String, Value>, WriteError> {
    let b = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if b.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(b) {
        Ok(Value::Object(m)) => Ok(m),
        // Never include serde's message: it is harmless today, but the rule is "no content".
        _ => Err(WriteError::Unreadable),
    }
}

pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, WriteError> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `<path><suffix>` next to the file (same folder, so the same permissions).
pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// State of our entry under `servers_key` in `path`, compared with `entry`.
pub fn entry_state(path: &Path, servers_key: &str, entry: &Value) -> EntryState {
    let bytes = match read_optional(path) {
        Ok(Some(b)) => b,
        Ok(None) => return EntryState::NotConnected,
        Err(_) => return EntryState::Unreadable,
    };
    let Ok(root) = parse_object(&bytes) else {
        return EntryState::Unreadable;
    };
    match root.get(servers_key) {
        None => EntryState::NotConnected,
        Some(Value::Object(servers)) => match servers.get(SERVER_KEY) {
            None => EntryState::NotConnected,
            Some(v) if v == entry => EntryState::Connected,
            Some(_) => EntryState::Outdated,
        },
        Some(_) => EntryState::Unreadable,
    }
}

/// Adds (or updates) our entry. Every other key and server entry is kept as is.
pub fn connect(
    path: &Path,
    servers_key: &str,
    backup_suffix: &str,
    entry: &Value,
) -> Result<Outcome, WriteError> {
    change(path, servers_key, backup_suffix, atomic_write, |servers| {
        if servers.get(SERVER_KEY) == Some(entry) {
            false
        } else {
            servers.insert(SERVER_KEY.to_owned(), entry.clone());
            true
        }
    })
}

/// Removes our entry (Disconnect, uninstaller cleanup). Other entries are kept.
pub fn disconnect(
    path: &Path,
    servers_key: &str,
    backup_suffix: &str,
) -> Result<Outcome, WriteError> {
    if !path.exists() {
        return Ok(Outcome::Unchanged);
    }
    change(path, servers_key, backup_suffix, atomic_write, |servers| {
        servers.remove(SERVER_KEY).is_some()
    })
}

/// OQ-46 "Repair…" (behind a plain confirm in the app): the unreadable file is **moved aside**
/// unchanged as `<file><broken_suffix>` (never read, parsed or copied by us), then a fresh file
/// holding only our entry is written and verified. Returns the path of the kept broken file.
/// A readable file is never replaced: use [`connect`] instead (`Unreadable` is not returned for
/// it; the caller checks [`entry_state`] first, and this function re-checks).
pub fn repair(
    path: &Path,
    servers_key: &str,
    broken_suffix: &str,
    entry: &Value,
) -> Result<PathBuf, WriteError> {
    #[cfg(test)]
    super::test_guard::assert_temp_path(path);

    if entry_state(path, servers_key, entry) != EntryState::Unreadable {
        return Err(WriteError::Unreadable);
    }
    let kept = sibling(path, broken_suffix);
    if kept.exists() {
        // Never overwrite an earlier kept copy.
        return Err(WriteError::Io(io::ErrorKind::AlreadyExists));
    }
    fs::rename(path, &kept)?;
    let mut servers = Map::new();
    servers.insert(SERVER_KEY.to_owned(), entry.clone());
    let mut root = Map::new();
    root.insert(servers_key.to_owned(), Value::Object(servers));
    let expected = Value::Object(root);
    let mut bytes = serde_json::to_vec_pretty(&expected).map_err(|_| WriteError::Unreadable)?;
    bytes.push(b'\n');
    let restore = |e: WriteError| {
        let _ = fs::remove_file(path);
        let _ = fs::rename(&kept, path);
        e
    };
    atomic_write(path, &bytes).map_err(|e| restore(e.into()))?;
    let ok = read_optional(path)
        .ok()
        .flatten()
        .and_then(|b| parse_object(&b).ok())
        .is_some_and(|m| Value::Object(m) == expected);
    if ok {
        Ok(kept)
    } else {
        Err(restore(WriteError::VerifyFailed))
    }
}

/// Read → parse → `edit(servers map)` → backup → write → verify (restore on mismatch).
/// `write` is injectable so tests can simulate a bad write.
pub fn change(
    path: &Path,
    servers_key: &str,
    backup_suffix: &str,
    write: impl Fn(&Path, &[u8]) -> io::Result<()>,
    edit: impl FnOnce(&mut Map<String, Value>) -> bool,
) -> Result<Outcome, WriteError> {
    #[cfg(test)]
    super::test_guard::assert_temp_path(path);

    let original = read_optional(path)?;
    let mut root = parse_object(original.as_deref().unwrap_or_default())?;
    let servers = root
        .entry(servers_key)
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(servers) = servers else {
        return Err(WriteError::Unreadable);
    };
    if !edit(servers) {
        return Ok(Outcome::Unchanged);
    }
    let expected = Value::Object(root);
    let mut bytes = serde_json::to_vec_pretty(&expected).map_err(|_| WriteError::Unreadable)?;
    bytes.push(b'\n');

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if let Some(orig) = &original {
        atomic_write(&sibling(path, backup_suffix), orig)?;
    }
    write(path, &bytes)?;

    let ok = read_optional(path)
        .ok()
        .flatten()
        .and_then(|b| parse_object(&b).ok())
        .is_some_and(|m| Value::Object(m) == expected);
    if ok {
        return Ok(Outcome::Written);
    }
    // Put the user's file back exactly as it was (or remove the one we created).
    match &original {
        Some(orig) => {
            let _ = atomic_write(path, orig);
        }
        None => {
            let _ = fs::remove_file(path);
        }
    }
    Err(WriteError::VerifyFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SECRET: &str = "sk-test-SECRET-do-not-leak-456";
    const BAK: &str = ".selene-retrieval.bak";

    fn entry() -> Value {
        json!({"type": "stdio", "command": r"C:\SR\sr-bridge.exe", "args": ["--client", "vscode"]})
    }

    #[test]
    fn other_server_map_names_work_the_same() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("mcp.json");
        let user = json!({
            "inputs": [{"id": "tok", "type": "promptString", "password": true}],
            "servers": {"gh": {"type": "http", "url": "https://x", "headers": {"A": SECRET}}}
        });
        fs::write(&p, serde_json::to_vec(&user).unwrap()).unwrap();
        assert_eq!(
            entry_state(&p, "servers", &entry()),
            EntryState::NotConnected
        );
        assert_eq!(connect(&p, "servers", BAK, &entry()), Ok(Outcome::Written));
        let got: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(got["servers"]["gh"]["headers"]["A"], SECRET);
        assert_eq!(got["inputs"], user["inputs"]);
        assert_eq!(got["servers"][SERVER_KEY], entry());
        assert_eq!(entry_state(&p, "servers", &entry()), EntryState::Connected);
        // The other map name doesn't count as ours.
        assert_eq!(
            entry_state(&p, "mcpServers", &entry()),
            EntryState::NotConnected
        );
        assert_eq!(disconnect(&p, "servers", BAK), Ok(Outcome::Written));
        let got: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(got, user);
    }

    #[test]
    fn jsonc_comments_are_never_touched() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("mcp.json");
        let jsonc =
            format!("// my servers\n{{\"servers\": {{\"x\": {{\"env\": \"{SECRET}\"}},}}}}\n");
        fs::write(&p, &jsonc).unwrap();
        assert_eq!(entry_state(&p, "servers", &entry()), EntryState::Unreadable);
        let e = connect(&p, "servers", BAK, &entry()).unwrap_err();
        assert_eq!(e, WriteError::Unreadable);
        assert!(!format!("{e:?}").contains(SECRET));
        assert_eq!(fs::read_to_string(&p).unwrap(), jsonc);
        assert!(!sibling(&p, BAK).exists());
    }

    #[test]
    fn repair_keeps_the_broken_file_and_writes_only_ours() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("claude_desktop_config.json");
        let broken = format!("{{\"mcpServers\": {{\"x\": \"{SECRET}\"");
        fs::write(&p, &broken).unwrap();
        let kept = repair(&p, "mcpServers", ".broken-20261019", &entry()).unwrap();
        assert_eq!(
            fs::read_to_string(&kept).unwrap(),
            broken,
            "kept byte for byte"
        );
        let got: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(got, json!({"mcpServers": {SERVER_KEY: entry()}}));
        // A readable file is never replaced, and an earlier kept copy is never overwritten.
        assert_eq!(
            repair(&p, "mcpServers", ".broken-20261019", &entry()),
            Err(WriteError::Unreadable)
        );
        fs::write(&p, &broken).unwrap();
        assert!(matches!(
            repair(&p, "mcpServers", ".broken-20261019", &entry()),
            Err(WriteError::Io(_))
        ));
        assert_eq!(fs::read_to_string(&p).unwrap(), broken, "untouched");
    }

    #[test]
    fn failed_verification_restores_the_original() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("mcp.json");
        let user = json!({"servers": {"a": {"command": "a.exe"}}});
        fs::write(&p, serde_json::to_vec(&user).unwrap()).unwrap();
        let original = fs::read(&p).unwrap();
        let lossy = |path: &Path, _: &[u8]| atomic_write(path, b"{\"servers\":{}}");
        let r = change(&p, "servers", BAK, lossy, |s| {
            s.insert(SERVER_KEY.into(), entry());
            true
        });
        assert_eq!(r, Err(WriteError::VerifyFailed));
        assert_eq!(fs::read(&p).unwrap(), original);
    }
}
