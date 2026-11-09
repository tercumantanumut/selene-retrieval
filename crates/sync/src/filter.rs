//! Which paths of a folder are cataloged: port of Selene's `lib/vectordb/ignore-patterns.ts`
//! (`DEFAULT_IGNORE_PATTERNS`, `createAggressiveIgnore`) plus the always-on sandbox policy.
//!
//! Order of checks for a root-relative path (`/`-separated, long names):
//! 1. **Sandbox** (`sr_common::sandbox`): every component passes
//!    [`validate_component`] (no `..`, ADS `:`, trailing dot/space, reserved device names by stem,
//!    control or wildcard characters) and the [`SecretFilter`] denylist. Cannot be turned off.
//! 2. **Selene defaults**: dependency/build/cache/venv directory names (`.env` only as a
//!    directory), well-known junk file names, junk globs, image/font asset directories and binary
//!    asset extensions that the include set does not ask for. The generic output names in
//!    [`CARGO_SOURCE_DIRS`] stay walkable inside a Cargo package's sources
//!    ([`FolderRules::cargo_source`]): Cargo writes only to `target/`, so there they are code.
//! 3. **User `exclude_globs`** with Selene's matcher semantics (no glob + no slash → any path
//!    segment; no glob + slash → sub-path; glob → `**/`-prefixed unless anchored with `/`).
//!    Case-insensitive, because Windows paths are.
//! 4. **Include set** from `file_types_preset` / `include_extensions` (`custom` with an empty list
//!    means "all files", as in Selene). **Code** also leaves data formats out (JSON that is not
//!    project configuration, Godot `.tres`: [`is_code_data_file`]); a `.seleneignore` `!pattern`
//!    keeps such a file.
//!
//! `.gitignore` / `.ignore` / `.seleneignore` files are honoured by the walker (the `ignore`
//! crate), not here. Step 2 (the built-in defaults, including the docs/06 change 5 noise rules:
//! generated, vendored and data-dump files) is lifted for a path that a `!pattern` in a
//! `.seleneignore` re-includes ([`crate::rules::FolderRules::reincluded`]); step 1 never is.
//! Both exceptions need the folder's [`FolderRules`]; without them (`check_dir` / `check_file`)
//! the defaults apply by name alone.

use std::collections::HashSet;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use sr_common::config::{FileTypesPreset, FolderSettings};
use sr_common::protocol::extract::DocKind;
use sr_common::sandbox::{SecretFilter, Violation, validate_component};

use crate::rules::FolderRules;

/// Selene `DEFAULT_IGNORED_DIRECTORY_NAMES` (matched exactly, like Selene).
pub const DEFAULT_IGNORED_DIRS: &[&str] = &[
    "node_modules",
    "bower_components",
    "jspm_packages",
    ".pnpm-store",
    ".yarn",
    "vendor",
    ".bundle",
    "Pods",
    ".dart_tool",
    ".git",
    ".vscode",
    ".idea",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".astro",
    ".docusaurus",
    ".output",
    ".vercel",
    ".netlify",
    "dist",
    "build",
    "out",
    "target",
    "coverage",
    "storybook-static",
    "dist-electron",
    "DerivedData",
    ".local-data",
    ".cache",
    ".parcel-cache",
    ".turbo",
    ".nx",
    ".angular",
    ".vite",
    ".gradle",
    "tmp",
    "temp",
    ".venv",
    "venv",
    "env",
    ".env",
    ".conda",
    ".direnv",
    "__pypackages__",
    "__pycache__",
    "site-packages",
    // docs/06 change 5: vendored upstream copies and test snapshots.
    "third_party",
    "third-party",
    "vendored",
    "__snapshots__",
    ".eggs",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".nox",
    ".hypothesis",
    ".ipynb_checkpoints",
    // 2026-11-09: Godot's editor caches (Godot 4 `.godot/`, Godot 3 `.import/`).
    ".godot",
    ".import",
];

/// Names in [`DEFAULT_IGNORED_DIRS`] that are code, not output, inside a Cargo package's sources
/// ([`FolderRules::cargo_source`]): build-script modules in `build/` next to `Cargo.toml`, and
/// modules such as `src/env/` or `src/build/`. Cargo itself writes only to `target/`.
pub const CARGO_SOURCE_DIRS: &[&str] = &[
    "build", "dist", "out", "target", "coverage", "tmp", "temp", "env",
];

