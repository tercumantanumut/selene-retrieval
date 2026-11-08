//! Remote providers (M3-P): chat (answers, enhance, iterate), embeddings (cloud search model),
//! live key validation and model discovery, for every enabled provider in Selene's registry
//! ([`crate::registry`]).
//!
//! ```text
//! registry::manifest().require("openrouter")      ProviderSpec (base URL, protocol, defaults)
//!   └─ ProviderHandle::connect(spec, key, settings, gate, proxy creds)
//!        ├─ proxy::plan_async        per-host proxy decision (override / Windows / PAC / WinHTTP)
//!        ├─ http::build_client       ring-only rustls, proxy, redirects, timeouts
//!        ├─ genai::Client            wire formats (OpenAI-compatible, Anthropic) on that client
//!        └─ Egress                   consent → pacing → audit → send (cancel / timeout = abort)
//!   ├─ .llm()            RemoteLlm: LlmProvider (chat, chat_stream, list_models, validate_key)
//!   ├─ .embedder(m, d)   RemoteEmbedder: EmbeddingProvider (Doc / Query)
//!   └─ wrap in Resilient (retries with jitter, Retry-After, cancel)
//! ```
//!
//! Every network call of this module goes through [`egress::Egress::call`]; nothing here talks
//! to the network around it.

pub mod chat;
pub mod discovery;
pub mod egress;
pub mod embed;
pub mod errors;
pub mod http;
pub mod limiter;
pub mod proxy;

use std::sync::Arc;
use std::time::Duration;

use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, Endpoint};
use genai::{ModelIden, ServiceTarget};
use sr_common::Result;
use sr_common::config::Settings;
use sr_common::error::{Error, ErrorCode};

pub use chat::RemoteLlm;
pub use discovery::{KeyCheck, check_key, pick_default_model};
pub use egress::Egress;
pub use embed::RemoteEmbedder;
pub use limiter::Limiter;
pub use proxy::{ProxyChoice, ProxyPlan};

use crate::privacy::{PrivacyGate, SecretStore};
use crate::registry::{AuthKind, Protocol, ProviderSpec};
use crate::resilient::{Backoff, Resilient};
use crate::secrets::{ApiKey, PROXY_ACCOUNT, provider_account};

/// The parts of `settings.toml` that shape provider calls (`[provider]`, `[network]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSettings {
    /// `network.base_url` (Advanced) or the wizard's local address (Ollama / vLLM).
    pub base_url_override: Option<String>,
    /// `network.request_timeout_secs`: whole-attempt timeout, and the stream idle timeout.
    pub attempt_timeout: Duration,
    /// `network.rate_limit_rpm`.
    pub rate_limit_rpm: Option<u32>,
    /// `network.proxy` (Advanced). Empty = Windows proxy settings.
    pub proxy_override: Option<String>,
    /// `network.remote_embed_batch_size`.
    pub embed_batch: usize,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            base_url_override: None,
            attempt_timeout: Duration::from_secs(60),
            rate_limit_rpm: None,
            proxy_override: None,
            embed_batch: 64,
        }
    }
}

impl RemoteSettings {
    pub fn from_settings(s: &Settings) -> Self {
        let n = &s.network;
        Self {
            base_url_override: n.base_url.clone().filter(|u| !u.trim().is_empty()),
            attempt_timeout: Duration::from_secs(u64::from(n.request_timeout_secs.clamp(5, 600))),
            rate_limit_rpm: n.rate_limit_rpm,
            proxy_override: n.proxy.clone().filter(|u| !u.trim().is_empty()),
            embed_batch: n.remote_embed_batch_size.clamp(1, 2048) as usize,
        }
    }

    /// Retry policy for `Resilient` around these providers.
    pub fn backoff(&self) -> Backoff {
        Backoff::default()
    }
}

/// Owned copy of [`errors::Site`] for streams that outlive the call.
#[derive(Clone)]
pub(crate) struct SiteOwned {
    pub provider: String,
    pub host: String,
    key: ApiKey,
    pub local: bool,
}

