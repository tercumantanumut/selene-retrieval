//! The one config schema (docs/02-architecture.md §2.5 "Config"; Product Principle P1.2/P1.6).
//!
//! * [`Settings`] is the global, app-written `settings.toml`. [`FolderSettings`] and
//!   [`CollectionSettings`] are stored in the catalog (one home per setting) but defined here.
//! * Every field has a working default; a missing file is created with defaults
//!   ([`load_or_create`]).
//! * Loading is lenient **per field**: a value of the wrong type or out of bounds falls back to its
//!   default, a warning is logged and the caller gets a `SettingsInvalid` notice ("Some settings
//!   were reset" + "Review settings"). The file never fails to load.
//! * Unknown keys are kept on rewrite (`extra` maps).
//! * Every leaf field carries `x-tier` (`basic` | `advanced`), `x-section` and `x-restart` in the
//!   JSON Schema; the UI generates the Basic page and the Advanced disclosure from it.

mod folder;
mod load;
pub mod policy;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

pub use folder::{CollectionSettings, FileTypesPreset, FolderSettings, SyncMode};
pub use load::{FieldIssue, LoadReport, SETTINGS_FILE, from_toml_str, load_or_create, save_atomic};

use crate::error::Section;

/// Unknown keys preserved on rewrite.
pub type Extra = BTreeMap<String, toml::Value>;

/// Settings tier used to generate the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldTier {
    Basic,
    Advanced,
}

// ---------------------------------------------------------------------------------------------
// Top level
// ---------------------------------------------------------------------------------------------

/// Global settings (`%LOCALAPPDATA%\SeleneRetrieval\settings.toml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Settings {
    /// Settings file format version.
    #[schemars(extend("x-tier" = "advanced", "x-section" = "system", "x-restart" = false))]
    pub version: u32,
    pub general: General,
    pub search: Search,
    pub answers: Answers,
    pub provider: Provider,
    pub indexing: Indexing,
    pub ranking: Ranking,
    pub network: Network,
    pub integrations: Integrations,
    pub system: System,
    #[serde(flatten)]
    #[schemars(skip)]
    pub extra: Extra,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            general: General::default(),
            search: Search::default(),
            answers: Answers::default(),
            provider: Provider::default(),
            indexing: Indexing::default(),
            ranking: Ranking::default(),
            network: Network::default(),
            integrations: Integrations::default(),
            system: System::default(),
            extra: Extra::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Basic sections
// ---------------------------------------------------------------------------------------------

/// General.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct General {
    /// Start with Windows (keeps folders up to date in the background).
    #[schemars(extend("x-tier" = "basic", "x-section" = "general", "x-restart" = false))]
    pub start_with_windows: bool,
    /// Show a notification when something needs attention.
    #[schemars(extend("x-tier" = "basic", "x-section" = "general", "x-restart" = false))]
    pub notify_on_problems: bool,
    /// Display language ("system" or a language tag such as "en" or "de-DE").
    #[schemars(extend("x-tier" = "basic", "x-section" = "general", "x-restart" = true))]
    pub language: String,
    #[serde(flatten)]
    #[schemars(skip)]
    pub extra: Extra,
}

impl Default for General {
    fn default() -> Self {
        Self {
            start_with_windows: true,
            notify_on_problems: true,
            language: "system".into(),
            extra: Extra::new(),
        }
    }
}

/// Which model turns text into searchable vectors.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum SearchModel {
    /// Bundled bge-small-en-v1.5 (local, no key, no network).
    #[default]
    Standard,
    /// multilingual-e5-small (local, one-click download).
    Multilingual,
    /// Remote embeddings from a configured provider (needs consent).
    Cloud(String),
}

impl fmt::Display for SearchModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchModel::Standard => f.write_str("standard"),
            SearchModel::Multilingual => f.write_str("multilingual"),
            SearchModel::Cloud(p) => write!(f, "cloud:{p}"),
        }
    }
}

impl From<SearchModel> for String {
    fn from(m: SearchModel) -> Self {
        m.to_string()
    }
}

impl TryFrom<String> for SearchModel {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.as_str() {
            "standard" => Ok(Self::Standard),
            "multilingual" => Ok(Self::Multilingual),
            _ => match s.strip_prefix("cloud:") {
                Some(p) if is_ident(p) => Ok(Self::Cloud(p.to_string())),
                _ => Err(format!("unknown search model {s:?}")),
            },
        }
    }
}

