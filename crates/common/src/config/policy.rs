//! Machine policy set by an administrator (M5-E): `%ProgramData%\SeleneRetrieval\policy.toml`
//! and the registry equivalents `HKLM\SOFTWARE\Policies\SeleneRetrieval` /
//! `HKCU\SOFTWARE\Policies\SeleneRetrieval` (docs/02 §2.5: "policy.toml overrides … Managed by
//! your organisation").
//!
//! Rules:
//! * **Restrict only.** Every key can only take something away (cloud AI providers, the HTTP
//!   listener, MCP `add_folder` and `sync_folder`, folders outside the allowed roots, model downloads). No key turns
//!   a feature on or widens access, so a policy can never weaken a security default, wherever it
//!   came from. There is no telemetry to manage (the product sends none).
//! * **Policy wins over `settings.toml`.** [`Policy::apply`] forces the locked settings after every
//!   load and every change; the locked fields are listed in the status snapshot
//!   ([`Policy::managed_fields`]) with the line "Managed by your organisation".
//! * **Most restrictive wins** when several sources set the same key ([`Policy::merge`]).
//! * **Fails closed.** A policy file that can't be parsed locks every key at its most restrictive
//!   value (and no folder is allowed); a key with a wrong type locks that key. Each problem is a
//!   plain note in [`Policy::issues`] for the log and diagnostics. Unknown keys are ignored.
//!
//! This module is plain data plus parsing; reading the file and the registry is the engine's job.
//! The engine installs the loaded policy once per process ([`install`]); the sandbox's production
//! root rules ([`crate::sandbox::DangerousRoots::from_env`]) consult it ([`current`]), so every
//! folder check in the service applies the allowed roots without being handed them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{SearchModel, Settings};

/// File name inside `%ProgramData%\SeleneRetrieval`.
pub const POLICY_FILE: &str = "policy.toml";
/// Folder under `%ProgramData%` holding [`POLICY_FILE`].
pub const POLICY_DIR: &str = "SeleneRetrieval";
/// Registry key (under HKLM and HKCU) holding the same keys as values.
pub const POLICY_REG_KEY: &str = r"SOFTWARE\Policies\SeleneRetrieval";
/// The one plain line shown next to a locked setting.
pub const MANAGED_TEXT: &str = "Managed by your organisation";

/// Pseudo settings paths for locks that have no `settings.toml` field.
pub mod paths {
    /// Cloud AI providers (`[providers] allow_remote`).
    pub const REMOTE_PROVIDERS: &str = "provider.remote";
    /// The provider allow-list (`[providers] allowed`).
    pub const ALLOWED_PROVIDERS: &str = "provider.provider";
    pub const HTTP_SERVER: &str = "integrations.http_server_enabled";
    pub const MCP_ADD_FOLDER: &str = "integrations.mcp_add_folder_enabled";
    pub const MCP_SYNC_FOLDER: &str = "integrations.mcp_sync_folder_enabled";
    /// Folders may only be added inside these roots (`[folders] allowed_roots`).
    pub const FOLDER_ROOTS: &str = "folders.allowed_roots";
    /// Optional model downloads (better search tiers, multilingual, reranker).
    pub const MODEL_DOWNLOADS: &str = "search.model_downloads";
    /// Remote embeddings follow the cloud-provider lock.
    pub const SEARCH_MODEL: &str = "search.search_model";
    /// The reranker needs a download.
    pub const RERANK: &str = "ranking.rerank_enabled";
}

/// One locked setting as the status snapshot lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ManagedField {
    /// Dotted settings path (or one of [`paths`]).
    pub path: String,
    /// Always [`MANAGED_TEXT`].
    pub message: String,
    /// Plain extra line, e.g. "Allowed: Ollama, OpenAI" or the allowed folder roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A registry value as the policy reader sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegValue {
    Dword(u32),
    /// `REG_SZ` (one string) or `REG_MULTI_SZ` (several).
    Strings(Vec<String>),
}