/// Selene `DEFAULT_IGNORED_FILE_NAMES`.
pub const DEFAULT_IGNORED_FILES: &[&str] = &[
    ".DS_Store",
    "Thumbs.db",
    ".eslintcache",
    ".stylelintcache",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
];

/// Selene `DEFAULT_IGNORED_FILE_GLOBS`.
pub const DEFAULT_IGNORED_GLOBS: &[&str] = &[
    "*.tsbuildinfo",
    "*.log",
    "*.lock",
    "*.pyc",
    "*.pyo",
    "*.egg-info",
    // docs/06 change 5: generated / minified files and data dumps (never hand-written text).
    "*.min.js",
    "*.min.css",
    "*.map",
    "*.bundle.js",
    "*.chunk.js",
    "*.pb.go",
    "*_pb2.py",
    "*.snap",
    "*.jsonl",
    "*.ndjson",
    // 2026-11-09: the import settings and UID files Godot writes next to every asset and script.
    "*.import",
    "*.uid",
];

/// Version of the built-in defaults: the lists above, the data-file rule, the file types of the
/// presets (`DocKind::from_extension`) and [`is_code_data_file`]. Raising it makes the next start
/// rescan every folder once (`settings_change`), so newly ignored files are swept, newly included
/// ones are indexed and indexed long-line JSON files are checked again (docs/06 change 5).
/// 2: [`CARGO_SOURCE_DIRS`] inside Cargo sources (e.g. `crates/x/build/*.rs`). 3 (2026-11-09):
/// GDScript and Godot's text formats, the other code types the audit added, Godot's caches and
/// sidecars, and the Code preset's data rule.
pub const IGNORE_DEFAULTS_VERSION: u32 = 3;

/// `meta` key holding the [`IGNORE_DEFAULTS_VERSION`] the folders were last scanned with.
pub const META_IGNORE_DEFAULTS_VERSION: &str = "ignore_defaults_version";

/// Extensions the data-file rule looks at (docs/06 change 5).
pub const DATA_FILE_EXTS: &[&str] = &["json", "geojson"];

/// A `.json` / `.geojson` file whose lines average more than this many bytes is a data dump
/// (one record per line, minified exports), not a document: skipped as `data_file`.
pub const DATA_FILE_MIN_AVG_LINE: u64 = 1000;

/// The data-file rule over a file's size and line count (newline
/// count, plus one for an unterminated last line).
pub fn is_data_dump(ext: &str, bytes: u64, lines: u64) -> bool {
    DATA_FILE_EXTS.contains(&ext) && bytes / lines.max(1) > DATA_FILE_MIN_AVG_LINE
}

/// Data formats a folder set to **Code** leaves out (2026-11-09): JSON that is not project
/// configuration, and Godot `.tres` resources. Asset registries, reports, test dumps, animation
/// data and sprite frames are not code; in a Godot project they filled every result of a code
/// question. "Documents and code" and "All text files" still index them.
pub const CODE_DATA_EXTS: &[&str] = &["json", "tres"];

/// Well-known project configuration JSON (lower-case name without `.json`), kept by **Code**.
const CONFIG_JSON_NAMES: &[&str] = &[
    "package",
    "composer",
    "deno",
    "angular",
    "nx",
    "project",
    "turbo",
    "lerna",
    "vercel",
    "now",
    "netlify",
    "railway",
    "firebase",
    "app",
    "manifest",
    "global",
    "launch",
    "tasks",
    "extensions",
    "devcontainer",
    ".devcontainer",
    "renovate",
    "biome",
    "components",
    "mcp",
    "wrangler",
    "vcpkg",
    "cdk",
    "host",
    "function",
    "nodemon",
    "typedoc",
    "cspell",
    ".cspell",
    "tslint",
    "openapi",
    "swagger",
];

/// Words that make a JSON name configuration (`tsconfig.base.json`, `appsettings.Development.json`,
/// `user.schema.json`).
const CONFIG_JSON_WORDS: &[&str] = &["config", "settings", "schema"];