impl JsonSchema for SearchModel {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SearchModel".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": "^(standard|multilingual|cloud:[a-z0-9_-]{1,32})$" })
    }
}

fn is_ident(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Search.
///
/// `tier_override` and `use_graphics_card` are user **choices** (Amendment 4.10: they live here
/// and nowhere else); what the daemon detected (`hw_detection`, the recommended tier) lives in
/// catalog `meta`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Search {
    /// Search model.
    #[schemars(extend("x-tier" = "basic", "x-section" = "search", "x-restart" = false))]
    pub search_model: SearchModel,
    // (The doc comments below are the UI labels, so implementation notes use `//`.)
    // TOML: "auto" (default = None) or "basic" / "better" / "best"; the tier names "minimum" /
    // "standard" / "high" are accepted too. Anything else is reset to "auto" by the loader.
    /// Search quality. Automatic uses the best level this computer runs well; a higher level may
    /// index slowly, and search keeps working while it does.
    #[serde(with = "tier_choice")]
    #[schemars(schema_with = "tier_choice::schema")]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "search", "x-restart" = true))]
    pub tier_override: Option<crate::tier::Tier>,
    // TOML: true (Auto, default per Amendment 2) or false (Off); "auto" / "on" / "off" are
    // accepted too. M1 always plans the CPU (GPU detection lands in M2), but the embedder's
    // execution-provider choice already honours `false`.
    /// Use the graphics card to index faster. Turn this off if other apps slow down while
    /// indexing.
    #[serde(deserialize_with = "gpu_switch::deserialize")]
    #[schemars(schema_with = "gpu_switch::schema")]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "search", "x-restart" = true))]
    pub use_graphics_card: bool,
    #[serde(flatten)]
    #[schemars(skip)]
    pub extra: Extra,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            search_model: SearchModel::Standard,
            tier_override: None,
            use_graphics_card: true,
            extra: Extra::new(),
        }
    }
}

/// `search.tier_override` as user words: `auto` | `basic` | `better` | `best`.
mod tier_choice {
    use schemars::{Schema, SchemaGenerator, json_schema};
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::tier::Tier;

    pub const AUTO: &str = "auto";

    /// User word for a tier (the UI label, lowercased).
    pub fn word(t: Tier) -> &'static str {
        match t {
            Tier::Minimum => "basic",
            Tier::Standard => "better",
            Tier::High => "best",
        }
    }

    pub fn parse(s: &str) -> Result<Option<Tier>, String> {
        let s = s.trim().to_ascii_lowercase();
        if s == AUTO {
            return Ok(None);
        }
        Tier::ALL
            .into_iter()
            .find(|t| word(*t) == s || t.as_str() == s)
            .map(Some)
            .ok_or_else(|| format!("unknown search quality {s:?}"))
    }

    pub fn serialize<S: Serializer>(v: &Option<Tier>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(v.map_or(AUTO, word))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Tier>, D::Error> {
        let s = String::deserialize(d)?;
        parse(&s).map_err(serde::de::Error::custom)
    }

    pub fn schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "enum": ["auto", "basic", "better", "best"],
            "default": "auto",
            "x-enum-labels": {
                "auto": "Automatic (recommended)",
                "basic": "Basic",
                "better": "Better",
                "best": "Best"
            }
        })
    }
}

/// `search.use_graphics_card`: a bool (Auto/Off switch), lenient about the words.
mod gpu_switch {
    use schemars::{Schema, SchemaGenerator, json_schema};
    use serde::{Deserialize, Deserializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Bool(bool),
        Word(String),
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
        match Raw::deserialize(d)? {
            Raw::Bool(b) => Ok(b),
            Raw::Word(w) => match w.trim().to_ascii_lowercase().as_str() {
                "auto" | "on" => Ok(true),
                "off" => Ok(false),
                other => Err(serde::de::Error::custom(format!(
                    "unknown graphics card choice {other:?}"
                ))),
            },
        }
    }

    pub fn schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "boolean",
            "default": true,
            "x-enum-labels": { "true": "Auto", "false": "Off" }
        })
    }
}