/// The effective machine policy. `None` = not managed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Policy {
    /// `false` → only AI providers on this computer (loopback address) may be used.
    pub allow_remote_providers: Option<bool>,
    /// Provider ids (lower-case) that may be used; anything else is refused.
    pub allowed_providers: Option<Vec<String>>,
    /// `false` → the opt-in HTTP listener stays off.
    pub allow_http_server: Option<bool>,
    /// `false` → the MCP `add_folder` tool stays off.
    pub allow_mcp_add_folder: Option<bool>,
    /// `false` → the MCP `sync_folder` tool stays off.
    pub allow_mcp_sync_folder: Option<bool>,
    /// Folders (to add, and already added) must be inside one of these; empty = none allowed.
    pub allowed_folder_roots: Option<Vec<PathBuf>>,
    /// `false` → no optional model download (the bundled model keeps working).
    pub allow_model_downloads: Option<bool>,
    /// Where the policy came from ("policy.toml", "HKLM", "HKCU"); diagnostics only.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Plain notes about problems that locked a key (diagnostics and the log).
    #[serde(default)]
    pub issues: Vec<String>,
}

const KEY_ALLOW_REMOTE: &str = "allow_remote";
const KEY_ALLOWED: &str = "allowed";
const KEY_HTTP: &str = "allow_http_server";
const KEY_ADD_FOLDER: &str = "allow_mcp_add_folder";
const KEY_SYNC_FOLDER: &str = "allow_mcp_sync_folder";
const KEY_ROOTS: &str = "allowed_roots";
const KEY_DOWNLOADS: &str = "allow_downloads";

/// Registry value names (same meaning as the TOML keys).
pub mod reg_names {
    pub const ALLOW_REMOTE_PROVIDERS: &str = "AllowRemoteProviders";
    pub const ALLOWED_PROVIDERS: &str = "AllowedProviders";
    pub const ALLOW_HTTP_SERVER: &str = "AllowHttpServer";
    pub const ALLOW_MCP_ADD_FOLDER: &str = "AllowMcpAddFolder";
    pub const ALLOW_MCP_SYNC_FOLDER: &str = "AllowMcpSyncFolder";
    pub const ALLOWED_FOLDER_ROOTS: &str = "AllowedFolderRoots";
    pub const ALLOW_MODEL_DOWNLOADS: &str = "AllowModelDownloads";
    pub const ALL: &[&str] = &[
        ALLOW_REMOTE_PROVIDERS,
        ALLOWED_PROVIDERS,
        ALLOW_HTTP_SERVER,
        ALLOW_MCP_ADD_FOLDER,
        ALLOW_MCP_SYNC_FOLDER,
        ALLOWED_FOLDER_ROOTS,
        ALLOW_MODEL_DOWNLOADS,
    ];
}

impl Policy {
    /// Nothing managed.
    pub fn unmanaged() -> Self {
        Self::default()
    }

    /// Every key at its most restrictive value (a policy that exists but can't be read).
    pub fn locked_down(source: &str, why: impl Into<String>) -> Self {
        Self {
            allow_remote_providers: Some(false),
            allowed_providers: None,
            allow_http_server: Some(false),
            allow_mcp_add_folder: Some(false),
            allow_mcp_sync_folder: Some(false),
            allowed_folder_roots: Some(Vec::new()),
            allow_model_downloads: Some(false),
            sources: vec![source.to_string()],
            issues: vec![why.into()],
        }
    }

    /// Whether any key is managed.
    pub fn is_managed(&self) -> bool {
        self.allow_remote_providers.is_some()
            || self.allowed_providers.is_some()
            || self.allow_http_server.is_some()
            || self.allow_mcp_add_folder.is_some()
            || self.allow_mcp_sync_folder.is_some()
            || self.allowed_folder_roots.is_some()
            || self.allow_model_downloads.is_some()
    }

