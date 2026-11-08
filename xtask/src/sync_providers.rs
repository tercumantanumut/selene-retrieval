//! `cargo xtask sync-providers [--check] [--selene <dir>] [--out <file>]` (M3-P).
//!
//! Selene's provider registry is the **single source** of the provider list (docs/research/
//! m3-design-spec-intake.md). This task reads four Selene files **read-only**:
//!
//! * `lib/ai/provider-types.ts`: the `LLMProvider` string-literal union (ids, in Selene's order);
//! * `lib/ai/providers.ts`: `DEFAULT_MODELS`, `UTILITY_MODELS` and the display names in
//!   `getProviderDisplayName()`;
//! * `lib/auth/claudecode-models.ts`, `lib/auth/codex-models.ts`: the model lists Selene offers
//!   for its Claude Code and Codex sign-in providers, with Selene's labels (Amendment 11, owner
//!   request 2026-10-19). They become the `models` list of the `claudecode` / `codex` rows (the
//!   UI's model picker); Codex ids Selene marks "(Legacy)" or only maps to another model are left
//!   out;
//!
//! joins them with our **policy table** ([`POLICY`]: auth kind, wire protocol, base URL, model
//! discovery, key-validation method, embeddings; facts Selene keeps in other files or that are our
//! own decisions) and writes `crates/providers/data/selene-providers.json`. `sr-providers` embeds
//! that file (`include_str!`) and the UI reads the same file, so neither hardcodes a provider or a
//! model string.
//!
//! * A provider added in Selene shows up after a re-run. Without a policy row it is listed but
//!   disabled (`disabled_reason = "not_supported_yet"`) until a row is added here.
//! * Account sign-in: `claudecode` ("Claude (sign in)", Amendment 10.2) and `codex` ("Codex (sign
//!   in)", Amendment 10.3) are **enabled** with protocol `cli`: requests run through the user's own
//!   Claude Code / Codex CLI and its login; no address, no key, no token here. Antigravity is API
//!   key only and disabled (`"not_allowed_by_vendor"`) — [`DISABLED`].
//! * `--check` regenerates in memory and fails if the committed file differs by a single byte
//!   (CI drift guard). Nothing is written in check mode.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Selene source files (relative to the Selene repo root). Never written.
pub const TYPES_TS: &str = "lib/ai/provider-types.ts";
pub const PROVIDERS_TS: &str = "lib/ai/providers.ts";
pub const CLAUDECODE_MODELS_TS: &str = "lib/auth/claudecode-models.ts";
pub const CODEX_MODELS_TS: &str = "lib/auth/codex-models.ts";
/// Every Selene file read, in `source_files` order.
pub const SOURCE_FILES: [&str; 4] = [
    TYPES_TS,
    PROVIDERS_TS,
    CLAUDECODE_MODELS_TS,
    CODEX_MODELS_TS,
];
/// Output, relative to the selene-retrieval project root.
pub const OUT_REL: &str = "crates/providers/data/selene-providers.json";

/// Policy rows that are listed but not selectable, with the reason the UI shows (M5-S /
/// Amendment 10.1). Every other policy row is enabled.
const DISABLED: &[(&str, &str)] = &[("antigravity", "not_allowed_by_vendor")];