/// Directories whose JSON is configuration (`web/config/voice.json`, `.github/...`).
const CONFIG_JSON_DIRS: &[&str] = &[
    "config",
    "configs",
    "conf",
    "settings",
    ".config",
    ".github",
    ".devcontainer",
    ".vscode",
];

/// A file a folder set to **Code** leaves out as data ([`CODE_DATA_EXTS`]). JSON stays when it
/// looks like project configuration: a well-known name (`package.json`, `vercel.json`, ...), a
/// name with `config`, `settings` or `schema` in it, a dot-file ending in `rc`
/// (`.eslintrc.json`), or a file in a `config` / `settings` directory. `dirs` are the path's
/// directories, `name` its file name, `ext` its lower-case extension.
pub fn is_code_data_file(dirs: &[&str], name: &str, ext: &str) -> bool {
    if !CODE_DATA_EXTS.contains(&ext) {
        return false;
    }
    if ext != "json" {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".json").unwrap_or(&lower);
    let config = CONFIG_JSON_NAMES.contains(&stem)
        || CONFIG_JSON_WORDS.iter().any(|w| stem.contains(w))
        || (stem.len() > 3 && stem.starts_with('.') && stem.ends_with("rc"))
        || dirs
            .iter()
            .any(|d| CONFIG_JSON_DIRS.contains(&d.to_ascii_lowercase().as_str()));
    !config
}

const IMAGE_EXTS: &[&str] = &[
    "avif", "bmp", "cur", "gif", "heic", "heif", "ico", "jpeg", "jpg", "png", "psd", "svg", "tif",
    "tiff", "webp",
];
const FONT_EXTS: &[&str] = &["eot", "otf", "ttf", "woff", "woff2"];
const MEDIA_EXTS: &[&str] = &[
    "aac", "avi", "flac", "m4a", "mkv", "mov", "mp3", "mp4", "ogg", "wav", "webm",
];
const ARCHIVE_EXTS: &[&str] = &["7z", "gz", "pdf", "rar", "tar", "zip"];
const IMAGE_DIRS: &[&str] = &[
    "icons",
    "image",
    "images",
    "img",
    "imgs",
    "photos",
    "screenshots",
];
const FONT_DIRS: &[&str] = &["font", "fonts", "typefaces"];
const ASSET_CONTAINER_DIRS: &[&str] = &["assets", "docs", "media", "public", "resources", "static"];
/// Executables and other binaries `all_text` never includes (the worker would sniff them out
/// anyway; skipping them avoids reading them at all).
const BINARY_EXTS: &[&str] = &[
    "exe",
    "dll",
    "so",
    "dylib",
    "bin",
    "obj",
    "o",
    "a",
    "lib",
    "pdb",
    "class",
    "jar",
    "wasm",
    "iso",
    "dmg",
    "msi",
    "db",
    "sqlite",
    "sqlite3",
    "pak",
    "dat",
    "idx",
    "pack",
    "onnx",
    "pt",
    "safetensors",
    "npy",
    "npz",
    "parquet",
    "bz2",
    "xz",
    "zst",
    "cab",
    "apk",
    "ipa",
];

/// Why a path is not cataloged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Excluded {
    /// Refused by the sandbox (traversal, ADS, device name, trailing dot/space, ...).
    Rejected(Violation),
    /// On the secret denylist (`.env`, `*.pem`, `.ssh/`, ...).
    Secret,
    /// Selene default ignore (dependency/build/cache dirs, junk files, assets).
    Default,
    /// The folder's `exclude_globs`.
    User,
    /// An ignore file (`.gitignore`, `.ignore`, `.seleneignore`; watcher events, docs/06 change 5).
    Ignored,
    /// Extension not in the folder's file-type set.
    NotIncluded,
}

/// Effective include set of a folder.
#[derive(Debug, Clone)]
enum Include {
    /// `custom` with an empty list: every file (Selene: empty filter = all files).
    All,
    /// Supported document/code kinds (preset), by kind predicate.
    Kinds(fn(DocKind) -> bool),
    /// Any extension except known binaries (`all_text`), extensionless files included.
    AllText,
    /// Exactly these extensions (`custom`).
    Exts(HashSet<String>),
}

fn is_document(k: DocKind) -> bool {
    matches!(
        k,
        DocKind::Text
            | DocKind::Markdown
            | DocKind::Html
            | DocKind::Pdf
            | DocKind::Docx
            | DocKind::Xlsx
            | DocKind::Pptx
    )
}