    /// Parse `policy.toml` leniently (see the module rules). `source` names it in notes.
    pub fn from_toml_str(text: &str, source: &str) -> Self {
        let table: toml::Table = match toml::from_str(text) {
            Ok(t) => t,
            Err(e) => {
                return Self::locked_down(
                    source,
                    format!(
                        "{source} couldn't be read ({}); every managed feature is turned off until it is fixed",
                        e.message()
                    ),
                );
            }
        };
        let mut p = Self {
            sources: vec![source.to_string()],
            ..Self::default()
        };
        for (section, v) in &table {
            let Some(t) = v.as_table() else {
                p.issues
                    .push(format!("{source}: [{section}] is not a section; ignored"));
                continue;
            };
            for (k, v) in t {
                let at = format!("{section}.{k}");
                match (section.as_str(), k.as_str()) {
                    ("providers", KEY_ALLOW_REMOTE) => {
                        p.allow_remote_providers = Some(p.toml_bool(v, &at, source));
                    }
                    ("providers", KEY_ALLOWED) => {
                        p.allowed_providers = Some(
                            p.toml_strings(v, &at, source)
                                .into_iter()
                                .map(|s| s.trim().to_ascii_lowercase())
                                .filter(|s| !s.is_empty())
                                .collect(),
                        );
                    }
                    ("integrations", KEY_HTTP) => {
                        p.allow_http_server = Some(p.toml_bool(v, &at, source));
                    }
                    ("integrations", KEY_ADD_FOLDER) => {
                        p.allow_mcp_add_folder = Some(p.toml_bool(v, &at, source));
                    }
                    ("integrations", KEY_SYNC_FOLDER) => {
                        p.allow_mcp_sync_folder = Some(p.toml_bool(v, &at, source));
                    }
                    ("folders", KEY_ROOTS) => {
                        let raw = p.toml_strings(v, &at, source);
                        p.allowed_folder_roots = Some(p.roots(raw, &at, source));
                    }
                    ("models", KEY_DOWNLOADS) => {
                        p.allow_model_downloads = Some(p.toml_bool(v, &at, source));
                    }
                    _ => p
                        .issues
                        .push(format!("{source}: unknown key {at}; ignored")),
                }
            }
        }
        p
    }

    /// Policy from registry values (`get(name)` reads one value of the key). Wrong types lock the
    /// key, like the file.
    pub fn from_registry(source: &str, get: impl Fn(&str) -> Option<RegValue>) -> Self {
        use reg_names as n;
        let mut p = Self {
            sources: vec![source.to_string()],
            ..Self::default()
        };
        let mut any = false;
        for name in n::ALL {
            let Some(v) = get(name) else { continue };
            any = true;
            let at = format!(r"{source}\{POLICY_REG_KEY}\{name}");
            match *name {
                n::ALLOW_REMOTE_PROVIDERS => p.allow_remote_providers = Some(p.reg_bool(&v, &at)),
                n::ALLOW_HTTP_SERVER => p.allow_http_server = Some(p.reg_bool(&v, &at)),
                n::ALLOW_MCP_ADD_FOLDER => p.allow_mcp_add_folder = Some(p.reg_bool(&v, &at)),
                n::ALLOW_MCP_SYNC_FOLDER => p.allow_mcp_sync_folder = Some(p.reg_bool(&v, &at)),
                n::ALLOW_MODEL_DOWNLOADS => p.allow_model_downloads = Some(p.reg_bool(&v, &at)),
                n::ALLOWED_PROVIDERS => {
                    p.allowed_providers = Some(
                        p.reg_strings(&v, &at)
                            .into_iter()
                            .map(|s| s.to_ascii_lowercase())
                            .collect(),
                    );
                }
                n::ALLOWED_FOLDER_ROOTS => {
                    let raw = p.reg_strings(&v, &at);
                    p.allowed_folder_roots = Some(p.roots(raw, &at, source));
                }
                _ => {}
            }
        }
        if !any {
            p.sources.clear();
        }
        p
    }

    fn toml_bool(&mut self, v: &toml::Value, at: &str, source: &str) -> bool {
        v.as_bool().unwrap_or_else(|| {
            self.issues.push(format!(
                "{source}: {at} must be true or false; locked (treated as false)"
            ));
            false
        })
    }