/// Display order (intake): Claude, Codex first, then Kimi, DeepSeek, OpenRouter, then the rest in
/// Selene's union order.
const ORDER_FIRST: &[&str] = &["anthropic", "codex", "kimi", "deepseek", "openrouter"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// API key pasted by the user, stored in Windows Credential Manager.
    ApiKey,
    /// OAuth / subscription login (needs a design pass; disabled, C2).
    Oauth,
    /// A server on this computer (Ollama, vLLM); a key is optional.
    NoneLocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// Anthropic Messages API.
    Anthropic,
    /// OpenAI Chat Completions / Embeddings / Models compatible API.
    OpenaiCompat,
    /// Provider-specific OAuth client (not implemented).
    Oauth,
    /// The user's own CLI runs the request with its own login (`claude -p`, `codex exec`).
    Cli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Discovery {
    /// Models are listed live from the provider (`GET {base}models`).
    Live,
    /// The registry's default and utility models are offered as hints ("e.g. …").
    Registry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Validation {
    /// `GET {base}models` with the key (401/403 = rejected). Costs nothing.
    Models,
    /// OpenRouter's `GET {base}key` (its `/models` is public, so it can't check a key).
    OpenrouterKey,
    /// One-token chat with the utility model (the provider has no free authenticated endpoint
    /// we can rely on). Costs at most one output token.
    ChatProbe,
    /// The CLI's own status command (`claude auth status`, `codex login status`). Sends nothing.
    CliStatus,
}

/// Which Selene model list a provider row offers in its model picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelSource {
    /// `CLAUDECODE_MODEL_IDS` + `MODEL_LABELS` ([`CLAUDECODE_MODELS_TS`]).
    ClaudeCode,
    /// `CODEX_MODEL_IDS` + `MODEL_MAP` + `BASE_MODEL_LABELS` ([`CODEX_MODELS_TS`]).
    Codex,
}

/// Our policy for one Selene provider id.
struct Policy {
    id: &'static str,
    /// Overrides Selene's display name (intake: anthropic is shown as "Claude").
    display_name: Option<&'static str>,
    auth: AuthKind,
    protocol: Protocol,
    /// Trailing slash required (genai appends `chat/completions`, `messages`, `embeddings`).
    base_url: &'static str,
    /// Models served from another base URL (Selene: Kimi `k3` uses the Kimi Code endpoint).
    base_url_by_model: &'static [(&'static str, &'static str)],
    discovery: Discovery,
    validation: Validation,
    /// Default remote embedding model (None = this provider is not offered for search).
    embedding_model: Option<&'static str>,
    /// For providers whose embedding model must be chosen from the live list (Ollama, vLLM).
    embeddings_live: bool,
    /// The provider's own page for getting an API key (local providers: their download page).
    /// Shown as "Where do I get a key?" on page 21 (Amendment 8.3, R-UX38); `https://` only.
    key_url: Option<&'static str>,
    /// The Selene model list offered in the model picker (None = default / utility hints only).
    models: Option<ModelSource>,
    /// SR's own (default, utility) model, replacing Selene's `DEFAULT_MODELS` / `UTILITY_MODELS`
    /// for this row. Both must be in the row's model list ([`verify`]).
    defaults: Option<(&'static str, &'static str)>,
}

/// Facts not in the two registry files. Base URLs mirror Selene's clients
/// (`lib/ai/providers/*-client.ts`, `lib/auth/*-models.ts`) as of 2026-10-19; the embedding
/// default for OpenRouter mirrors `lib/ai/providers/embedding-provider.ts`.
const POLICY: &[Policy] = &[
    Policy {
        id: "anthropic",
        display_name: Some("Claude"),
        auth: AuthKind::ApiKey,
        protocol: Protocol::Anthropic,
        base_url: "https://api.anthropic.com/v1/",
        base_url_by_model: &[],
        discovery: Discovery::Registry,
        validation: Validation::Models,
        embedding_model: None,
        embeddings_live: false,
        key_url: Some("https://console.anthropic.com/settings/keys"),
        models: None,
        defaults: None,
    },
    Policy {
        id: "openrouter",
        display_name: None,
        auth: AuthKind::ApiKey,
        protocol: Protocol::OpenaiCompat,
        base_url: "https://openrouter.ai/api/v1/",
        base_url_by_model: &[],
        discovery: Discovery::Live,
        validation: Validation::OpenrouterKey,
        embedding_model: Some("qwen/qwen3-embedding-4b"),
        embeddings_live: false,
        key_url: Some("https://openrouter.ai/settings/keys"),
        models: None,
        defaults: None,
    },
    Policy {
        id: "kimi",
        display_name: None,
        auth: AuthKind::ApiKey,
        protocol: Protocol::OpenaiCompat,
        base_url: "https://api.moonshot.ai/v1/",
        base_url_by_model: &[("k3", "https://api.kimi.com/coding/v1/")],
        discovery: Discovery::Registry,
        validation: Validation::Models,
        embedding_model: None,
        embeddings_live: false,
        key_url: Some("https://platform.moonshot.ai/console/api-keys"),
        models: None,
        defaults: None,
    },
    Policy {
        id: "deepseek",
        display_name: None,
        auth: AuthKind::ApiKey,
        protocol: Protocol::OpenaiCompat,
        base_url: "https://api.deepseek.com/",
        base_url_by_model: &[],
        discovery: Discovery::Registry,
        validation: Validation::Models,
        embedding_model: None,
        embeddings_live: false,
        key_url: Some("https://platform.deepseek.com/api_keys"),
        models: None,
        defaults: None,
    },
    Policy {
        id: "minimax",
        display_name: None,
        auth: AuthKind::ApiKey,
        protocol: Protocol::OpenaiCompat,
        base_url: "https://api.minimax.chat/v1/",
        base_url_by_model: &[],
        discovery: Discovery::Registry,
        validation: Validation::ChatProbe,
        embedding_model: None,
        embeddings_live: false,
        key_url: Some("https://platform.minimax.io/user-center/basic-information/interface-key"),
        models: None,
        defaults: None,
    },
    Policy {
        id: "blackboxai",
        display_name: None,
        auth: AuthKind::ApiKey,
        protocol: Protocol::OpenaiCompat,
        base_url: "https://api.blackbox.ai/",
        base_url_by_model: &[],
        discovery: Discovery::Registry,
        validation: Validation::ChatProbe,
        embedding_model: None,
        embeddings_live: false,
        key_url: Some("https://docs.blackbox.ai/api-reference/authentication"),
        models: None,
        defaults: None,
    },
    Policy {
        id: "ollama",
        display_name: None,
        auth: AuthKind::NoneLocal,
        protocol: Protocol::OpenaiCompat,
        base_url: "http://localhost:11434/v1/",
        base_url_by_model: &[],
        discovery: Discovery::Live,
        validation: Validation::Models,
        embedding_model: None,
        embeddings_live: true,
        key_url: Some("https://ollama.com/download"),
        models: None,
        defaults: None,
    },
    Policy {
        id: "vllm",
        display_name: None,
        auth: AuthKind::NoneLocal,
        protocol: Protocol::OpenaiCompat,
        base_url: "http://localhost:8000/v1/",
        base_url_by_model: &[],
        discovery: Discovery::Live,
        validation: Validation::Models,
        embedding_model: None,
        embeddings_live: true,
        key_url: Some("https://docs.vllm.ai/en/latest/getting_started/installation/"),
        models: None,
        defaults: None,
    },
    Policy {
        // Amendment 10.3: the user's own Codex CLI and its login (`codex exec`).
        id: "codex",
        display_name: Some("Codex (sign in)"),
        auth: AuthKind::Oauth,
        protocol: Protocol::Cli,
        base_url: "",
        base_url_by_model: &[],
        discovery: Discovery::Registry,
        validation: Validation::CliStatus,
        embedding_model: None,
        embeddings_live: false,
        key_url: Some("https://developers.openai.com/codex/cli"),
        // Selene's Codex list (`lib/auth/codex-models.ts`), legacy ids left out.
        models: Some(ModelSource::Codex),
        // SR policy (owner request 2026-10-19, Amendment 11): Selene's DEFAULT_MODELS still say
        // `gpt-5.4` / `gpt-5.4-low`. GPT-5.5 is the newest model we know Codex CLI 0.160 runs
        // with a ChatGPT login; GPT-6 Astra / GPT-5.6 stay in the picker but are not the default
        // until a CLI that accepts them is verified. Saved choices are never changed.
        defaults: Some(("gpt-5.5", "gpt-5.4-mini-low")),
    },
    Policy {
        // Amendment 10.2: the user's own Claude Code CLI and its login (`claude -p`).
        id: "claudecode",
        display_name: Some("Claude (sign in)"),
        auth: AuthKind::Oauth,
        protocol: Protocol::Cli,
        base_url: "",
        base_url_by_model: &[],
        discovery: Discovery::Registry,
        validation: Validation::CliStatus,
        embedding_model: None,
        embeddings_live: false,
        key_url: Some("https://claude.com/claude-code"),
        // Selene's Claude Code list (`lib/auth/claudecode-models.ts`).
        models: Some(ModelSource::ClaudeCode),
        // SR policy (owner request 2026-10-19, Amendment 11): Selene's DEFAULT_MODELS still say
        // `claude-sonnet-4-6`; use the newest Sonnet and the newest Haiku that Selene lists.
        // Saved choices are never changed.
        defaults: Some(("claude-sonnet-5-5", "claude-haiku-4-5-20251001")),
    },
    Policy {
        // M5-S / Amendment 10.1: API key only (no subscription sign-in); listed, disabled.
        id: "antigravity",
        display_name: None,
        auth: AuthKind::ApiKey,
        protocol: Protocol::Oauth,
        base_url: "",
        base_url_by_model: &[],
        discovery: Discovery::Registry,
        validation: Validation::Models,
        embedding_model: None,
        embeddings_live: false,
        key_url: None,
        models: None,
        defaults: None,
    },
];

/// One entry of a provider's model picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelRow {
    pub id: String,
    /// Selene's label, e.g. "Claude Sonnet 5.5", "GPT-6 Astra (Max)".
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct Embeddings {
    /// Default embedding model; `None` when it has to be picked from the live list.
    pub default_model: Option<String>,
    pub discovery: Discovery,
}

/// One manifest row. Field order is the JSON order.
#[derive(Debug, Serialize)]
pub struct ProviderRow {
    pub id: String,
    pub display_name: String,
    pub order: u32,
    pub enabled: bool,
    /// `oauth_pending_design` | `not_supported_yet` (None when enabled).
    pub disabled_reason: Option<&'static str>,
    pub auth_kind: Option<AuthKind>,
    pub protocol: Option<Protocol>,
    pub base_url: Option<String>,
    pub base_url_by_model: BTreeMap<String, String>,
    /// Selene `DEFAULT_MODELS[id]`, or the policy row's `defaults` (None when empty, e.g. vLLM:
    /// discovered).
    pub default_model: Option<String>,
    /// Selene `UTILITY_MODELS[id]`.
    pub utility_model: Option<String>,
    /// The model picker's list (CLI sign-in rows; omitted when empty).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelRow>,
    pub discovery: Discovery,
    pub validation: Option<Validation>,
    pub key_optional: bool,
    pub embeddings: Option<Embeddings>,
    /// Page 21 "Where do I get a key?" (None for disabled providers).
    pub key_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Manifest {
    /// Bump on a breaking change of the row shape.
    pub schema: u32,
    pub generated_by: &'static str,
    pub source_files: [&'static str; 4],
    pub providers: Vec<ProviderRow>,
}

/// What the Selene files say.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Registry {
    pub ids: Vec<String>,
    pub default_models: BTreeMap<String, String>,
    pub utility_models: BTreeMap<String, String>,
    pub display_names: BTreeMap<String, String>,
    /// Claude Code models (empty when the file wasn't read, e.g. in unit tests).
    pub claudecode_models: Vec<ModelRow>,
    /// Codex models, legacy ids left out.
    pub codex_models: Vec<ModelRow>,
}

// ----------------------------------------------------------------------------------- parsing

/// Ids of `export type LLMProvider = | "a" | "b" ...;` in source order.
pub fn parse_union(src: &str) -> Result<Vec<String>, String> {
    let start = src
        .find("export type LLMProvider")
        .ok_or("provider-types.ts: `export type LLMProvider` not found")?;
    let rest = &src[start..];
    let eq = rest.find('=').ok_or("LLMProvider: missing `=`")?;
    let end = rest[eq..].find(';').ok_or("LLMProvider: missing `;`")? + eq;
    let ids = string_literals(&rest[eq + 1..end]);
    if ids.is_empty() {
        return Err("LLMProvider union is empty".into());
    }
    check_ids(&ids)?;
    Ok(ids)
}

/// `export const NAME: Record<LLMProvider, string> = { key: "value", ... };`
pub fn parse_record(src: &str, name: &str) -> Result<BTreeMap<String, String>, String> {
    parse_object(src, &format!("export const {name}"), name)
}

/// `{head}: Record<…> = { key: "value", ... };` (keys bare or quoted, line comments ignored).
fn parse_object(src: &str, head: &str, name: &str) -> Result<BTreeMap<String, String>, String> {
    let start = src
        .find(head)
        .ok_or_else(|| format!("`{head}` not found"))?;
    let rest = &src[start..];
    let open = rest
        .find('{')
        .ok_or_else(|| format!("{name}: missing `{{`"))?;
    let close = rest[open..]
        .find("};")
        .ok_or_else(|| format!("{name}: missing `}};`"))?
        + open;
    let mut out = BTreeMap::new();
    for raw in rest[open + 1..close].lines() {
        let line = strip_line_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let (key, val) = line
            .split_once(':')
            .ok_or_else(|| format!("{name}: can't read line {raw:?}"))?;
        let key = key.trim().trim_matches('"').trim_matches('\'').to_string();
        let lits = string_literals(val);
        let [v] = lits.as_slice() else {
            return Err(format!(
                "{name}: expected one string literal for {key:?}, got {raw:?}"
            ));
        };
        if out.insert(key.clone(), v.clone()).is_some() {
            return Err(format!("{name}: duplicate key {key:?}"));
        }
    }
    Ok(out)
}

/// Display names from `getProviderDisplayName()`: `case "x":` lines followed by
/// `` return `Name (${model})` ``; `default:` is attached to the cases it shares a return with.
pub fn parse_display_names(src: &str) -> Result<BTreeMap<String, String>, String> {
    let start = src
        .find("export function getProviderDisplayName")
        .ok_or("providers.ts: `getProviderDisplayName` not found")?;
    let body = &src[start..];
    let end = body
        .find("\n}")
        .ok_or("getProviderDisplayName: missing end")?;
    let mut out = BTreeMap::new();
    let mut pending: Vec<String> = Vec::new();
    for raw in body[..end].lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("case ") {
            if let Some(id) = string_literals(rest).into_iter().next() {
                pending.push(id);
            }
        } else if let Some(rest) = line.strip_prefix("return `") {
            let name = rest.split(" (").next().unwrap_or("").trim_end_matches('`');
            if name.is_empty() {
                return Err(format!("getProviderDisplayName: can't read {raw:?}"));
            }
            for id in pending.drain(..) {
                out.insert(id, name.to_string());
            }
        }
    }
    Ok(out)
}

/// String literals of `const NAME = [ ... ]` in source order (line comments ignored).
fn parse_array(src: &str, name: &str) -> Result<Vec<String>, String> {
    let head = format!("const {name}");
    let start = src
        .find(&head)
        .ok_or_else(|| format!("`{head}` not found"))?;
    let rest = &src[start..];
    let open = rest
        .find('[')
        .ok_or_else(|| format!("{name}: missing `[`"))?;
    let close = rest[open..]
        .find(']')
        .ok_or_else(|| format!("{name}: missing `]`"))?
        + open;
    let ids: Vec<String> = rest[open + 1..close]
        .lines()
        .flat_map(|l| string_literals(strip_line_comment(l)))
        .collect();
    if ids.is_empty() {
        return Err(format!("{name} is empty"));
    }
    let mut seen = std::collections::BTreeSet::new();
    if let Some(d) = ids.iter().find(|i| !seen.insert(i.as_str())) {
        return Err(format!("{name}: duplicate {d:?}"));
    }
    Ok(ids)
}

/// `case "x":` … `return "Label";` pairs (and `if (modelId === "x") {` … `return "Label";`)
/// inside `function {func}`.
fn parse_returns(src: &str, func: &str) -> Result<BTreeMap<String, String>, String> {
    let start = src
        .find(&format!("function {func}"))
        .ok_or_else(|| format!("`function {func}` not found"))?;
    let body = &src[start..];
    let end = body
        .find("\n}")
        .ok_or_else(|| format!("{func}: missing end"))?;
    let mut out = BTreeMap::new();
    let mut pending: Vec<String> = Vec::new();
    for raw in body[..end].lines() {
        let line = raw.trim();
        if let Some(rest) = line
            .strip_prefix("case ")
            .or_else(|| line.strip_prefix("if (modelId === "))
        {
            pending.extend(string_literals(rest).into_iter().take(1));
        } else if line.starts_with("return ") {
            if let Some(v) = string_literals(line).into_iter().next() {
                for k in pending.drain(..) {
                    out.insert(k, v.clone());
                }
            }
            pending.clear();
        }
    }
    Ok(out)
}

/// `claudecode-models.ts`: every id with its `MODEL_LABELS` label (the id when unlabelled).
pub fn parse_claudecode_models(src: &str) -> Result<Vec<ModelRow>, String> {
    let ids = parse_array(src, "CLAUDECODE_MODEL_IDS")?;
    let labels = parse_object(src, "const MODEL_LABELS", "MODEL_LABELS")?;
    Ok(ids
        .into_iter()
        .map(|id| ModelRow {
            name: labels.get(&id).cloned().unwrap_or_else(|| id.clone()),
            id,
        })
        .collect())
}

/// `-effort` suffix of `id` (one of `efforts`), with the id before it.
fn strip_effort<'a>(id: &'a str, efforts: &[&str]) -> Option<(&'a str, String)> {
    efforts.iter().find_map(|e| {
        id.strip_suffix(&format!("-{e}"))
            .filter(|b| !b.is_empty())
            .map(|b| (b, (*e).to_string()))
    })
}

