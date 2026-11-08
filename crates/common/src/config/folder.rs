//! Per-folder and per-collection settings (docs/02-architecture.md §2.4 `folders`, §2.5 Config).
//!
//! These live in the catalog (one home per setting), but their schema, defaults, tiers and
//! validation are defined here, next to [`super::Settings`], so there is still one config schema.
//! The catalog stores them as columns/JSON; the app and MCP send them as JSON. Both go through
//! [`FolderSettings::from_json_lenient`], which keeps good fields and resets bad ones.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::{ChunkPreset, FieldIssue, ReindexPolicy, SearchModel};
use crate::error::Section;

/// Which files of a folder are indexed (Basic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileTypesPreset {
    Documents,
    Code,
    #[default]
    DocumentsAndCode,
    AllText,
    Custom,
}

/// When a folder is re-synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// File watcher plus periodic reconcile.
    #[default]
    Auto,
    Manual,
    Scheduled,
    Triggered,
}

/// Settings of one folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct FolderSettings {
    /// Name shown in the app and to connected apps. Empty = the folder's own name.
    #[schemars(extend("x-tier" = "basic", "x-section" = "folder", "x-restart" = false))]
    pub display_name: String,
    /// Keep this folder up to date automatically.
    #[schemars(extend("x-tier" = "basic", "x-section" = "folder", "x-restart" = false))]
    pub keep_up_to_date: bool,
    /// Which kinds of files to include.
    #[schemars(extend("x-tier" = "basic", "x-section" = "folder", "x-restart" = false))]
    pub file_types_preset: FileTypesPreset,
    /// Include OneDrive files that are only stored online (downloads them).
    #[schemars(extend("x-tier" = "basic", "x-section" = "folder", "x-restart" = false))]
    pub include_online_only: bool,

    /// When to re-check the folder (used when "keep up to date" is on).
    #[schemars(extend("x-tier" = "advanced", "x-section" = "folder", "x-restart" = false))]
    pub sync_mode: SyncMode,
    /// Full re-check interval in minutes (5-1440).
    #[schemars(range(min = 5, max = 1440))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "folder", "x-restart" = false))]
    pub cadence_min: u32,
    /// When to re-index changed files.
    #[schemars(extend("x-tier" = "advanced", "x-section" = "folder", "x-restart" = false))]
    pub reindex_policy: ReindexPolicy,
    /// Include subfolders.
    #[schemars(extend("x-tier" = "advanced", "x-section" = "folder", "x-restart" = false))]
    pub recursive: bool,
    /// Extensions to include when the preset is "custom" (without the dot, e.g. "md").
    #[schemars(extend("x-tier" = "advanced", "x-section" = "folder", "x-restart" = false))]
    pub include_extensions: Vec<String>,
    /// Extra glob patterns to exclude.
    #[schemars(extend("x-tier" = "advanced", "x-section" = "folder", "x-restart" = false))]
    pub exclude_globs: Vec<String>,
    /// Skip files larger than this many bytes (1 KiB-200 MiB).
    #[schemars(range(min = 1024, max = 209_715_200))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "folder", "x-restart" = false))]
    pub max_file_bytes: u64,
    /// Chunk size preset.
    #[schemars(extend("x-tier" = "advanced", "x-section" = "folder", "x-restart" = false))]
    pub chunk_preset: ChunkPreset,
    /// Custom chunk size in tokens (64-8192).
    #[schemars(range(min = 64, max = 8192))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "folder", "x-restart" = false))]
    pub chunk_size_override: Option<u32>,
    /// Custom chunk overlap in tokens (0-2048, smaller than the chunk size).
    #[schemars(range(min = 0, max = 2048))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "folder", "x-restart" = false))]
    pub chunk_overlap_override: Option<u32>,
    /// Follow symbolic links (never outside the folder).
    #[schemars(extend("x-tier" = "advanced", "x-section" = "folder", "x-restart" = false))]
    pub follow_symlinks: bool,
}

impl Default for FolderSettings {
    fn default() -> Self {
        Self {
            display_name: String::new(),
            keep_up_to_date: true,
            file_types_preset: FileTypesPreset::DocumentsAndCode,
            include_online_only: false,
            sync_mode: SyncMode::Auto,
            cadence_min: 60,
            reindex_policy: ReindexPolicy::Smart,
            recursive: true,
            include_extensions: Vec::new(),
            exclude_globs: Vec::new(),
            max_file_bytes: 10 * 1024 * 1024,
            chunk_preset: ChunkPreset::Balanced,
            chunk_size_override: None,
            chunk_overlap_override: None,
            follow_symlinks: false,
        }
    }
}