/// Answers (visible once a provider exists).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Answers {
    /// Let connected apps ask for answers written from your files.
    #[schemars(extend("x-tier" = "basic", "x-section" = "answers", "x-restart" = false, "x-requires" = "provider"))]
    pub answers_enabled: bool,
    /// Deeper answers (uses more AI credits, slower).
    #[schemars(extend("x-tier" = "basic", "x-section" = "answers", "x-restart" = false, "x-requires" = "provider"))]
    pub deeper_answers: bool,
    /// Maximum extra search rounds for deeper answers (1-5).
    #[schemars(range(min = 1, max = 5))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "answers", "x-restart" = false))]
    pub max_rounds: u32,
    #[serde(flatten)]
    #[schemars(skip)]
    pub extra: Extra,
}

impl Default for Answers {
    fn default() -> Self {
        Self {
            answers_enabled: false,
            deeper_answers: false,
            max_rounds: 2,
            extra: Extra::new(),
        }
    }
}

/// AI provider (the key itself lives in Windows Credential Manager, never here).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Provider {
    /// Provider id ("openai", "gemini", "ollama", ...). Empty = none.
    #[schemars(extend("x-tier" = "basic", "x-section" = "providers", "x-restart" = false))]
    pub provider: Option<String>,
    /// Model used for answers (picked automatically when empty).
    #[schemars(extend("x-tier" = "basic", "x-section" = "providers", "x-restart" = false))]
    pub default_model: Option<String>,
    /// Model used for cloud search (`search_model = cloud:<provider>`). Empty = the provider's
    /// default embedding model (M3-P).
    #[schemars(extend("x-tier" = "advanced", "x-section" = "providers", "x-restart" = false, "x-requires" = "provider"))]
    pub embedding_model: Option<String>,
    #[serde(flatten)]
    #[schemars(skip)]
    pub extra: Extra,
}

// ---------------------------------------------------------------------------------------------
// Advanced sections
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChunkPreset {
    #[default]
    Balanced,
    Small,
    Large,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReindexPolicy {
    #[default]
    Smart,
    Always,
    Never,
}

/// Advanced: indexing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Indexing {
    /// Chunk size preset.
    #[schemars(extend("x-tier" = "advanced", "x-section" = "indexing", "x-restart" = false))]
    pub chunk_preset: ChunkPreset,
    /// Custom chunk size in tokens (64-8192); used when the preset is "custom".
    #[schemars(range(min = 64, max = 8192))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "indexing", "x-restart" = false))]
    pub chunk_size: Option<u32>,
    /// Custom chunk overlap in tokens (0-2048, smaller than the chunk size).
    #[schemars(range(min = 0, max = 2048))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "indexing", "x-restart" = false))]
    pub chunk_overlap: Option<u32>,
    /// Skip files larger than this many bytes (1 KiB-200 MiB).
    #[schemars(range(min = 1024, max = 209_715_200))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "indexing", "x-restart" = false))]
    pub max_file_bytes: u64,
    /// Extra glob patterns to exclude (added to the built-in excludes).
    #[schemars(extend("x-tier" = "advanced", "x-section" = "indexing", "x-restart" = false))]
    pub exclude_globs: Vec<String>,
    /// When to re-index changed files.
    #[schemars(extend("x-tier" = "advanced", "x-section" = "indexing", "x-restart" = false))]
    pub reindex_policy: ReindexPolicy,
    /// Include subfolders.
    #[schemars(extend("x-tier" = "advanced", "x-section" = "indexing", "x-restart" = false))]
    pub recursive: bool,
    /// Full re-check interval in minutes (5-1440).
    #[schemars(range(min = 5, max = 1440))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "indexing", "x-restart" = false))]
    pub reconcile_minutes: u32,
    /// Wait this long after a change before indexing, in ms (100-5000).
    #[schemars(range(min = 100, max = 5000))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "indexing", "x-restart" = false))]
    pub debounce_ms: u32,
    /// Follow symbolic links (never outside the folder).
    #[schemars(extend("x-tier" = "advanced", "x-section" = "indexing", "x-restart" = false))]
    pub follow_symlinks: bool,
    #[serde(flatten)]
    #[schemars(skip)]
    pub extra: Extra,
}

impl Default for Indexing {
    fn default() -> Self {
        Self {
            chunk_preset: ChunkPreset::Balanced,
            chunk_size: None,
            chunk_overlap: None,
            max_file_bytes: 10 * 1024 * 1024,
            exclude_globs: Vec::new(),
            reindex_policy: ReindexPolicy::Smart,
            recursive: true,
            reconcile_minutes: 60,
            debounce_ms: 500,
            follow_symlinks: false,
            extra: Extra::new(),
        }
    }
}