/// The model `codex -m` gets for `id`, exactly as `sr_providers::cli::codex::split_model` splits
/// it: a listed id that has its own effort variants listed (`gpt-5.1-codex-max` next to
/// `gpt-5.1-codex-max-low`) is a model name and never split.
fn codex_base<'a>(id: &'a str, listed: &[&str], efforts: &[&str]) -> &'a str {
    let has_variants = listed.iter().any(|l| {
        l.strip_prefix(id)
            .and_then(|r| r.strip_prefix('-'))
            .is_some_and(|e| efforts.contains(&e))
    });
    if has_variants {
        return id;
    }
    strip_effort(id, efforts).map_or(id, |(b, _)| b)
}

/// `codex-models.ts`: the ids in Selene's order with Selene's labels ("GPT-6 Astra (Max)").
/// Left out: ids Selene labels "(Legacy)" and ids whose `MODEL_MAP` model differs from what
/// `codex -m` would get (aliases such as `gpt-5.1-chat-latest`); the model itself is listed.
pub fn parse_codex_models(src: &str) -> Result<Vec<ModelRow>, String> {
    let ids = parse_array(src, "CODEX_MODEL_IDS")?;
    let map = parse_object(src, "const MODEL_MAP", "MODEL_MAP")?;
    let labels = parse_object(src, "const BASE_MODEL_LABELS", "BASE_MODEL_LABELS")?;
    let suffix_labels = parse_returns(src, "formatReasoningSuffix")?;
    let exceptions = parse_returns(src, "getCodexModelDisplayName")?;
    if suffix_labels.is_empty() {
        return Err("formatReasoningSuffix: no effort levels found".into());
    }
    let efforts: Vec<&str> = suffix_labels.keys().map(String::as_str).collect();
    let all: Vec<&str> = ids.iter().map(String::as_str).collect();
    let name = |id: &str| -> String {
        if let Some(n) = exceptions.get(id) {
            return n.clone();
        }
        // An exact label first: `gpt-5.1-codex-max` is "GPT-5.1 Codex Max", not "… Codex (Max)".
        if let Some(l) = labels.get(id) {
            return l.clone();
        }
        match strip_effort(id, &efforts) {
            Some((b, e)) => format!(
                "{} ({})",
                labels.get(b).map_or(b, String::as_str),
                suffix_labels.get(&e).map_or(e.as_str(), String::as_str)
            ),
            None => id.to_string(),
        }
    };
    let mut out = Vec::new();
    for id in &ids {
        let n = name(id);
        let base = codex_base(id, &all, &efforts);
        if n.contains("(Legacy)") || map.get(id).map(String::as_str) != Some(base) {
            continue;
        }
        out.push(ModelRow {
            id: id.clone(),
            name: n,
        });
    }
    if out.is_empty() {
        return Err("CODEX_MODEL_IDS: no current models found".into());
    }
    Ok(out)
}

