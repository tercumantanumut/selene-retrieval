//! Lenient per-field loading, auto-creation and atomic writes of `settings.toml`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::Settings;
use crate::error::{Error, ErrorCode, Section};

/// File name inside the data directory.
pub const SETTINGS_FILE: &str = "settings.toml";

const HEADER: &str = "# Selene Retrieval settings. Change these in the app (Settings).\n\
                      # Invalid values are reset to their defaults automatically.\n\n";

/// One field that was reset to its default (or would be, for `validate`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldIssue {
    /// Dotted path, e.g. `ranking.rrf_k`. Empty = the whole file.
    pub path: String,
    pub section: Section,
    /// Internal reason (shown under Details / logs, not as the user-facing title).
    pub reason: String,
}

impl FieldIssue {
    pub(crate) fn new(path: &str, section: Section, reason: impl Into<String>) -> Self {
        Self {
            path: path.to_string(),
            section,
            reason: reason.into(),
        }
    }
}

/// Result of [`load_or_create`]. Loading never fails; problems are reported here.
#[derive(Debug)]
pub struct LoadReport {
    pub settings: Settings,
    /// Fields reset to defaults.
    pub issues: Vec<FieldIssue>,
    /// The file did not exist and was created with defaults.
    pub created: bool,
    /// The file could not be parsed at all; it was backed up and replaced by defaults.
    pub file_reset: bool,
    /// Writing the file failed (settings are still usable in memory).
    pub persist_error: Option<Error>,
}

impl LoadReport {
    /// The user-facing notice ("Some settings were reset" + "Review settings"), if any.
    pub fn notice(&self) -> Option<Error> {
        let first = self.issues.first()?;
        let detail = self
            .issues
            .iter()
            .map(|i| {
                if i.path.is_empty() {
                    "settings file".to_string()
                } else {
                    i.path.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        Some(
            Error::new(ErrorCode::SettingsInvalid)
                .with_section(first.section)
                .with_detail(detail),
        )
    }
}

fn section_for(path: &[String]) -> Section {
    match path.first().map(String::as_str) {
        Some("search") => Section::Search,
        Some("answers") => Section::Answers,
        Some("provider" | "network") => Section::Providers,
        Some("indexing") => Section::Indexing,
        Some("ranking") => Section::Ranking,
        Some("integrations") => Section::Integrations,
        Some("system" | "version") => Section::System,
        _ => Section::General,
    }
}

fn get_at<'a>(root: &'a toml::Table, path: &[String]) -> Option<&'a toml::Value> {
    let (last, parents) = path.split_last()?;
    let mut t = root;
    for p in parents {
        t = t.get(p)?.as_table()?;
    }
    t.get(last)
}

fn set_at(root: &mut toml::Table, path: &[String], v: toml::Value) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut t = root;
    for p in parents {
        match t.get_mut(p).and_then(toml::Value::as_table_mut) {
            Some(next) => t = next,
            None => return false,
        }
    }
    t.insert(last.clone(), v);
    true
}

fn deserializes(t: &toml::Table) -> bool {
    toml::Value::Table(t.clone()).try_into::<Settings>().is_ok()
}

/// Apply user values onto the defaults one leaf at a time, keeping only values that deserialize.
fn merge(
    root: &mut toml::Table,
    path: &mut Vec<String>,
    user: &toml::Table,
    issues: &mut Vec<FieldIssue>,
) {
    for (k, uv) in user {
        path.push(k.clone());
        let base_is_table = get_at(root, path).is_some_and(toml::Value::is_table);
        match uv.as_table() {
            Some(ut) if base_is_table => merge(root, path, ut, issues),
            _ => {
                let mut candidate = root.clone();
                if set_at(&mut candidate, path, uv.clone()) && deserializes(&candidate) {
                    *root = candidate;
                } else {
                    issues.push(FieldIssue::new(
                        &path.join("."),
                        section_for(path),
                        "wrong type or unknown value",
                    ));
                }
            }
        }
        path.pop();
    }
}

/// Parse settings leniently. Returns the settings, every field reset, and whether the whole file
/// was unreadable (then all values are defaults).
pub fn from_toml_str(s: &str) -> (Settings, Vec<FieldIssue>, bool) {
    let s = s.strip_prefix('\u{feff}').unwrap_or(s);
    let user: toml::Table = match s.parse::<toml::Table>() {
        Ok(t) => t,
        Err(e) => {
            let issue = FieldIssue::new("", Section::General, format!("not valid TOML: {e}"));
            return (Settings::default(), vec![issue], true);
        }
    };
    let mut root = match toml::Value::try_from(Settings::default()) {
        Ok(toml::Value::Table(t)) => t,
        _ => toml::Table::new(),
    };
    let mut issues = Vec::new();
    merge(&mut root, &mut Vec::new(), &user, &mut issues);
    let mut settings = toml::Value::Table(root)
        .try_into::<Settings>()
        .unwrap_or_default();
    issues.extend(settings.sanitize());
    (settings, issues, false)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Write settings atomically: `<file>.tmp` → keep the previous file as `<file>.bak` → rename.
pub fn save_atomic(path: &Path, settings: &Settings) -> Result<(), Error> {
    let body = toml::to_string_pretty(settings)
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = sibling(path, ".tmp");
    fs::write(&tmp, format!("{HEADER}{body}"))?;
    if path.exists() {
        fs::copy(path, sibling(path, ".bak"))?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Load `settings.toml`, creating it with defaults if it is missing. Never fails: unreadable files
/// are backed up (`.invalid`) and replaced, invalid fields are reset (original kept as `.bak`).
pub fn load_or_create(path: &Path) -> LoadReport {
    let mut report = LoadReport {
        settings: Settings::default(),
        issues: Vec::new(),
        created: false,
        file_reset: false,
        persist_error: None,
    };
    let text = match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).ok(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            report.created = true;
            report.persist_error = save_atomic(path, &report.settings).err();
            if let Some(e) = &report.persist_error {
                tracing::warn!(error = %e, "could not create settings file; using defaults");
            }
            return report;
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not read settings file; using defaults");
            report.persist_error = Some(Error::from(e));
            return report;
        }
    };
    let (settings, issues, file_reset) = match text {
        Some(t) => from_toml_str(&t),
        None => (
            Settings::default(),
            vec![FieldIssue::new("", Section::General, "file is not UTF-8")],
            true,
        ),
    };
    for i in &issues {
        tracing::warn!(field = %i.path, reason = %i.reason, "setting reset to default");
    }
    report.settings = settings;
    report.file_reset = file_reset;
    if file_reset {
        let backup = fs::copy(path, sibling(path, ".invalid")).map(|_| ());
        report.persist_error = backup
            .map_err(Error::from)
            .and_then(|()| save_atomic(path, &report.settings))
            .err();
    } else if !issues.is_empty() {
        // save_atomic keeps the original as .bak before replacing it.
        report.persist_error = save_atomic(path, &report.settings).err();
    }
    report.issues = issues;
    report
}