    fn toml_strings(&mut self, v: &toml::Value, at: &str, source: &str) -> Vec<String> {
        let list = match v {
            toml::Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_string)).collect(),
            toml::Value::String(s) => Some(vec![s.clone()]),
            _ => None,
        };
        list.unwrap_or_else(|| {
            self.issues.push(format!(
                "{source}: {at} must be a list of text values; locked (nothing allowed)"
            ));
            Vec::new()
        })
    }

    fn reg_bool(&mut self, v: &RegValue, at: &str) -> bool {
        match v {
            RegValue::Dword(d) => *d != 0,
            RegValue::Strings(s) => match s.first().map(|x| x.trim().to_ascii_lowercase()) {
                Some(x) if x == "1" || x == "true" => true,
                Some(x) if x == "0" || x == "false" => false,
                _ => {
                    self.issues.push(format!(
                        "{at} must be a DWORD 0 or 1; locked (treated as 0)"
                    ));
                    false
                }
            },
        }
    }

    fn reg_strings(&mut self, v: &RegValue, at: &str) -> Vec<String> {
        match v {
            // REG_SZ may carry several entries separated by ';'.
            RegValue::Strings(s) => s
                .iter()
                .flat_map(|x| x.split(';'))
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
                .collect(),
            RegValue::Dword(_) => {
                self.issues.push(format!(
                    "{at} must be text (REG_SZ or REG_MULTI_SZ); locked (nothing allowed)"
                ));
                Vec::new()
            }
        }
    }

    /// Absolute local or UNC roots only; anything else is dropped with a note (fewer roots =
    /// more restrictive, so a bad entry never widens access).
    fn roots(&mut self, raw: Vec<String>, at: &str, source: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for r in raw {
            let p = PathBuf::from(r.trim());
            if is_absolute_root(&p) {
                out.push(p);
            } else {
                self.issues.push(format!(
                    "{source}: {at} entry \"{}\" is not a full folder path; ignored",
                    r.trim()
                ));
            }
        }
        out
    }

    /// Combine two sources: for every key the most restrictive value wins (`false` beats
    /// `true`; allow-lists intersect).
    pub fn merge(mut self, other: Policy) -> Policy {
        fn and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
            match (a, b) {
                (Some(x), Some(y)) => Some(x && y),
                (x, None) => x,
                (None, y) => y,
            }
        }
        fn intersect<T: Clone>(
            a: Option<Vec<T>>,
            b: Option<Vec<T>>,
            same: impl Fn(&T, &T) -> bool,
        ) -> Option<Vec<T>> {
            match (a, b) {
                (Some(x), Some(y)) => Some(
                    x.iter()
                        .filter(|i| y.iter().any(|j| same(i, j)))
                        .cloned()
                        .collect(),
                ),
                (x, None) => x,
                (None, y) => y,
            }
        }
        self.allow_remote_providers =
            and(self.allow_remote_providers, other.allow_remote_providers);
        self.allow_http_server = and(self.allow_http_server, other.allow_http_server);
        self.allow_mcp_add_folder = and(self.allow_mcp_add_folder, other.allow_mcp_add_folder);
        self.allow_mcp_sync_folder = and(self.allow_mcp_sync_folder, other.allow_mcp_sync_folder);
        self.allow_model_downloads = and(self.allow_model_downloads, other.allow_model_downloads);
        self.allowed_providers =
            intersect(self.allowed_providers, other.allowed_providers, |a, b| {
                a == b
            });
        // Roots: keep each root of one list that lies inside a root of the other (both ways),
        // i.e. the overlap of the two allowed areas.
        self.allowed_folder_roots = match (self.allowed_folder_roots, other.allowed_folder_roots) {
            (Some(x), Some(y)) => {
                let mut out: Vec<PathBuf> = x
                    .iter()
                    .filter(|a| y.iter().any(|b| path_within(a, b)))
                    .cloned()
                    .collect();
                for b in &y {
                    if x.iter().any(|a| path_within(b, a)) && !out.iter().any(|o| same_path(o, b)) {
                        out.push(b.clone());
                    }
                }
                Some(out)
            }
            (x, None) => x,
            (None, y) => y,
        };
        self.sources.extend(other.sources);
        self.issues.extend(other.issues);
        self
    }

    /// Whether the AI provider `id`, reached at `base_url` (the effective address), may be used.
    pub fn provider_allowed(&self, id: &str, base_url: Option<&str>) -> bool {
        let id = id.trim().to_ascii_lowercase();
        if let Some(list) = &self.allowed_providers
            && !list.contains(&id)
        {
            return false;
        }
        if self.allow_remote_providers == Some(false) {
            return base_url.is_some_and(url_is_loopback);
        }
        true
    }

    /// Whether remote (cloud) AI providers are allowed at all.
    pub fn remote_providers_allowed(&self) -> bool {
        self.allow_remote_providers != Some(false)
    }

    pub fn http_server_allowed(&self) -> bool {
        self.allow_http_server != Some(false)
    }

    pub fn mcp_add_folder_allowed(&self) -> bool {
        self.allow_mcp_add_folder != Some(false)
    }

    pub fn mcp_sync_folder_allowed(&self) -> bool {
        self.allow_mcp_sync_folder != Some(false)
    }

    pub fn model_downloads_allowed(&self) -> bool {
        self.allow_model_downloads != Some(false)
    }

    /// Whether a folder root (ideally canonical) is inside the allowed roots.
    pub fn folder_allowed(&self, root: &Path) -> bool {
        match &self.allowed_folder_roots {
            None => true,
            Some(list) => list.iter().any(|r| path_within(root, r)),
        }
    }

    /// Force the locked settings (policy wins over `settings.toml`). Returns the dotted paths
    /// that were changed (for the log). Idempotent.
    pub fn apply(&self, s: &mut Settings) -> Vec<String> {
        let mut changed = Vec::new();
        if !self.http_server_allowed() && s.integrations.http_server_enabled {
            s.integrations.http_server_enabled = false;
            changed.push(paths::HTTP_SERVER.to_string());
        }
        if !self.mcp_add_folder_allowed() && s.integrations.mcp_add_folder_enabled {
            s.integrations.mcp_add_folder_enabled = false;
            changed.push(paths::MCP_ADD_FOLDER.to_string());
        }
        if !self.mcp_sync_folder_allowed() && s.integrations.mcp_sync_folder_enabled {
            s.integrations.mcp_sync_folder_enabled = false;
            changed.push(paths::MCP_SYNC_FOLDER.to_string());
        }
        if let SearchModel::Cloud(p) = &s.search.search_model
            && !self.provider_allowed(p, None)
        {
            // Remote embeddings send file text out; never under a cloud lock.
            s.search.search_model = SearchModel::Standard;
            changed.push(paths::SEARCH_MODEL.to_string());
        }
        if let Some(id) = s.provider.provider.clone().filter(|p| !p.is_empty())
            && !self.provider_allowed(
                &id,
                s.network.base_url.as_deref().or(default_local_url(&id)),
            )
        {
            s.provider.provider = None;
            s.provider.default_model = None;
            changed.push(paths::ALLOWED_PROVIDERS.to_string());
        }
        changed
    }

    /// The locked settings for the status snapshot (field + "Managed by your organisation").
    pub fn managed_fields(&self) -> Vec<ManagedField> {
        let f = |path: &str, detail: Option<String>| ManagedField {
            path: path.to_string(),
            message: MANAGED_TEXT.to_string(),
            detail,
        };
        let mut out = Vec::new();
        if self.allow_remote_providers == Some(false) {
            out.push(f(
                paths::REMOTE_PROVIDERS,
                Some("Only AI providers on this computer can be used.".into()),
            ));
        }
        if let Some(list) = &self.allowed_providers {
            out.push(f(
                paths::ALLOWED_PROVIDERS,
                Some(if list.is_empty() {
                    "No AI provider can be used.".into()
                } else {
                    format!("Allowed: {}", list.join(", "))
                }),
            ));
        }
        if self.allow_http_server == Some(false) {
            out.push(f(paths::HTTP_SERVER, None));
        }
        if self.allow_mcp_add_folder == Some(false) {
            out.push(f(paths::MCP_ADD_FOLDER, None));
        }
        if self.allow_mcp_sync_folder == Some(false) {
            out.push(f(paths::MCP_SYNC_FOLDER, None));
        }
        if let Some(roots) = &self.allowed_folder_roots {
            out.push(f(
                paths::FOLDER_ROOTS,
                Some(if roots.is_empty() {
                    "No folders can be added.".into()
                } else {
                    format!(
                        "Folders can only be added inside: {}",
                        roots
                            .iter()
                            .map(|r| r.display().to_string())
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                }),
            ));
        }
        if self.allow_model_downloads == Some(false) {
            out.push(f(
                paths::MODEL_DOWNLOADS,
                Some("Search uses the built-in model.".into()),
            ));
            out.push(f(paths::RERANK, None));
        }
        out
    }
}