/// Advanced: ranking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Ranking {
    /// Reciprocal-rank-fusion constant (1-1000).
    #[schemars(range(min = 1, max = 1000))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "ranking", "x-restart" = false))]
    pub rrf_k: u32,
    /// Re-rank the top results with a local model (downloads the model).
    #[schemars(extend("x-tier" = "advanced", "x-section" = "ranking", "x-restart" = false))]
    pub rerank_enabled: bool,
    /// How many results to re-rank (1-200).
    #[schemars(range(min = 1, max = 200))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "ranking", "x-restart" = false))]
    pub rerank_top_n: u32,
    /// Diversity (MMR lambda, 0-1). Empty = off.
    #[schemars(range(min = 0.0, max = 1.0))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "ranking", "x-restart" = false))]
    pub mmr_lambda: Option<f32>,
    /// Default number of results (1-150).
    #[schemars(range(min = 1, max = 150))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "ranking", "x-restart" = false))]
    pub default_max_results: u32,
    /// Minimum relevance score (0-1).
    #[schemars(range(min = 0.0, max = 1.0))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "ranking", "x-restart" = false))]
    pub min_score: f32,
    /// Use only vector search (no keyword search).
    #[schemars(extend("x-tier" = "advanced", "x-section" = "ranking", "x-restart" = false))]
    pub dense_only: bool,
    #[serde(flatten)]
    #[schemars(skip)]
    pub extra: Extra,
}

impl Default for Ranking {
    fn default() -> Self {
        Self {
            rrf_k: 60,
            rerank_enabled: false,
            rerank_top_n: 50,
            mmr_lambda: None,
            default_max_results: 50,
            min_score: 0.1,
            dense_only: false,
            extra: Extra::new(),
        }
    }
}

/// Advanced: providers / network.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Network {
    /// Custom provider address (https, or http on this computer for Ollama).
    #[schemars(extend("x-tier" = "advanced", "x-section" = "providers", "x-restart" = false))]
    pub base_url: Option<String>,
    /// Request timeout in seconds (5-600).
    #[schemars(range(min = 5, max = 600))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "providers", "x-restart" = false))]
    pub request_timeout_secs: u32,
    /// Requests per minute per provider (1-10000). Empty = provider default.
    #[schemars(range(min = 1, max = 10000))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "providers", "x-restart" = false))]
    pub rate_limit_rpm: Option<u32>,
    /// Proxy override (http, https or socks5 URL). Empty = Windows proxy settings.
    #[schemars(extend("x-tier" = "advanced", "x-section" = "providers", "x-restart" = false))]
    pub proxy: Option<String>,
    /// Batch size for remote embeddings (1-2048).
    #[schemars(range(min = 1, max = 2048))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "providers", "x-restart" = false))]
    pub remote_embed_batch_size: u32,
    #[serde(flatten)]
    #[schemars(skip)]
    pub extra: Extra,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            base_url: None,
            request_timeout_secs: 60,
            rate_limit_rpm: None,
            proxy: None,
            remote_embed_batch_size: 64,
            extra: Extra::new(),
        }
    }
}

/// Advanced: integrations. Security-relevant switches are all off by default (P1.7).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Integrations {
    /// Allow other local tools to connect over HTTP (127.0.0.1 only, token required).
    /// M4-E: applies at once (`http.set_enabled` starts or stops the listener).
    #[schemars(extend("x-tier" = "advanced", "x-section" = "integrations", "x-restart" = false))]
    pub http_server_enabled: bool,
    /// HTTP port (1024-65535). Empty = pick a free port once and remember it.
    #[schemars(range(min = 1024, max = 65535))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "integrations", "x-restart" = true))]
    pub http_port: Option<u16>,
    /// Let connected apps ask to add folders (you always approve each request).
    #[schemars(extend("x-tier" = "advanced", "x-section" = "integrations", "x-restart" = false))]
    pub mcp_add_folder_enabled: bool,
    /// Let connected apps ask to check a folder for changes now (folders usually update on their
    /// own; one set to manual checks updates only when checked).
    #[schemars(extend("x-tier" = "advanced", "x-section" = "integrations", "x-restart" = false))]
    pub mcp_sync_folder_enabled: bool,
    #[serde(flatten)]
    #[schemars(skip)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