fn is_code(k: DocKind) -> bool {
    k == DocKind::Code
}

fn is_any_kind(_: DocKind) -> bool {
    true
}

/// One compiled user pattern (Selene `createIgnoreMatcher` semantics).
#[derive(Debug, Clone)]
enum UserPattern {
    /// No glob, no slash: any path segment equals it.
    Segment(String),
    /// No glob, with slash: these consecutive segments appear in the path.
    Subpath(Vec<String>),
    /// No glob, leading `/`: the path starts with these segments (Selene's regex never matched
    /// a leading slash; anchoring is what users mean, as in gitignore).
    Anchored(Vec<String>),
}

/// Lower-case extension (without the dot) of a file name; `""` if none.
pub fn extension_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Compiled filter of one folder (cheap to share; built per scan).
#[derive(Debug, Clone)]
pub struct PathFilter {
    secrets: SecretFilter,
    include: Include,
    /// The folder is set to **Code**: data formats stay out ([`is_code_data_file`]).
    code_only: bool,
    /// Selene's asset pruning is off when the include set is "all files".
    prune_images: bool,
    prune_fonts: bool,
    default_globs: GlobSet,
    user_globs: GlobSet,
    /// `foo/**`-style user globs as directory patterns (prune the directory itself).
    user_dir_globs: GlobSet,
    user_patterns: Vec<UserPattern>,
}

fn glob(p: &str) -> Option<globset::Glob> {
    GlobBuilder::new(p)
        .literal_separator(true)
        .case_insensitive(true)
        .backslash_escape(false)
        .build()
        .ok()
}

/// Selene `normalizePattern` + backslash → slash.
fn normalize_pattern(raw: &str) -> String {
    let mut p = raw.trim().replace('\\', "/");
    if let Some(s) = p.strip_prefix("./") {
        p = s.to_string();
    }
    while p.ends_with('/') {
        p.pop();
    }
    p
}

/// Selene `globToRegex` anchoring: `**/`-prefixed unless anchored with a leading `/`.
fn anchored_glob(p: &str) -> String {
    if let Some(rest) = p.strip_prefix('/') {
        rest.to_string()
    } else if p.starts_with("**/") {
        p.to_string()
    } else {
        format!("**/{p}")
    }
}

impl PathFilter {
    /// Filter for a folder's settings.
    pub fn new(s: &FolderSettings) -> Self {
        let include = match s.file_types_preset {
            FileTypesPreset::Documents => Include::Kinds(is_document),
            FileTypesPreset::Code => Include::Kinds(is_code),
            FileTypesPreset::DocumentsAndCode => Include::Kinds(is_any_kind),
            FileTypesPreset::AllText => Include::AllText,
            FileTypesPreset::Custom if s.include_extensions.is_empty() => Include::All,
            FileTypesPreset::Custom => Include::Exts(
                s.include_extensions
                    .iter()
                    .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
                    .collect(),
            ),
        };
        let (prune_images, prune_fonts) = match &include {
            Include::All => (false, false),
            Include::Exts(set) => (
                !IMAGE_EXTS.iter().any(|e| set.contains(*e)),
                !FONT_EXTS.iter().any(|e| set.contains(*e)),
            ),
            Include::Kinds(_) | Include::AllText => (true, true),
        };
        let mut defaults = GlobSetBuilder::new();
        for g in DEFAULT_IGNORED_GLOBS {
            if let Some(g) = glob(&anchored_glob(g)) {
                defaults.add(g);
            }
        }
        let mut user = GlobSetBuilder::new();
        let mut user_dirs = GlobSetBuilder::new();
        let mut user_patterns = Vec::new();
        for raw in &s.exclude_globs {
            let p = normalize_pattern(raw);
            if p.is_empty() {
                continue;
            }
            let has_glob = p.contains(['*', '?', '[', '{']);
            if !has_glob {
                let segs: Vec<String> = p
                    .trim_start_matches('/')
                    .split('/')
                    .filter(|x| !x.is_empty())
                    .map(str::to_lowercase)
                    .collect();
                match segs.len() {
                    0 => {}
                    _ if p.starts_with('/') => user_patterns.push(UserPattern::Anchored(segs)),
                    1 if !p.contains('/') => user_patterns.push(UserPattern::Segment(
                        segs.into_iter().next().unwrap_or_default(),
                    )),
                    _ => user_patterns.push(UserPattern::Subpath(segs)),
                }
                continue;
            }
            let a = anchored_glob(&p);
            if let Some(g) = glob(&a) {
                user.add(g);
            }
            if let Some(dir) = a.strip_suffix("/**")
                && let Some(g) = glob(dir)
            {
                user_dirs.add(g);
            }
        }
        Self {
            secrets: SecretFilter::default(),
            include,
            code_only: s.file_types_preset == FileTypesPreset::Code,
            prune_images,
            prune_fonts,
            default_globs: defaults.build().unwrap_or_else(|_| GlobSet::empty()),
            user_globs: user.build().unwrap_or_else(|_| GlobSet::empty()),
            user_dir_globs: user_dirs.build().unwrap_or_else(|_| GlobSet::empty()),
            user_patterns,
        }
    }