impl SiteOwned {
    pub fn site(&self) -> errors::Site<'_> {
        errors::Site {
            provider: &self.provider,
            host: &self.host,
            key: (!self.key.is_empty()).then(|| self.key.expose()),
            local: self.local,
        }
    }
}

pub(crate) struct Inner {
    pub spec: ProviderSpec,
    pub key: ApiKey,
    pub settings: RemoteSettings,
    pub http: reqwest::Client,
    pub genai: genai::Client,
    pub egress: Egress,
    pub proxy: ProxyPlan,
}

/// One connected provider. Cheap to clone; share it between the LLM and embedding sides.
#[derive(Clone)]
pub struct ProviderHandle {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for ProviderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderHandle")
            .field("provider", &self.inner.spec.id)
            .field("key", &self.inner.key)
            .finish_non_exhaustive()
    }
}

/// Anthropic API version header for raw (non-genai) calls.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

impl ProviderHandle {
    /// Builds the client for `spec` (resolves the proxy off the async threads first). No network
    /// call is made here.
    pub async fn connect(
        spec: &ProviderSpec,
        key: ApiKey,
        settings: RemoteSettings,
        gate: Arc<dyn PrivacyGate>,
        proxy_credentials: Option<(String, String)>,
    ) -> Result<Self> {
        spec.ensure_enabled()?;
        // "Claude (sign in)" / "Codex (sign in)" run through the user's own CLI (`crate::cli`),
        // never over HTTP from here, and no other account sign-in is offered.
        if spec.is_cli() || spec.auth_kind == Some(AuthKind::Oauth) {
            return Err(Error::new(ErrorCode::ProviderNotConfigured)
                .with_provider(spec.display_name.clone())
                .with_detail("this provider runs through its own program, not over HTTP"));
        }
        if spec.needs_key() && key.is_empty() {
            return Err(Error::new(ErrorCode::ProviderAuth)
                .with_provider(spec.display_name.clone())
                .with_detail("no key saved for this provider"));
        }
        if let Some(u) = &settings.base_url_override
            && !sr_common::config::is_valid_provider_url(u)
        {
            return Err(Error::new(ErrorCode::SettingsInvalid)
                .with_section(sr_common::error::Section::Providers)
                .with_detail("provider address must be https, or http on this computer"));
        }
        let mut urls: Vec<String> = Vec::new();
        let ovr = settings.base_url_override.as_deref();
        if let Some(u) = spec.base_url_for("", ovr) {
            urls.push(u);
        }
        if ovr.is_none() {
            urls.extend(spec.base_url_by_model.values().cloned());
        }
        let proxy =
            proxy::plan_async(urls, settings.proxy_override.clone(), proxy_credentials).await;
        let http = http::build_client(&proxy, settings.attempt_timeout)?;
        let genai = genai::Client::builder()
            .with_reqwest(http.clone())
            .build()
            .map_err(|e| {
                Error::new(ErrorCode::Internal).with_detail(format!("genai client: {e}"))
            })?;
        let limiter = Arc::new(Limiter::new(settings.rate_limit_rpm));
        let egress = Egress::new(
            spec.id.clone(),
            spec.display_name.clone(),
            gate,
            limiter,
            settings.attempt_timeout,
        );
        Ok(Self {
            inner: Arc::new(Inner {
                spec: spec.clone(),
                key,
                settings,
                http,
                genai,
                egress,
                proxy,
            }),
        })
    }

    /// [`connect`](Self::connect) with the key and proxy credentials read from `secrets`
    /// (Credential Manager in the daemon).
    pub async fn connect_with_store(
        spec: &ProviderSpec,
        secrets: &dyn SecretStore,
        settings: RemoteSettings,
        gate: Arc<dyn PrivacyGate>,
    ) -> Result<Self> {
        let key = ApiKey::new(
            secrets
                .get(&provider_account(&spec.id))?
                .unwrap_or_default(),
        );
        let creds = secrets
            .get(PROXY_ACCOUNT)?
            .as_deref()
            .and_then(proxy::parse_credentials);
        Self::connect(spec, key, settings, gate, creds).await
    }