fn valid_ext(e: &str) -> bool {
    (1..=16).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl FolderSettings {
    /// Effective sync mode: "keep up to date" off always means manual.
    pub fn effective_sync_mode(&self) -> SyncMode {
        if self.keep_up_to_date {
            self.sync_mode
        } else {
            SyncMode::Manual
        }
    }

    /// Fields that are out of bounds or break a cross-field rule (UI live validation).
    pub fn validate(&self) -> Vec<FieldIssue> {
        self.clone().sanitize()
    }

    /// Reset every invalid field to its default; returns what was reset.
    pub fn sanitize(&mut self) -> Vec<FieldIssue> {
        let d = Self::default();
        let mut out = Vec::new();
        let flag = |bad: bool, path: &str, why: &str, out: &mut Vec<FieldIssue>| {
            if bad {
                out.push(FieldIssue::new(path, Section::Folder, why));
            }
            bad
        };
        if flag(
            self.display_name.chars().count() > 200
                || self.display_name.chars().any(char::is_control),
            "display_name",
            "too long or contains control characters",
            &mut out,
        ) {
            self.display_name = d.display_name.clone();
        }
        if flag(
            !(5..=1440).contains(&self.cadence_min),
            "cadence_min",
            "must be between 5 and 1440",
            &mut out,
        ) {
            self.cadence_min = d.cadence_min;
        }
        if flag(
            !(1024..=200 * 1024 * 1024).contains(&self.max_file_bytes),
            "max_file_bytes",
            "must be between 1024 and 209715200",
            &mut out,
        ) {
            self.max_file_bytes = d.max_file_bytes;
        }
        if flag(
            self.include_extensions.iter().any(|e| !valid_ext(e)),
            "include_extensions",
            "extensions are letters and digits without a dot",
            &mut out,
        ) {
            self.include_extensions = d.include_extensions.clone();
        }
        if flag(
            self.exclude_globs
                .iter()
                .any(|g| g.is_empty() || g.len() > 1024),
            "exclude_globs",
            "contains an empty or overlong pattern",
            &mut out,
        ) {
            self.exclude_globs = d.exclude_globs.clone();
        }
        if flag(
            self.chunk_size_override
                .is_some_and(|v| !(64..=8192).contains(&v)),
            "chunk_size_override",
            "must be empty or between 64 and 8192",
            &mut out,
        ) {
            self.chunk_size_override = None;
        }
        if flag(
            self.chunk_overlap_override.is_some_and(|v| v > 2048)
                || matches!(
                    (self.chunk_size_override, self.chunk_overlap_override),
                    (Some(s), Some(o)) if o >= s
                ),
            "chunk_overlap_override",
            "must be smaller than the chunk size",
            &mut out,
        ) {
            self.chunk_overlap_override = None;
        }
        out
    }

    /// Lenient load from JSON (catalog row or app/MCP input): wrong-typed or unknown-valued
    /// fields fall back to their defaults, then bounds are checked. Never fails. Unknown keys are
    /// ignored, among them `indexing_mode` of the removed Files-only mode (Amendment 17).
    pub fn from_json_lenient(v: &serde_json::Value) -> (Self, Vec<FieldIssue>) {
        let (mut s, mut issues) = lenient_json::<Self>(v, Section::Folder);
        issues.extend(s.sanitize());
        (s, issues)
    }
}

/// Settings of one collection (a named group of folders sharing one search model).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CollectionSettings {
    /// Collection name.
    #[schemars(extend("x-tier" = "advanced", "x-section" = "integrations", "x-restart" = false))]
    pub name: String,
    /// Search model for this collection (changing it rebuilds the collection in the background).
    #[schemars(extend("x-tier" = "advanced", "x-section" = "integrations", "x-restart" = false))]
    pub search_model: SearchModel,
}

impl Default for CollectionSettings {
    fn default() -> Self {
        Self {
            name: "My files".into(),
            search_model: SearchModel::Standard,
        }
    }
}

impl CollectionSettings {
    pub fn sanitize(&mut self) -> Vec<FieldIssue> {
        let n = self.name.trim();
        if n.is_empty() || n.chars().count() > 100 || n.chars().any(char::is_control) {
            self.name = Self::default().name;
            return vec![FieldIssue::new(
                "name",
                Section::Integrations,
                "empty, too long or contains control characters",
            )];
        }
        Vec::new()
    }

    pub fn from_json_lenient(v: &serde_json::Value) -> (Self, Vec<FieldIssue>) {
        let (mut s, mut issues) = lenient_json::<Self>(v, Section::Integrations);
        issues.extend(s.sanitize());
        (s, issues)
    }
}