pub fn parse_registry(types_ts: &str, providers_ts: &str) -> Result<Registry, String> {
    let reg = Registry {
        ids: parse_union(types_ts)?,
        default_models: parse_record(providers_ts, "DEFAULT_MODELS")?,
        utility_models: parse_record(providers_ts, "UTILITY_MODELS")?,
        display_names: parse_display_names(providers_ts)?,
        claudecode_models: Vec::new(),
        codex_models: Vec::new(),
    };
    // Selene's own records are `Record<LLMProvider, string>`: exactly one entry per id.
    for (what, map) in [
        ("DEFAULT_MODELS", &reg.default_models),
        ("UTILITY_MODELS", &reg.utility_models),
    ] {
        for id in &reg.ids {
            if !map.contains_key(id) {
                return Err(format!("{what} has no entry for provider {id:?}"));
            }
        }
        if let Some(extra) = map.keys().find(|k| !reg.ids.contains(k)) {
            return Err(format!("{what} has {extra:?}, which is not an LLMProvider"));
        }
    }
    Ok(reg)
}

fn strip_line_comment(line: &str) -> &str {
    // Model ids never contain `//` (they are inside quotes anyway; we cut outside quotes only).
    let mut in_str: Option<char> = None;
    let b = line.as_bytes();
    for (i, c) in line.char_indices() {
        match in_str {
            Some(q) if c == q => in_str = None,
            Some(_) => {}
            None if c == '"' || c == '\'' || c == '`' => in_str = Some(c),
            None if c == '/' && b.get(i + 1) == Some(&b'/') => return &line[..i],
            None => {}
        }
    }
    line
}