    pub fn spec(&self) -> &ProviderSpec {
        &self.inner.spec
    }
    pub fn id(&self) -> &str {
        &self.inner.spec.id
    }
    pub fn display_name(&self) -> &str {
        &self.inner.spec.display_name
    }
    pub fn settings(&self) -> &RemoteSettings {
        &self.inner.settings
    }
    /// Proxy decisions, one line per host, for "Copy diagnostics" (no credentials).
    pub fn proxy_diagnostics(&self) -> Vec<String> {
        self.inner.proxy.describe()
    }

    /// The chat side.
    pub fn llm(&self) -> RemoteLlm {
        RemoteLlm::new(self.clone())
    }
    /// The chat side wrapped in [`Resilient`] (what the engine should hold).
    pub fn resilient_llm(&self) -> Resilient<RemoteLlm> {
        Resilient::with_policy(self.llm(), self.inner.settings.backoff())
    }

    /// The embedding side for `model` with `dims` dimensions (probe them with
    /// [`RemoteEmbedder::probe_dims`] when the user picks the model). `ProviderNotConfigured` if
    /// this provider isn't offered for embeddings.
    pub fn embedder(&self, model: &str, dims: u32) -> Result<RemoteEmbedder> {
        if !self.inner.spec.supports_embeddings() {
            return Err(Error::new(ErrorCode::ProviderNotConfigured)
                .with_provider(self.display_name().to_string())
                .with_detail("this provider has no embeddings in Selene Retrieval"));
        }
        Ok(RemoteEmbedder::new(self.clone(), model, dims))
    }

    pub(crate) fn base_url_for(&self, model: &str) -> Result<String> {
        self.inner
            .spec
            .base_url_for(model, self.inner.settings.base_url_override.as_deref())
            .ok_or_else(|| {
                Error::new(ErrorCode::ProviderNotConfigured)
                    .with_provider(self.display_name().to_string())
                    .with_detail("provider has no address")
            })
    }

    pub(crate) fn site_for(&self, base_url: &str) -> SiteOwned {
        let host = proxy::host_of(base_url).unwrap_or_default();
        SiteOwned {
            provider: self.inner.spec.display_name.clone(),
            local: proxy::is_loopback(&host),
            host,
            key: self.inner.key.clone(),
        }
    }

    pub(crate) fn is_openai_compat(&self) -> bool {
        self.inner.spec.protocol == Some(Protocol::OpenaiCompat)
    }

    fn adapter(&self) -> AdapterKind {
        match self.inner.spec.protocol {
            Some(Protocol::Anthropic) => AdapterKind::Anthropic,
            _ => AdapterKind::OpenAI,
        }
    }

    /// genai target for `model`: explicit endpoint, key and adapter (no model-name routing).
    pub(crate) fn target(&self, model: &str) -> Result<(ServiceTarget, SiteOwned)> {
        let base = self.base_url_for(model)?;
        let site = self.site_for(&base);
        // Local servers without a key still get a (dummy) bearer: genai's OpenAI adapter needs a
        // value, and Ollama / vLLM ignore it unless the user configured a key.
        let key = if self.inner.key.is_empty() {
            "none".to_string()
        } else {
            self.inner.key.expose().to_string()
        };
        let auth = AuthData::from_single(key);
        Ok((
            ServiceTarget {
                endpoint: Endpoint::from_owned(base),
                auth,
                model: ModelIden::new(self.adapter(), model.to_string()),
            },
            site,
        ))
    }

    /// A GET with this provider's auth headers (model list, key check).
    pub(crate) fn get(&self, url: &str) -> reqwest::RequestBuilder {
        let mut rb = self.inner.http.get(url);
        let key = self.inner.key.expose();
        match self.inner.spec.protocol {
            Some(Protocol::Anthropic) => {
                rb = rb
                    .header("x-api-key", key)
                    .header("anthropic-version", ANTHROPIC_VERSION);
            }
            _ if !key.is_empty() => rb = rb.bearer_auth(key),
            _ => {}
        }
        rb
    }
}