    /// Sandbox policy for every component (always on).
    fn sandbox_check(&self, comps: &[&str]) -> Result<(), Excluded> {
        for c in comps {
            validate_component(c).map_err(Excluded::Rejected)?;
            if self.secrets.is_denied_name(c) {
                return Err(Excluded::Secret);
            }
        }
        Ok(())
    }

    fn user_segments_match(&self, comps: &[&str]) -> bool {
        let lower: Vec<String> = comps.iter().map(|c| c.to_lowercase()).collect();
        self.user_patterns.iter().any(|p| match p {
            UserPattern::Segment(s) => lower.iter().any(|c| c == s),
            UserPattern::Subpath(segs) => lower.windows(segs.len()).any(|w| w == segs.as_slice()),
            UserPattern::Anchored(segs) => lower.starts_with(segs),
        })
    }

    /// Selene `isExcludedAssetDirectory` for component `i` of the directory chain `lower`.
    fn asset_dir_at(&self, lower: &[String], i: usize) -> bool {
        let seg = lower[i].as_str();
        let asset = (self.prune_images && IMAGE_DIRS.contains(&seg))
            || (self.prune_fonts && FONT_DIRS.contains(&seg));
        asset
            && (i == 0
                || lower[..i]
                    .iter()
                    .any(|p| ASSET_CONTAINER_DIRS.contains(&p.as_str())))
    }

    /// A directory of the chain `dirs` is a built-in default ignore (dependency/build/cache or
    /// asset directory) that no `.seleneignore` re-includes and that is not part of a Cargo
    /// package's sources.
    fn default_dir(&self, dirs: &[&str], rules: Option<&FolderRules>) -> bool {
        let lower: Vec<String> = dirs.iter().map(|c| c.to_lowercase()).collect();
        (0..dirs.len()).any(|i| {
            (DEFAULT_IGNORED_DIRS.contains(&dirs[i]) || self.asset_dir_at(&lower, i))
                && !rules.is_some_and(|r| {
                    let rel = dirs[..=i].join("/");
                    r.reincluded(&rel, true)
                        || (CARGO_SOURCE_DIRS.contains(&dirs[i]) && r.cargo_source(&rel))
                })
        })
    }

    /// Directory `rel` (root-relative, `/`-separated): descend into it?
    pub fn check_dir(&self, rel: &str) -> Result<(), Excluded> {
        self.check_dir_with(rel, None)
    }

    /// [`Self::check_dir`] with the folder's ignore files: a `.seleneignore` `!pattern` lifts the
    /// built-in defaults (never the sandbox or the secret denylist).
    pub fn check_dir_with(&self, rel: &str, rules: Option<&FolderRules>) -> Result<(), Excluded> {
        let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
        self.sandbox_check(&comps)?;
        if self.default_dir(&comps, rules) {
            return Err(Excluded::Default);
        }
        if self.user_segments_match(&comps)
            || self.user_globs.is_match(rel)
            || self.user_dir_globs.is_match(rel)
        {
            return Err(Excluded::User);
        }
        Ok(())
    }

    /// File `rel` (root-relative, `/`-separated): catalog it?
    pub fn check_file(&self, rel: &str) -> Result<(), Excluded> {
        self.check_file_with(rel, None)
    }