/// Every `"…"` / `'…'` literal in `s` (no escapes in provider ids or model ids).
fn string_literals(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.char_indices();
    while let Some((_, c)) = chars.next() {
        if c == '"' || c == '\'' {
            let mut lit = String::new();
            for (_, d) in chars.by_ref() {
                if d == c {
                    break;
                }
                lit.push(d);
            }
            out.push(lit);
        }
    }
    out
}

fn check_ids(ids: &[String]) -> Result<(), String> {
    for id in ids {
        let ok = (1..=32).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if !ok {
            return Err(format!(
                "provider id {id:?} doesn't fit settings.toml ([a-z0-9_-]{{1,32}})"
            ));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    if let Some(d) = ids.iter().find(|i| !seen.insert(i.as_str())) {
        return Err(format!("duplicate provider id {d:?}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------- building

fn non_empty(s: Option<&String>) -> Option<String> {
    s.filter(|s| !s.is_empty()).cloned()
}

pub fn build_manifest(reg: &Registry) -> Manifest {
    let mut ordered: Vec<&String> = ORDER_FIRST
        .iter()
        .filter_map(|f| reg.ids.iter().find(|i| i.as_str() == *f))
        .collect();
    ordered.extend(
        reg.ids
            .iter()
            .filter(|i| !ORDER_FIRST.contains(&i.as_str())),
    );

    let providers = ordered
        .into_iter()
        .enumerate()
        .map(|(n, id)| {
            let policy = POLICY.iter().find(|p| p.id == id.as_str());
            let selene_name = reg
                .display_names
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.clone());
            let display_name = policy
                .and_then(|p| p.display_name)
                .map_or(selene_name, str::to_string);
            let default_model = non_empty(reg.default_models.get(id));
            let utility_model = non_empty(reg.utility_models.get(id));
            match policy {
                Some(p) => {
                    let (default_model, utility_model) = match p.defaults {
                        Some((d, u)) => (Some(d.to_string()), Some(u.to_string())),
                        None => (default_model, utility_model),
                    };
                    let models = match p.models {
                        Some(ModelSource::ClaudeCode) => reg.claudecode_models.clone(),
                        Some(ModelSource::Codex) => reg.codex_models.clone(),
                        None => Vec::new(),
                    };
                    let disabled = DISABLED
                        .iter()
                        .find(|(d, _)| *d == id.as_str())
                        .map(|(_, why)| *why)
                        .or_else(|| {
                            (p.base_url.is_empty() && p.protocol != Protocol::Cli)
                                .then_some("oauth_pending_design")
                        });
                    let off = disabled.is_some();
                    ProviderRow {
                        id: id.clone(),
                        display_name,
                        order: n as u32,
                        enabled: !off,
                        disabled_reason: disabled,
                        auth_kind: Some(p.auth),
                        protocol: Some(p.protocol),
                        base_url: (!p.base_url.is_empty()).then(|| p.base_url.to_string()),
                        base_url_by_model: p
                            .base_url_by_model
                            .iter()
                            .map(|(m, u)| ((*m).to_string(), (*u).to_string()))
                            .collect(),
                        default_model,
                        utility_model,
                        models,
                        discovery: p.discovery,
                        validation: (!off).then_some(p.validation),
                        key_optional: p.auth == AuthKind::NoneLocal,
                        embeddings: if p.embedding_model.is_some() || p.embeddings_live {
                            Some(Embeddings {
                                default_model: p.embedding_model.map(str::to_string),
                                discovery: if p.embeddings_live {
                                    Discovery::Live
                                } else {
                                    Discovery::Registry
                                },
                            })
                        } else {
                            None
                        },
                        key_url: p.key_url.filter(|_| !off).map(str::to_string),
                    }
                }
                None => ProviderRow {
                    id: id.clone(),
                    display_name,
                    order: n as u32,
                    enabled: false,
                    disabled_reason: Some("not_supported_yet"),
                    auth_kind: None,
                    protocol: None,
                    base_url: None,
                    base_url_by_model: BTreeMap::new(),
                    default_model,
                    utility_model,
                    models: Vec::new(),
                    discovery: Discovery::Registry,
                    validation: None,
                    key_optional: false,
                    embeddings: None,
                    key_url: None,
                },
            }
        })
        .collect();

    Manifest {
        schema: 1,
        generated_by: "cargo xtask sync-providers (do not edit by hand)",
        source_files: SOURCE_FILES,
        providers,
    }
}

pub fn render(m: &Manifest) -> Result<String, String> {
    serde_json::to_string_pretty(m)
        .map(|s| s + "\n")
        .map_err(|e| e.to_string())
}

/// A row with a model list offers its own default and utility model (else the picker couldn't
/// show the automatic choice). Fails the run, so a Selene list change can't ship a broken default.
pub fn verify(m: &Manifest) -> Result<(), String> {
    for p in m.providers.iter().filter(|p| !p.models.is_empty()) {
        for want in [&p.default_model, &p.utility_model].into_iter().flatten() {
            if !p.models.iter().any(|x| &x.id == want) {
                return Err(format!(
                    "{}: model {want:?} is not in Selene's model list; update the policy row's \
                     `defaults` in xtask/src/sync_providers.rs",
                    p.id
                ));
            }
        }
    }
    Ok(())
}

/// Reads and parses every Selene source file under `selene`.
pub fn load(selene: &Path) -> Result<Registry, String> {
    let read = |rel: &str| {
        let p = selene.join(rel);
        std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))
    };
    let mut reg = parse_registry(&read(TYPES_TS)?, &read(PROVIDERS_TS)?)?;
    reg.claudecode_models = parse_claudecode_models(&read(CLAUDECODE_MODELS_TS)?)?;
    reg.codex_models = parse_codex_models(&read(CODEX_MODELS_TS)?)?;
    Ok(reg)
}

/// Ids in the policy table that Selene no longer has (warned, not fatal: the row is unused).
pub fn stale_policy(reg: &Registry) -> Vec<&'static str> {
    POLICY
        .iter()
        .map(|p| p.id)
        .filter(|id| !reg.ids.iter().any(|i| i == id))
        .collect()
}

// --------------------------------------------------------------------------------------- cli

fn project_root() -> PathBuf {
    // xtask/ → project root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Selene is a separate repository, checked out next to this one (`../selene`).
fn default_selene_root() -> PathBuf {
    let p = project_root();
    p.parent()
        .map_or_else(|| PathBuf::from("selene"), |parent| parent.join("selene"))
}

pub fn main(args: &[String]) -> Result<(), String> {
    let mut check = false;
    let mut selene = default_selene_root();
    let mut out = project_root().join(OUT_REL);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => check = true,
            "--selene" => selene = it.next().ok_or("--selene needs a directory")?.into(),
            "--out" => out = it.next().ok_or("--out needs a file")?.into(),
            other => return Err(format!("sync-providers: unknown argument {other:?}")),
        }
    }
    if !selene.join(TYPES_TS).is_file() {
        return Err(format!(
            "sync-providers: Selene's sources are not at {} (expected a Selene checkout next to \
             this repository); pass --selene <dir>",
            selene.display()
        ));
    }
    let reg = load(&selene)?;
    for id in stale_policy(&reg) {
        eprintln!("sync-providers: note: policy row {id:?} is no longer in Selene's registry");
    }
    let manifest = build_manifest(&reg);
    verify(&manifest)?;
    for p in manifest
        .providers
        .iter()
        .filter(|p| p.disabled_reason == Some("not_supported_yet"))
    {
        eprintln!(
            "sync-providers: note: {:?} is new in Selene and listed as disabled until a policy row \
             is added in xtask/src/sync_providers.rs",
            p.id
        );
    }
    let body = render(&manifest)?;
    if check {
        let current = std::fs::read_to_string(&out).unwrap_or_default();
        // Normalise line endings: a CRLF checkout must not count as drift.
        if current.replace("\r\n", "\n") != body {
            return Err(format!(
                "{} is out of date with Selene's provider registry ({}). \
                 Run `cargo xtask sync-providers` and commit the result.",
                out.display(),
                SOURCE_FILES.join(", ")
            ));
        }
        println!(
            "sync-providers: {} is up to date ({} providers)",
            out.display(),
            manifest.providers.len()
        );
        return Ok(());
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&out, &body).map_err(|e| format!("{}: {e}", out.display()))?;
    println!(
        "sync-providers: wrote {} ({} providers)",
        out.display(),
        manifest.providers.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPES: &str = r#"
export type LLMProvider =
  | "anthropic"
  | "codex"
  | "vllm"
  | "newco";
const X = new Set<LLMProvider>(["deepseek"]);
"#;

    const PROVIDERS: &str = r#"
export const DEFAULT_MODELS: Record<LLMProvider, string> = {
  anthropic: "claude-sonnet-4-6",
  codex: "gpt-5.4", // OAuth
  vllm: "", // Discovered from /v1/models at runtime
  newco: "newco-1",
};

export const UTILITY_MODELS: Record<LLMProvider, string> = {
  anthropic: "claude-haiku-4-5-20251001",
  codex: "gpt-5.4-low",
  vllm: "",
  newco: "newco-mini",
};

export function getProviderDisplayName(): string {
  switch (provider) {
    case "codex":
      return `Codex (${model})`;
    case "vllm":
      return `vLLM (${model})`;
    case "anthropic":
    default:
      return `Anthropic (${model})`;
  }
}
"#;

    #[test]
    fn parses_union_records_and_names() {
        let reg = parse_registry(TYPES, PROVIDERS).unwrap_or_default();
        assert_eq!(reg.ids, ["anthropic", "codex", "vllm", "newco"]);
        assert_eq!(reg.default_models["codex"], "gpt-5.4");
        assert_eq!(reg.default_models["vllm"], "");
        assert_eq!(reg.utility_models["newco"], "newco-mini");
        assert_eq!(reg.display_names["anthropic"], "Anthropic");
        assert_eq!(reg.display_names["vllm"], "vLLM");
    }

    #[test]
    fn manifest_orders_and_flags() {
        let reg = parse_registry(TYPES, PROVIDERS).unwrap_or_default();
        let m = build_manifest(&reg);
        let ids: Vec<_> = m.providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["anthropic", "codex", "vllm", "newco"]);
        let by = |id: &str| m.providers.iter().find(|p| p.id == id);
        let a = by("anthropic");
        assert_eq!(a.map(|p| p.display_name.as_str()), Some("Claude"));
        assert_eq!(a.map(|p| p.enabled), Some(true));
        // Amendment 10.3: Codex reuses the local Codex CLI login (protocol `cli`, no address).
        let c = by("codex");
        assert_eq!(c.and_then(|p| p.disabled_reason), None);
        assert_eq!(c.map(|p| p.enabled), Some(true));
        assert_eq!(c.map(|p| p.display_name.as_str()), Some("Codex (sign in)"));
        assert_eq!(c.and_then(|p| p.protocol), Some(Protocol::Cli));
        assert_eq!(c.and_then(|p| p.base_url.clone()), None);
        assert_eq!(c.and_then(|p| p.validation), Some(Validation::CliStatus));
        assert!(c.and_then(|p| p.key_url.clone()).is_some());
        let v = by("vllm");
        assert_eq!(v.and_then(|p| p.default_model.clone()), None);
        assert_eq!(v.map(|p| p.key_optional), Some(true));
        let n = by("newco");
        assert_eq!(n.map(|p| p.enabled), Some(false));
        assert_eq!(n.and_then(|p| p.disabled_reason), Some("not_supported_yet"));
        assert_eq!(n.map(|p| p.display_name.as_str()), Some("newco"));
    }

    #[test]
    fn missing_record_entry_fails() {
        let bad = PROVIDERS.replace("  newco: \"newco-1\",\n", "");
        assert!(parse_registry(TYPES, &bad).is_err());
    }

    #[test]
    fn rejects_bad_ids() {
        assert!(parse_union("export type LLMProvider = | \"Bad Id\";").is_err());
        assert!(parse_union("export type LLMProvider = | \"a\" | \"a\";").is_err());
    }

    #[test]
    fn comment_with_slashes_is_cut_outside_quotes_only() {
        assert_eq!(strip_line_comment("  a: \"x//y\", // c"), "  a: \"x//y\", ");
    }

    const CLAUDECODE: &str = r#"
export const CLAUDECODE_MODEL_IDS = [
  "claude-opus-5-5",
  "claude-sonnet-5-5", // newest Sonnet
  "claude-new-1",
] as const;

const MODEL_LABELS: Record<string, string> = {
  "claude-opus-5-5": "Claude Opus 5.5",
  "claude-sonnet-5-5": "Claude Sonnet 5.5",
};
"#;

    const CODEX: &str = r#"
export const CODEX_MODEL_IDS = [
  // GPT-6 Astra (six levels)
  "gpt-6-astra",
  "gpt-6-astra-max",
  "gpt-5.2-none",
  "gpt-5.1-codex-max",
  "gpt-5.1-codex-max-low",
  "gpt-5.1-chat-latest",
  // Legacy mappings
  "gpt-5-codex",
] as const;

const MODEL_MAP: Record<string, string> = {
  "gpt-6-astra": "gpt-6-astra",
  "gpt-6-astra-max": "gpt-6-astra",
  "gpt-5.2-none": "gpt-5.2",
  "gpt-5.1-codex-max": "gpt-5.1-codex-max",
  "gpt-5.1-codex-max-low": "gpt-5.1-codex-max",
  "gpt-5.1-chat-latest": "gpt-5.1",
  "gpt-5-codex": "gpt-5.1-codex",
};

const BASE_MODEL_LABELS: Record<string, string> = {
  "gpt-6-astra": "GPT-6 Astra",
  "gpt-5.2": "GPT-5.2",
  "gpt-5.1-codex-max": "GPT-5.1 Codex Max",
  "gpt-5-codex": "GPT-5 Codex (Legacy)",
};

function formatReasoningSuffix(suffix: string): string {
  switch (suffix) {
    case "none":
      return "None";
    case "low":
      return "Low";
    case "max":
      return "Max";
    default:
      return suffix;
  }
}

function getCodexModelDisplayName(modelId: string): string {
  if (modelId === "gpt-5.1-chat-latest") {
    return "GPT-5.1 (Chat Latest)";
  }
  return baseLabel;
}
"#;

    #[test]
    fn claudecode_models_keep_order_and_labels() {
        let m = parse_claudecode_models(CLAUDECODE).unwrap_or_default();
        let got: Vec<_> = m.iter().map(|r| (r.id.as_str(), r.name.as_str())).collect();
        assert_eq!(
            got,
            [
                ("claude-opus-5-5", "Claude Opus 5.5"),
                ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
                ("claude-new-1", "claude-new-1"),
            ]
        );
    }

    #[test]
    fn codex_models_drop_legacy_and_aliases() {
        let m = parse_codex_models(CODEX).unwrap_or_default();
        let got: Vec<_> = m.iter().map(|r| (r.id.as_str(), r.name.as_str())).collect();
        assert_eq!(
            got,
            [
                ("gpt-6-astra", "GPT-6 Astra"),
                ("gpt-6-astra-max", "GPT-6 Astra (Max)"),
                ("gpt-5.2-none", "GPT-5.2 (None)"),
                ("gpt-5.1-codex-max", "GPT-5.1 Codex Max"),
                ("gpt-5.1-codex-max-low", "GPT-5.1 Codex Max (Low)"),
            ]
        );
    }

    #[test]
    fn policy_defaults_and_model_lists() {
        let mut reg = parse_registry(TYPES, PROVIDERS).unwrap_or_default();
        reg.codex_models = parse_codex_models(CODEX).unwrap_or_default();
        let m = build_manifest(&reg);
        let c = m.providers.iter().find(|p| p.id == "codex");
        // SR's policy override wins over Selene's DEFAULT_MODELS (`gpt-5.4`).
        assert_eq!(c.and_then(|p| p.default_model.as_deref()), Some("gpt-5.5"));
        assert_eq!(c.map(|p| p.models.len()), Some(5));
        // `gpt-5.5` isn't in this small list: the run fails instead of shipping a broken default.
        assert!(verify(&m).is_err());
        let a = m.providers.iter().find(|p| p.id == "anthropic");
        assert_eq!(a.map(|p| p.models.is_empty()), Some(true));
    }

    /// The committed manifest matches Selene's registry (same check as `--check`). Skipped when
    /// the Selene sources aren't next to this project (standalone checkout).
    #[test]
    fn committed_manifest_is_current() {
        let root = default_selene_root();
        if !SOURCE_FILES.iter().all(|f| root.join(f).is_file()) {
            return;
        }
        let reg = load(&root);
        assert!(reg.is_ok(), "{reg:?}");
        let m = reg.map(|r| build_manifest(&r));
        if let Ok(m) = &m {
            assert_eq!(verify(m), Ok(()));
        }
        let body = m.ok().and_then(|m| render(&m).ok());
        let committed = std::fs::read_to_string(project_root().join(OUT_REL))
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert_eq!(body.as_deref(), Some(committed.as_str()));
    }
}
