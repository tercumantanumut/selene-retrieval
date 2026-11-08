//! App-local preferences that belong to the app, not the service: today only whether the user has
//! made a "Start with Windows" choice (OQ-16: the wizard must not turn autostart back on for a user
//! who turned it off). Stored as `prefs.json` in the app's own config folder
//! (`%APPDATA%\ai.selene.retrieval`, removed by the uninstaller). No secrets, paths or names.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE: &str = "prefs.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// The user (or the wizard's Finish) chose "Start with Windows" at least once.
    pub autostart_decided: bool,
    /// R-UX22: "Selene Retrieval is still running" was shown after the first window close.
    pub close_note_shown: bool,
    /// OQ-10 (c): "Your folders update again the next time Selene Retrieval starts." was shown on
    /// the first Quit.
    pub quit_note_shown: bool,
}

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Missing or unreadable file = defaults (a fresh first run).
pub fn load(dir: &Path) -> Prefs {
    std::fs::read(path(dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Best effort (temp file + rename); a failure only means the wizard asks again.
pub fn save(dir: &Path, p: &Prefs) {
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(p).unwrap_or_default())?;
        std::fs::rename(&tmp, path(dir))
    };
    if let Err(e) = write() {
        tracing::warn!(kind = ?e.kind(), "couldn't save app preferences");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(load(t.path()), Prefs::default());
        save(
            t.path(),
            &Prefs {
                autostart_decided: true,
                ..Prefs::default()
            },
        );
        assert!(load(t.path()).autostart_decided);
        assert!(
            !load(t.path()).close_note_shown,
            "older files read the new fields as false"
        );
        std::fs::write(t.path().join(FILE), b"{ broken").unwrap();
        assert_eq!(load(t.path()), Prefs::default());
    }
}

/// Marks a one-time note as shown; returns `true` the first time only (then it is saved).
pub fn first_time(dir: Option<&Path>, pick: impl Fn(&mut Prefs) -> &mut bool) -> bool {
    let Some(d) = dir else {
        return false; // no config folder: never nag on every close
    };
    let mut p = load(d);
    let flag = pick(&mut p);
    if *flag {
        return false;
    }
    *flag = true;
    save(d, &p);
    true
}

#[cfg(test)]
mod first_time_tests {
    use super::*;

    #[test]
    fn a_note_shows_once() {
        let t = tempfile::tempdir().unwrap();
        assert!(first_time(Some(t.path()), |p| &mut p.close_note_shown));
        assert!(!first_time(Some(t.path()), |p| &mut p.close_note_shown));
        assert!(first_time(Some(t.path()), |p| &mut p.quit_note_shown));
        assert!(!first_time(None, |p| &mut p.quit_note_shown));
    }
}