    /// [`Self::check_file`] with the folder's ignore files (see [`Self::check_dir_with`]).
    pub fn check_file_with(&self, rel: &str, rules: Option<&FolderRules>) -> Result<(), Excluded> {
        let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
        let Some((name, dirs)) = comps.split_last() else {
            return Err(Excluded::Rejected(Violation::Empty));
        };
        self.sandbox_check(&comps)?;
        // `.env` is only ignored as a directory (it is commonly a file; the denylist covers it).
        if self.default_dir(dirs, rules) {
            return Err(Excluded::Default);
        }
        let ext = extension_of(name);
        if (DEFAULT_IGNORED_FILES.contains(name)
            || self.default_globs.is_match(rel)
            || self.binary_asset(&ext))
            && !rules.is_some_and(|r| r.reincluded(rel, false))
        {
            return Err(Excluded::Default);
        }
        if self.user_segments_match(&comps)
            || self.user_globs.is_match(rel)
            || self.user_dir_globs.is_match(rel)
        {
            return Err(Excluded::User);
        }
        if !self.includes_ext(&ext) {
            return Err(Excluded::NotIncluded);
        }
        if self.code_only
            && is_code_data_file(dirs, name, &ext)
            && !rules.is_some_and(|r| r.reincluded(rel, false))
        {
            return Err(Excluded::NotIncluded);
        }
        Ok(())
    }

    /// Selene `isBinaryAssetPath`: a known binary asset extension not explicitly included.
    fn binary_asset(&self, ext: &str) -> bool {
        let known = IMAGE_EXTS.contains(&ext)
            || FONT_EXTS.contains(&ext)
            || MEDIA_EXTS.contains(&ext)
            || ARCHIVE_EXTS.contains(&ext);
        if !known {
            return false;
        }
        match &self.include {
            Include::All => false,
            Include::Exts(set) => !set.contains(ext),
            // Presets: PDFs are documents; everything else in the asset list is binary.
            Include::Kinds(f) => !DocKind::from_extension(ext).is_some_and(f),
            Include::AllText => ext != "pdf",
        }
    }