/// Apply each top-level key of `v` onto `T::default()` separately, keeping only those that
/// deserialize. Unknown keys are ignored (the catalog owns the columns).
fn lenient_json<T: Default + Serialize + DeserializeOwned>(
    v: &serde_json::Value,
    section: Section,
) -> (T, Vec<FieldIssue>) {
    let mut issues = Vec::new();
    let Some(user) = v.as_object() else {
        if !v.is_null() {
            issues.push(FieldIssue::new("", section, "not an object"));
        }
        return (T::default(), issues);
    };
    let mut base = match serde_json::to_value(T::default()) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return (T::default(), issues),
    };
    for (k, uv) in user {
        if !base.contains_key(k) {
            continue;
        }
        let mut candidate = base.clone();
        candidate.insert(k.clone(), uv.clone());
        if serde_json::from_value::<T>(serde_json::Value::Object(candidate.clone())).is_ok() {
            base = candidate;
        } else {
            issues.push(FieldIssue::new(k, section, "wrong type or unknown value"));
        }
    }
    let out = serde_json::from_value(serde_json::Value::Object(base)).unwrap_or_default();
    (out, issues)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn folder_defaults_are_valid_and_match_the_plan() {
        let f = FolderSettings::default();
        assert!(f.validate().is_empty());
        assert!(f.keep_up_to_date);
        assert_eq!(f.file_types_preset, FileTypesPreset::DocumentsAndCode);
        assert!(!f.include_online_only);
        assert!(!f.follow_symlinks);
        assert_eq!(f.max_file_bytes, 10 * 1024 * 1024);
        assert_eq!(f.cadence_min, 60);
        assert_eq!(f.effective_sync_mode(), SyncMode::Auto);
    }

    #[test]
    fn folder_bad_values_fall_back_per_field() {
        let (f, issues) = FolderSettings::from_json_lenient(&json!({
            "display_name": "Work",
            "keep_up_to_date": "yes",
            "file_types_preset": "everything",
            "cadence_min": 1,
            "max_file_bytes": 2048,
            "include_extensions": [".md"],
            "chunk_size_override": 128,
            "chunk_overlap_override": 500,
            "some_future_field": 1
        }));
        assert_eq!(f.display_name, "Work");
        assert!(f.keep_up_to_date, "wrong type -> default");
        assert_eq!(f.file_types_preset, FileTypesPreset::DocumentsAndCode);
        assert_eq!(f.cadence_min, 60);
        assert_eq!(f.max_file_bytes, 2048, "good value kept");
        assert!(f.include_extensions.is_empty());
        assert_eq!(f.chunk_size_override, Some(128));
        assert_eq!(f.chunk_overlap_override, None, "overlap >= size reset");
        let paths: Vec<_> = issues.iter().map(|i| i.path.as_str()).collect();
        for p in [
            "keep_up_to_date",
            "file_types_preset",
            "cadence_min",
            "include_extensions",
            "chunk_overlap_override",
        ] {
            assert!(paths.contains(&p), "{p} missing from {paths:?}");
        }
        assert!(issues.iter().all(|i| i.section == Section::Folder));
    }

    /// Amendment 17: the removed Files-only mode's key still loads (ignored, no issue), from the
    /// catalog or from an older client alike.
    #[test]
    fn removed_indexing_mode_key_is_ignored() {
        for mode in ["files_only", "vector"] {
            let (f, issues) = FolderSettings::from_json_lenient(&json!({
                "display_name": "Scans",
                "indexing_mode": mode
            }));
            assert!(issues.is_empty(), "{mode}: {issues:?}");
            assert_eq!(f.display_name, "Scans");
        }
        let f: FolderSettings =
            serde_json::from_value(json!({"indexing_mode": "files_only"})).unwrap();
        assert_eq!(f, FolderSettings::default());
    }

    #[test]
    fn folder_non_object_gives_defaults() {
        let (f, issues) = FolderSettings::from_json_lenient(&json!(42));
        assert_eq!(f, FolderSettings::default());
        assert_eq!(issues.len(), 1);
        let (f, issues) = FolderSettings::from_json_lenient(&serde_json::Value::Null);
        assert_eq!(f, FolderSettings::default());
        assert!(issues.is_empty());
    }

    #[test]
    fn keep_up_to_date_off_means_manual() {
        let f = FolderSettings {
            keep_up_to_date: false,
            sync_mode: SyncMode::Scheduled,
            ..FolderSettings::default()
        };
        assert_eq!(f.effective_sync_mode(), SyncMode::Manual);
    }

    #[test]
    fn collection_lenient() {
        let (c, issues) =
            CollectionSettings::from_json_lenient(&json!({"name": "  ", "search_model": "x"}));
        assert_eq!(c, CollectionSettings::default());
        assert_eq!(issues.len(), 2);
        let (c, issues) = CollectionSettings::from_json_lenient(
            &json!({"name": "Research", "search_model": "multilingual"}),
        );
        assert!(issues.is_empty());
        assert_eq!(c.search_model, SearchModel::Multilingual);
    }

    #[test]
    fn every_folder_field_has_a_tier() {
        let schema = serde_json::to_value(schemars::schema_for!(FolderSettings)).unwrap();
        let props = schema["properties"].as_object().unwrap();
        assert_eq!(props.len(), 15);
        let mut basic = Vec::new();
        for (k, v) in props {
            let tier = v["x-tier"]
                .as_str()
                .unwrap_or_else(|| panic!("{k} has no tier"));
            if tier == "basic" {
                basic.push(k.as_str());
            }
        }
        basic.sort_unstable();
        assert_eq!(
            basic,
            [
                "display_name",
                "file_types_preset",
                "include_online_only",
                "keep_up_to_date"
            ]
        );
    }
}