/// Advanced: system.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct System {
    /// Where the search index is stored. Empty = the default location.
    #[schemars(extend("x-tier" = "advanced", "x-section" = "system", "x-restart" = true))]
    pub data_dir: Option<PathBuf>,
    /// Log detail.
    #[schemars(extend("x-tier" = "advanced", "x-section" = "system", "x-restart" = false))]
    pub log_level: LogLevel,
    /// Keep old index versions for this many hours (1-168).
    #[schemars(range(min = 1, max = 168))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "system", "x-restart" = false))]
    pub retention_hours: u32,
    /// Indexing threads (1-64). Empty = automatic.
    #[schemars(range(min = 1, max = 64))]
    #[schemars(extend("x-tier" = "advanced", "x-section" = "system", "x-restart" = true))]
    pub indexing_threads: Option<u32>,
    #[serde(flatten)]
    #[schemars(skip)]
    pub extra: Extra,
}

impl Default for System {
    fn default() -> Self {
        Self {
            data_dir: None,
            log_level: LogLevel::Info,
            retention_hours: 1,
            indexing_threads: None,
            extra: Extra::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Validation (bounds + cross-field rules). The loader resets every flagged field to its default.
// ---------------------------------------------------------------------------------------------

fn in_range<T: PartialOrd>(v: &T, lo: T, hi: T) -> bool {
    *v >= lo && *v <= hi
}

fn is_loopback_host(host: &str) -> bool {
    let h = host.rsplit_once(':').map_or(host, |(h, p)| {
        if p.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            host
        }
    });
    matches!(h, "127.0.0.1" | "localhost" | "[::1]")
}

/// `https://…`, or `http://` to a loopback host only (Ollama).
pub fn is_valid_provider_url(s: &str) -> bool {
    let rest = if let Some(r) = s.strip_prefix("https://") {
        r
    } else if let Some(r) = s.strip_prefix("http://") {
        let host = r.split(['/', '?', '#']).next().unwrap_or("");
        if !is_loopback_host(host) {
            return false;
        }
        r
    } else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty() && !host.contains('@') && !s.chars().any(char::is_whitespace)
}

fn is_valid_proxy_url(s: &str) -> bool {
    ["http://", "https://", "socks5://", "socks5h://"]
        .iter()
        .any(|p| {
            s.strip_prefix(p)
                .is_some_and(|r| !r.is_empty() && !r.chars().any(char::is_whitespace))
        })
}

fn is_valid_language(s: &str) -> bool {
    if s == "system" {
        return true;
    }
    let mut parts = s.split('-');
    let first_ok = parts
        .next()
        .is_some_and(|p| (2..=3).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphabetic()));
    first_ok
        && parts.all(|p| (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

impl Settings {
    /// Every field whose value is out of bounds or breaks a cross-field rule. Called live by the
    /// UI and by the loader (which then resets those fields).
    pub fn validate(&self) -> Vec<FieldIssue> {
        let mut c = self.clone();
        c.sanitize()
    }

    /// Reset every invalid field to its default; returns what was reset.
    pub fn sanitize(&mut self) -> Vec<FieldIssue> {
        let d = Settings::default();
        let mut out = Vec::new();
        macro_rules! reset_if {
            ($cond:expr, $sec:ident . $f:ident, $section:expr, $why:expr) => {
                if $cond {
                    out.push(FieldIssue::new(
                        concat!(stringify!($sec), ".", stringify!($f)),
                        $section,
                        $why,
                    ));
                    self.$sec.$f = d.$sec.$f.clone();
                }
            };
        }
        macro_rules! range {
            ($sec:ident . $f:ident, $lo:expr, $hi:expr, $section:expr) => {
                reset_if!(
                    !in_range(&self.$sec.$f, $lo, $hi),
                    $sec.$f,
                    $section,
                    concat!(
                        "must be between ",
                        stringify!($lo),
                        " and ",
                        stringify!($hi)
                    )
                );
            };
        }
        macro_rules! opt_range {
            ($sec:ident . $f:ident, $lo:expr, $hi:expr, $section:expr) => {
                reset_if!(
                    self.$sec.$f.is_some_and(|v| !in_range(&v, $lo, $hi)),
                    $sec.$f,
                    $section,
                    concat!(
                        "must be empty or between ",
                        stringify!($lo),
                        " and ",
                        stringify!($hi)
                    )
                );
            };
        }
        use Section as S;

        reset_if!(
            !is_valid_language(&self.general.language),
            general.language,
            S::General,
            "not a language tag"
        );
        reset_if!(
            self.answers.max_rounds < 1 || self.answers.max_rounds > 5,
            answers.max_rounds,
            S::Answers,
            "must be between 1 and 5"
        );
        reset_if!(
            self.provider
                .provider
                .as_deref()
                .is_some_and(|p| !is_ident(p)),
            provider.provider,
            S::Providers,
            "not a provider id"
        );

        opt_range!(indexing.chunk_size, 64, 8192, S::Indexing);
        opt_range!(indexing.chunk_overlap, 0, 2048, S::Indexing);
        reset_if!(
            matches!((self.indexing.chunk_size, self.indexing.chunk_overlap), (Some(s), Some(o)) if o >= s),
            indexing.chunk_overlap,
            S::Indexing,
            "must be smaller than the chunk size"
        );
        range!(
            indexing.max_file_bytes,
            1024,
            200 * 1024 * 1024,
            S::Indexing
        );
        reset_if!(
            self.indexing
                .exclude_globs
                .iter()
                .any(|g| g.is_empty() || g.len() > 1024),
            indexing.exclude_globs,
            S::Indexing,
            "contains an empty or overlong pattern"
        );
        range!(indexing.reconcile_minutes, 5, 1440, S::Indexing);
        range!(indexing.debounce_ms, 100, 5000, S::Indexing);

        range!(ranking.rrf_k, 1, 1000, S::Ranking);
        range!(ranking.rerank_top_n, 1, 200, S::Ranking);
        opt_range!(ranking.mmr_lambda, 0.0, 1.0, S::Ranking);
        range!(ranking.default_max_results, 1, 150, S::Ranking);
        range!(ranking.min_score, 0.0, 1.0, S::Ranking);

        reset_if!(
            self.network
                .base_url
                .as_deref()
                .is_some_and(|u| !is_valid_provider_url(u)),
            network.base_url,
            S::Providers,
            "must be https (or http on this computer)"
        );
        range!(network.request_timeout_secs, 5, 600, S::Providers);
        opt_range!(network.rate_limit_rpm, 1, 10000, S::Providers);
        reset_if!(
            self.network
                .proxy
                .as_deref()
                .is_some_and(|u| !is_valid_proxy_url(u)),
            network.proxy,
            S::Providers,
            "must be an http, https or socks5 URL"
        );
        range!(network.remote_embed_batch_size, 1, 2048, S::Providers);

        opt_range!(integrations.http_port, 1024, 65535, S::Integrations);

        reset_if!(
            self.system
                .data_dir
                .as_ref()
                .is_some_and(|p| !p.is_absolute()),
            system.data_dir,
            S::System,
            "must be an absolute path"
        );
        range!(system.retention_hours, 1, 168, S::System);
        opt_range!(system.indexing_threads, 1, 64, S::System);
        out
    }

    /// JSON Schema of the settings (exported by xtask; drives the generated Basic/Advanced UI).
    pub fn json_schema() -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Settings)).unwrap_or_default()
    }

    /// `(dotted path, tier)` for every leaf field, read from the generated schema.
    pub fn field_tiers() -> Vec<(String, Option<FieldTier>)> {
        let schema = Self::json_schema();
        let defs = schema.get("$defs").cloned().unwrap_or_default();
        let mut out = Vec::new();
        let resolve = |v: &serde_json::Value| -> serde_json::Value {
            v.get("$ref")
                .and_then(|r| r.as_str())
                .and_then(|r| r.strip_prefix("#/$defs/"))
                .and_then(|n| defs.get(n))
                .cloned()
                .unwrap_or_else(|| v.clone())
        };
        let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
            return out;
        };
        for (k, v) in props {
            let target = resolve(v);
            match target.get("properties").and_then(|p| p.as_object()) {
                Some(inner) if v.get("$ref").is_some() => {
                    for (fk, fv) in inner {
                        out.push((format!("{k}.{fk}"), tier_of(fv)));
                    }
                }
                _ => out.push((k.clone(), tier_of(v))),
            }
        }
        out
    }
}

fn tier_of(v: &serde_json::Value) -> Option<FieldTier> {
    match v.get("x-tier").and_then(|t| t.as_str()) {
        Some("basic") => Some(FieldTier::Basic),
        Some("advanced") => Some(FieldTier::Advanced),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