    fn includes_ext(&self, ext: &str) -> bool {
        match &self.include {
            Include::All => true,
            Include::Exts(set) => set.contains(ext),
            Include::Kinds(f) => DocKind::from_extension(ext).is_some_and(f),
            Include::AllText => !BINARY_EXTS.contains(&ext),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(preset: FileTypesPreset, exts: &[&str], globs: &[&str]) -> PathFilter {
        PathFilter::new(&FolderSettings {
            file_types_preset: preset,
            include_extensions: exts.iter().map(|s| (*s).to_string()).collect(),
            exclude_globs: globs.iter().map(|s| (*s).to_string()).collect(),
            ..FolderSettings::default()
        })
    }

    #[test]
    fn selene_default_ignores() {
        let p = f(FileTypesPreset::DocumentsAndCode, &[], &[]);
        for d in [
            "node_modules",
            "a/node_modules",
            ".git",
            "src/target",
            "x/__pycache__",
            "public/images",
            "assets/fonts",
        ] {
            assert_eq!(p.check_dir(d), Err(Excluded::Default), "{d}");
        }
        for d in ["src", "docs", "src/icons", "Node", "environment"] {
            assert_eq!(p.check_dir(d), Ok(()), "{d}");
        }
        for file in [
            "a/node_modules/x.js",
            "package-lock.json",
            "sub/yarn.lock",
            "x/Thumbs.db",
            "build.log",
            "deep/a/b.pyc",
            "logo.png",
            "media/clip.mp4",
            "fonts/a.ttf",
        ] {
            assert_eq!(p.check_file(file), Err(Excluded::Default), "{file}");
        }
        for file in ["README.md", "src/main.rs", "docs/a.pdf", "env.md"] {
            assert_eq!(p.check_file(file), Ok(()), "{file}");
        }
    }

    /// Inside a Cargo package's sources `build`, `env`, `out`, ... are code; next to the manifest
    /// (`dist`, `target`) and outside Cargo packages they stay default ignores.
    #[test]
    fn cargo_sources_keep_output_like_directory_names() {
        let t = tempfile::tempdir().unwrap();
        for (p, body) in [
            ("Cargo.toml", "[workspace]\n"),
            ("crates/common/Cargo.toml", "[package]\n"),
            ("web/package.json", "{}"),
        ] {
            let p = t.path().join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let rules = FolderRules::new(t.path());
        let r = Some(&rules);
        let p = f(FileTypesPreset::DocumentsAndCode, &[], &[]);
        for d in [
            "crates/common/build",
            "crates/common/src/env",
            "crates/common/src/build",
        ] {
            assert_eq!(p.check_dir_with(d, r), Ok(()), "{d}");
            assert_eq!(p.check_dir(d), Err(Excluded::Default), "{d}: no rules");
        }
        for file in [
            "crates/common/build/static_crt.rs",
            "crates/common/src/env/mod.rs",
            "crates/common/src/out/gen.rs",
        ] {
            assert_eq!(p.check_file_with(file, r), Ok(()), "{file}");
        }
        for d in [
            "target",
            "dist",
            "crates/common/target",
            "crates/common/src/node_modules",
            "crates/common/src/vendor",
            "web/build",
            "web/dist",
        ] {
            assert_eq!(p.check_dir_with(d, r), Err(Excluded::Default), "{d}");
        }
        for file in [
            "crates/common/target/debug/build/x/out/private.rs",
            "crates/common/build/bundle.min.js",
            "web/build/app.js",
        ] {
            assert_eq!(p.check_file_with(file, r), Err(Excluded::Default), "{file}");
        }
    }

    #[test]
    fn sandbox_rules_always_apply() {
        let p = f(FileTypesPreset::Custom, &[], &[]);
        assert_eq!(p.check_file(".env"), Err(Excluded::Secret));
        assert_eq!(p.check_file("certs/server.pem"), Err(Excluded::Secret));
        assert_eq!(p.check_file(".ssh/config.md"), Err(Excluded::Secret));
        assert_eq!(p.check_dir(".aws"), Err(Excluded::Secret));
        assert_eq!(
            p.check_dir(".env"),
            Err(Excluded::Secret),
            "denylist before defaults"
        );
        assert_eq!(
            p.check_file("a.txt:secret"),
            Err(Excluded::Rejected(Violation::AlternateDataStream))
        );
        assert_eq!(
            p.check_file("con.txt"),
            Err(Excluded::Rejected(Violation::ReservedName))
        );
        assert_eq!(
            p.check_file("dir/notes."),
            Err(Excluded::Rejected(Violation::TrailingDotOrSpace))
        );
        assert_eq!(
            p.check_file("a/../b.md"),
            Err(Excluded::Rejected(Violation::ParentTraversal))
        );
        assert_eq!(
            p.check_dir("LPT1"),
            Err(Excluded::Rejected(Violation::ReservedName))
        );
        assert_eq!(p.check_file("anything.bin"), Ok(()), "custom+empty = all");
    }

    #[test]
    fn presets_and_custom_extensions() {
        let docs = f(FileTypesPreset::Documents, &[], &[]);
        assert_eq!(docs.check_file("a.docx"), Ok(()));
        assert_eq!(docs.check_file("a.rs"), Err(Excluded::NotIncluded));
        let code = f(FileTypesPreset::Code, &[], &[]);
        assert_eq!(code.check_file("a.rs"), Ok(()));
        assert_eq!(code.check_file("a.pdf"), Err(Excluded::Default));
        let all = f(FileTypesPreset::AllText, &[], &[]);
        assert_eq!(all.check_file("Makefile"), Ok(()));
        assert_eq!(all.check_file("notes.weird"), Ok(()));
        assert_eq!(all.check_file("app.exe"), Err(Excluded::NotIncluded));
        let custom = f(FileTypesPreset::Custom, &["md", "PNG"], &[]);
        assert_eq!(custom.check_file("a.md"), Ok(()));
        assert_eq!(
            custom.check_file("b.png"),
            Ok(()),
            "explicitly included asset"
        );
        assert_eq!(custom.check_dir("images"), Ok(()), "images not pruned");
        assert_eq!(custom.check_dir("fonts"), Err(Excluded::Default));
        assert_eq!(custom.check_file("a.txt"), Err(Excluded::NotIncluded));
    }

    /// 2026-11-09: a Godot project. GDScript, shaders, scenes and `project.godot` are code;
    /// Godot's caches and sidecars are default ignores in every preset.
    #[test]
    fn godot_projects_are_code_without_their_caches() {
        for preset in [FileTypesPreset::Code, FileTypesPreset::DocumentsAndCode] {
            let p = f(preset, &[], &[]);
            for file in [
                "game/src/temple_maps.gd",
                "game/shaders/water.gdshader",
                "game/scenes/temple.tscn",
                "game/project.godot",
            ] {
                assert_eq!(p.check_file(file), Ok(()), "{preset:?} {file}");
            }
            assert_eq!(p.check_dir("game/.godot"), Err(Excluded::Default));
            assert_eq!(p.check_dir(".import"), Err(Excluded::Default));
            for file in [
                "game/.godot/editor/cached.gd",
                "game/icon.png.import",
                "game/src/temple_maps.gd.uid",
            ] {
                assert_eq!(p.check_file(file), Err(Excluded::Default), "{file}");
            }
        }
        let docs = f(FileTypesPreset::Documents, &[], &[]);
        assert_eq!(docs.check_file("a.gd"), Err(Excluded::NotIncluded));
        let all = f(FileTypesPreset::AllText, &[], &[]);
        assert_eq!(all.check_file("icon.png.import"), Err(Excluded::Default));
    }

    /// 2026-11-09: **Code** keeps project-configuration JSON and leaves data JSON and `.tres`
    /// resources out; "Documents and code" keeps all of them; `.seleneignore` `!` keeps one.
    #[test]
    fn code_preset_leaves_data_json_and_resources_out() {
        let code = f(FileTypesPreset::Code, &[], &[]);
        let both = f(FileTypesPreset::DocumentsAndCode, &[], &[]);
        for file in [
            "package.json",
            "web/vercel.json",
            "tsconfig.base.json",
            "src/appsettings.Development.json",
            ".eslintrc.json",
            "api/user.schema.json",
            "web/config/voice.json",
            ".github/renovate.json",
            "a.jsonc",
        ] {
            assert_eq!(code.check_file(file), Ok(()), "{file}");
        }
        for file in [
            "art/med/rooms/registry.json",
            "art/characters/her/anim/pour/clip.json",
            "game/test/coop/data/temple_runs.json",
            "src.json",
            ".rc.json",
            "game/ui/theme.tres",
        ] {
            assert_eq!(code.check_file(file), Err(Excluded::NotIncluded), "{file}");
            assert_eq!(both.check_file(file), Ok(()), "{file}");
        }
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join(".seleneignore"), "!art/keep.json\n").unwrap();
        let rules = FolderRules::new(t.path());
        assert_eq!(code.check_file_with("art/keep.json", Some(&rules)), Ok(()));
        assert_eq!(
            code.check_file_with("art/other.json", Some(&rules)),
            Err(Excluded::NotIncluded)
        );
    }

    #[test]
    fn user_globs_use_selene_semantics() {
        let p = f(
            FileTypesPreset::DocumentsAndCode,
            &[],
            &[
                "drafts",
                "docs/old",
                "*.tmp.md",
                "/top.md",
                "gen/**",
                r"win\style",
            ],
        );
        assert_eq!(p.check_dir("a/Drafts"), Err(Excluded::User));
        assert_eq!(p.check_file("x/drafts/a.md"), Err(Excluded::User));
        assert_eq!(p.check_file("docs/old/a.md"), Err(Excluded::User));
        assert_eq!(p.check_file("z/docs/old/a.md"), Err(Excluded::User));
        assert_eq!(p.check_file("docs/older/a.md"), Ok(()));
        assert_eq!(p.check_file("q/w.tmp.md"), Err(Excluded::User));
        assert_eq!(p.check_file("top.md"), Err(Excluded::User));
        assert_eq!(p.check_file("sub/top.md"), Ok(()), "anchored");
        assert_eq!(p.check_dir("gen"), Err(Excluded::User));
        assert_eq!(p.check_dir("a/gen"), Err(Excluded::User));
        assert_eq!(p.check_file("win/style/a.md"), Err(Excluded::User));
        assert_eq!(extension_of(".gitignore"), "");
        assert_eq!(extension_of("A.MD"), "md");
    }
}