/// Built-in addresses of providers that run on this computer (Ollama, vLLM), used when the user
/// set no address: such providers stay usable under a cloud lock.
fn default_local_url(id: &str) -> Option<&'static str> {
    match id {
        "ollama" => Some("http://localhost:11434"),
        "vllm" => Some("http://localhost:8000"),
        _ => None,
    }
}

/// `http(s)://` URL whose host is this computer (`localhost`, `127.x.x.x`, `::1`).
pub fn url_is_loopback(url: &str) -> bool {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return false;
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // No credentials in the authority (`http://localhost@evil.example`).
    if authority.contains('@') {
        return false;
    }
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host == "::1" {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4 && parts[0] == "127" && parts.iter().all(|p| p.parse::<u8>().is_ok())
}

/// `C:\…` or `\\server\share\…` (not drive-relative, not relative, not a device path).
fn is_absolute_root(p: &Path) -> bool {
    let s = p.to_string_lossy();
    if s.starts_with(r"\\?\") || s.starts_with(r"\\.\") {
        return false;
    }
    let b = s.as_bytes();
    let drive = b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/');
    let unc =
        s.starts_with(r"\\") && s[2..].split(['\\', '/']).filter(|x| !x.is_empty()).count() >= 2;
    drive || unc
}

/// Case-insensitive path components (`\` and `/` alike; a verbatim prefix dropped).
fn components(p: &Path) -> Vec<String> {
    let s = dunce::simplified(p).to_string_lossy().to_lowercase();
    let unc = s.starts_with(r"\\") || s.starts_with("//");
    let mut v: Vec<String> = s
        .split(['\\', '/'])
        .filter(|c| !c.is_empty() && *c != ".")
        .map(str::to_string)
        .collect();
    if unc && !v.is_empty() {
        v[0] = format!(r"\\{}", v[0]);
    }
    v
}

fn same_path(a: &Path, b: &Path) -> bool {
    components(a) == components(b)
}

/// Whether `p` is `root` or inside it.
pub fn path_within(p: &Path, root: &Path) -> bool {
    let (p, r) = (components(p), components(root));
    !r.is_empty() && p.len() >= r.len() && p[..r.len()] == r[..]
}

static CURRENT: RwLock<Option<Arc<Policy>>> = RwLock::new(None);

/// Install the machine policy for this process (the engine does this once at start, before any
/// folder or setting is checked). Later calls replace it.
pub fn install(p: Policy) {
    *CURRENT
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(p));
}

/// The installed policy (unmanaged until [`install`] runs).
pub fn current() -> Arc<Policy> {
    CURRENT
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_file_parses_and_locks() {
        let p = Policy::from_toml_str(
            r#"
[providers]
allow_remote = false
allowed = ["Ollama", "openai"]
[integrations]
allow_http_server = false
allow_mcp_add_folder = false
allow_mcp_sync_folder = false
[folders]
allowed_roots = ['D:\Work', '\\server\share\team']
[models]
allow_downloads = false
"#,
            "policy.toml",
        );
        assert!(p.issues.is_empty(), "{:?}", p.issues);
        assert_eq!(p.allow_remote_providers, Some(false));
        assert_eq!(
            p.allowed_providers,
            Some(vec!["ollama".to_string(), "openai".to_string()])
        );
        assert!(!p.http_server_allowed() && !p.mcp_add_folder_allowed());
        assert!(!p.mcp_sync_folder_allowed());
        assert!(!p.model_downloads_allowed());
        assert!(p.folder_allowed(Path::new(r"d:\work\notes")));
        assert!(p.folder_allowed(Path::new(r"\\SERVER\share\team\x")));
        assert!(!p.folder_allowed(Path::new(r"D:\Workshop")));
        assert!(!p.folder_allowed(Path::new(r"C:\Users\me\Documents")));
        // Ollama on this computer is fine; OpenAI is a cloud provider and remote is off.
        assert!(p.provider_allowed("ollama", Some("http://localhost:11434")));
        assert!(!p.provider_allowed("openai", Some("https://api.openai.com/v1/")));
        assert!(!p.provider_allowed("gemini", Some("http://127.0.0.1:9")));
        assert_eq!(p.managed_fields().len(), 8);
        assert!(
            p.managed_fields()
                .iter()
                .all(|f| f.message == "Managed by your organisation")
        );
    }

    /// Amendment 17: no policy key locks the removed Files-only mode. A policy that still sets
    /// `indexing_mode` loads without an error: the key is ignored with a note, nothing is locked.
    #[test]
    fn removed_indexing_mode_key_is_ignored() {
        let p = Policy::from_toml_str(
            r#"
[folders]
indexing_mode = "files_only"
[indexing]
indexing_mode = "files_only"
"#,
            "policy.toml",
        );
        assert!(!p.is_managed());
        assert_eq!(p.issues.len(), 2, "{:?}", p.issues);
        assert!(
            p.issues
                .iter()
                .all(|i| i.contains("indexing_mode; ignored"))
        );
        let mut s = Settings::default();
        assert!(p.apply(&mut s).is_empty());
        assert_eq!(s, Settings::default());
        let r = Policy::from_registry("HKLM", |name| {
            (name == "IndexingMode").then(|| RegValue::Strings(vec!["files_only".into()]))
        });
        assert!(!r.is_managed() && r.issues.is_empty());
    }

    #[test]
    fn empty_or_absent_keys_manage_nothing() {
        let p = Policy::from_toml_str("# nothing yet\n", "policy.toml");
        assert!(!p.is_managed());
        assert!(p.issues.is_empty());
        let mut s = Settings::default();
        s.integrations.http_server_enabled = true;
        assert!(p.apply(&mut s).is_empty());
        assert!(s.integrations.http_server_enabled);
    }

    #[test]
    fn broken_file_fails_closed() {
        let p = Policy::from_toml_str("[providers\nallow_remote = ", "policy.toml");
        assert!(p.is_managed());
        assert!(!p.remote_providers_allowed());
        assert!(!p.http_server_allowed());
        assert!(!p.mcp_add_folder_allowed());
        assert!(!p.mcp_sync_folder_allowed());
        assert!(!p.model_downloads_allowed());
        assert!(!p.folder_allowed(Path::new(r"D:\Work")));
        assert_eq!(p.issues.len(), 1);
        assert!(p.issues[0].contains("couldn't be read"), "{}", p.issues[0]);
    }

    #[test]
    fn wrong_types_lock_that_key_only() {
        let p = Policy::from_toml_str(
            r#"
[providers]
allow_remote = "no"
[integrations]
allow_http_server = true
[folders]
allowed_roots = 7
[telemetry]
enabled = false
"#,
            "policy.toml",
        );
        assert!(!p.remote_providers_allowed(), "bad bool locks");
        assert!(p.http_server_allowed(), "a valid true stays true");
        assert_eq!(p.allowed_folder_roots, Some(Vec::new()), "bad list locks");
        assert_eq!(p.allow_mcp_add_folder, None, "absent stays unmanaged");
        assert_eq!(p.allow_mcp_sync_folder, None, "absent stays unmanaged");
        assert_eq!(p.issues.len(), 3, "{:?}", p.issues);
    }

    #[test]
    fn relative_roots_are_dropped_never_widening() {
        let p = Policy::from_toml_str(
            "[folders]\nallowed_roots = ['Work', 'C:Docs', '\\\\?\\C:\\x', 'E:\\Data']\n",
            "policy.toml",
        );
        assert_eq!(
            p.allowed_folder_roots,
            Some(vec![PathBuf::from(r"E:\Data")])
        );
        assert_eq!(p.issues.len(), 3);
    }

    #[test]
    fn apply_forces_locked_settings() {
        let p = Policy {
            allow_remote_providers: Some(false),
            allow_http_server: Some(false),
            allow_mcp_add_folder: Some(false),
            allow_mcp_sync_folder: Some(false),
            ..Policy::default()
        };
        let mut s = Settings::default();
        s.integrations.http_server_enabled = true;
        s.integrations.mcp_add_folder_enabled = true;
        s.integrations.mcp_sync_folder_enabled = true;
        s.search.search_model = SearchModel::Cloud("openai".into());
        s.provider.provider = Some("openai".into());
        s.provider.default_model = Some("gpt-x".into());
        let changed = p.apply(&mut s);
        assert_eq!(changed.len(), 5, "{changed:?}");
        assert!(!s.integrations.http_server_enabled && !s.integrations.mcp_add_folder_enabled);
        assert!(!s.integrations.mcp_sync_folder_enabled);
        assert!(changed.contains(&paths::MCP_SYNC_FOLDER.to_string()));
        assert_eq!(s.search.search_model, SearchModel::Standard);
        assert_eq!(s.provider.provider, None);
        assert!(p.apply(&mut s).is_empty(), "idempotent");
        // A local provider survives a cloud lock (built-in or user address on this computer).
        let mut s = Settings::default();
        s.provider.provider = Some("ollama".into());
        assert!(p.apply(&mut s).is_empty());
        s.network.base_url = Some("http://192.168.1.5:11434".into());
        assert_eq!(p.apply(&mut s), vec![paths::ALLOWED_PROVIDERS.to_string()]);
    }

    #[test]
    fn merge_takes_the_most_restrictive() {
        let file = Policy {
            allow_http_server: Some(true),
            allow_model_downloads: Some(false),
            allowed_providers: Some(vec!["ollama".into(), "openai".into()]),
            allowed_folder_roots: Some(vec![PathBuf::from(r"D:\"), PathBuf::from(r"E:\Data")]),
            ..Policy::default()
        };
        let hklm = Policy {
            allow_http_server: Some(false),
            allow_mcp_sync_folder: Some(false),
            allowed_providers: Some(vec!["openai".into()]),
            allowed_folder_roots: Some(vec![PathBuf::from(r"D:\Work")]),
            ..Policy::default()
        };
        let m = file.merge(hklm);
        assert!(!m.http_server_allowed());
        assert!(!m.mcp_sync_folder_allowed());
        assert!(!m.model_downloads_allowed());
        assert_eq!(m.allowed_providers, Some(vec!["openai".to_string()]));
        assert!(m.folder_allowed(Path::new(r"D:\Work\a")));
        assert!(!m.folder_allowed(Path::new(r"D:\Other")));
        assert!(!m.folder_allowed(Path::new(r"E:\Data")));
    }

    #[test]
    fn registry_values_map_like_the_file() {
        let p = Policy::from_registry("HKLM", |name| match name {
            reg_names::ALLOW_REMOTE_PROVIDERS => Some(RegValue::Dword(0)),
            reg_names::ALLOWED_FOLDER_ROOTS => Some(RegValue::Strings(vec![
                r"D:\Work".into(),
                r"E:\A; E:\B".into(),
            ])),
            reg_names::ALLOW_HTTP_SERVER => Some(RegValue::Strings(vec!["maybe".into()])),
            reg_names::ALLOWED_PROVIDERS => Some(RegValue::Dword(1)),
            reg_names::ALLOW_MCP_SYNC_FOLDER => Some(RegValue::Dword(0)),
            _ => None,
        });
        assert_eq!(p.sources, vec!["HKLM".to_string()]);
        assert!(!p.remote_providers_allowed());
        assert!(!p.mcp_sync_folder_allowed() && p.mcp_add_folder_allowed());
        assert_eq!(
            p.allowed_folder_roots.as_ref().map(Vec::len),
            Some(3),
            "{:?}",
            p.allowed_folder_roots
        );
        assert!(!p.http_server_allowed(), "unreadable value locks");
        assert_eq!(p.allowed_providers, Some(Vec::new()), "wrong type locks");
        assert_eq!(p.issues.len(), 2);
        assert!(!Policy::from_registry("HKCU", |_| None).is_managed());
    }

    #[test]
    fn loopback_detection_is_strict() {
        for ok in [
            "http://localhost:11434",
            "http://127.0.0.1:8000/v1/",
            "https://[::1]:9/",
            "HTTP://LOCALHOST",
        ] {
            assert!(url_is_loopback(ok), "{ok}");
        }
        for bad in [
            "http://localhost.evil.example",
            "http://localhost@evil.example/",
            "http://128.0.0.1",
            "https://api.openai.com",
            "file://localhost/x",
            "localhost:11434",
            "http://127.0.0.1.nip.io",
        ] {
            assert!(!url_is_loopback(bad), "{bad}");
        }
    }
}
