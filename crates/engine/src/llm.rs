//! The AI provider inside the engine (M3-Q): the catalog-backed [`CatalogGate`] (consent +
//! audit, docs/02 §2.5 "PrivacyGate"), the one configured chat provider ([`LlmSlot`]), and the
//! app-RPC the Providers & Keys / Answers pages use (`providers.*`, `consent.*`,
//! `settings.answers`; M3-U requests R-Q4/R-Q5).
//!
//! * The provider list is Selene's registry (`sr_providers::registry::manifest`, M3-P); nothing
//!   here names a provider or a model.
//! * Keys live in Windows Credential Manager (`sr_providers::KeyringStore`) and are never logged,
//!   echoed or written to `settings.toml`. `providers.status` returns at most the last 4
//!   characters of a long key.
//! * **No egress without consent:** every remote request passes [`CatalogGate::check`] first
//!   (M3-P's `Egress`); a blocked purpose is remembered (`EngineStatus.consent_needed`) and pushed
//!   as `consent.needed`, so the app can open the consent dialog (page 22). [Check and save] is the
//!   consent for `validate_key` + `list_models` (D-35).
//! * Changes push `SettingsChanged`, which makes every MCP session re-announce its tools
//!   (`tools/list_changed`: `context_answer` appears or disappears, U17 / R-UX29).

use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use sr_catalog::Catalog;
use sr_catalog::repo::{meta, privacy};
use sr_common::error::{Error, ErrorCode, Purpose};
use sr_common::events::{ConsentRequest, EngineEvent};
use sr_common::ids::CollectionId;
use sr_providers::{EgressRecord, LlmProvider, PrivacyGate, SecretStore};
use tokio::sync::mpsc;

use crate::EngineHandle;
use crate::bus::EventBus;

/// A chat provider given by the embedder of the engine (tests, the eval bench): used instead of
/// connecting the configured remote provider.
#[derive(Clone)]
pub struct LlmOverride {
    pub provider_id: String,
    pub display_name: String,
    pub llm: Arc<dyn LlmProvider>,
}

impl std::fmt::Debug for LlmOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmOverride")
            .field("provider_id", &self.provider_id)
            .finish_non_exhaustive()
    }
}

/// The configured chat provider, ready to call.
#[derive(Clone)]
pub struct LlmSlot {
    pub provider_id: String,
    pub display_name: String,
    /// The user's model, else empty (the provider's registry default is used).
    pub model: String,
    pub llm: Arc<dyn LlmProvider>,
}

impl std::fmt::Debug for LlmSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmSlot")
            .field("provider_id", &self.provider_id)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// Engine-side provider state (in `Inner`).
pub(crate) struct LlmState {
    pub(crate) slot: RwLock<Option<LlmSlot>>,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) gate: Arc<CatalogGate>,
    pub(crate) override_: Option<LlmOverride>,
}

// ---------------------------------------------------------------------------------------------
// PrivacyGate over the catalog
// ---------------------------------------------------------------------------------------------

/// `egress_consent` / `egress_audit` behind `sr_providers::PrivacyGate`. Audit rows are written
/// by a background task (the egress path runs on async threads and must not wait for the
/// catalog writer).
pub struct CatalogGate {
    cat: Arc<Catalog>,
    bus: Arc<EventBus>,
    needed: Arc<Mutex<Option<ConsentRequest>>>,
    audit_tx: Mutex<Option<mpsc::UnboundedSender<EgressRecord>>>,
}

impl std::fmt::Debug for CatalogGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogGate").finish_non_exhaustive()
    }
}

fn now_ms() -> i64 {
    i64::try_from(sr_common::events::now_ms()).unwrap_or(i64::MAX)
}

fn display_name(provider_id: &str) -> String {
    sr_providers::manifest()
        .get(provider_id)
        .map_or_else(|| provider_id.to_string(), |s| s.display_name.clone())
}

impl CatalogGate {
    pub fn new(cat: Arc<Catalog>, bus: Arc<EventBus>) -> Self {
        Self {
            cat,
            bus,
            needed: Arc::new(Mutex::new(None)),
            audit_tx: Mutex::new(None),
        }
    }

    /// Start the audit writer (needs a runtime). Until then, rows are written inline.
    pub(crate) fn start_audit_writer(&self) -> tokio::task::JoinHandle<()> {
        let (tx, mut rx) = mpsc::unbounded_channel::<EgressRecord>();
        *self
            .audit_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
        let cat = self.cat.clone();
        let bus = self.bus.clone();
        tokio::spawn(async move {
            while let Some(rec) = rx.recv().await {
                // M4 (page 30): the stored row is pushed as `egress.recorded`.
                let r = cat
                    .write(move |c| {
                        write_audit(c, &rec)?;
                        let id = c.last_insert_rowid();
                        Ok(sr_catalog::repo::activity::egress_after(c, id - 1, 1)?
                            .into_iter()
                            .next()
                            .map(|r| crate::activity::egress_row(c, r)))
                    })
                    .await;
                match r {
                    Ok(Some(row)) => {
                        bus.publish(sr_common::events::EngineEvent::EgressRecorded { row });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(target: "sr_engine::egress", code = %e.code, "egress audit row not written");
                    }
                }
            }
        })
    }

    /// The last purpose blocked for missing consent.
    pub fn consent_needed(&self) -> Option<ConsentRequest> {
        self.needed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn clear_needed(&self, provider: &str, purposes: &[Purpose]) {
        let mut g = self
            .needed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if g.as_ref()
            .is_some_and(|n| n.provider == provider && purposes.contains(&n.purpose))
        {
            *g = None;
        }
    }
}

fn write_audit(c: &rusqlite::Connection, rec: &EgressRecord) -> Result<(), Error> {
    let coll = rec
        .ctx
        .collection_id
        .as_deref()
        .and_then(|s| s.parse::<CollectionId>().ok());
    privacy::audit_egress(
        c,
        &rec.provider,
        rec.purpose,
        rec.bytes,
        coll,
        rec.ctx.client_id.as_deref(),
        now_ms(),
    )
}

impl PrivacyGate for CatalogGate {
    fn check(&self, provider: &str, purpose: Purpose) -> sr_common::Result<()> {
        let p = provider.to_string();
        let ok = self
            .cat
            .read(move |c| privacy::has_consent(c, &p, purpose))
            .unwrap_or(false);
        if ok {
            return Ok(());
        }
        let req = ConsentRequest {
            provider: provider.to_string(),
            purpose,
        };
        let changed = {
            let mut g = self
                .needed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let changed = g.as_ref() != Some(&req);
            *g = Some(req);
            changed
        };
        if changed {
            self.bus.publish(EngineEvent::ConsentNeeded {
                provider: provider.to_string(),
                purpose,
            });
        }
        tracing::info!(target: "sr_engine::egress", provider, ?purpose, "blocked: no consent for this purpose");
        Err(sr_providers::privacy::privacy_blocked(
            &display_name(provider),
            purpose,
        ))
    }

    fn audit(&self, provider: &str, purpose: Purpose, bytes: u64) {
        self.audit_ctx(&EgressRecord {
            provider: provider.to_string(),
            purpose,
            bytes,
            host: String::new(),
            ctx: sr_providers::EgressCtx::default(),
        });
    }

    fn audit_ctx(&self, rec: &EgressRecord) {
        let tx = self
            .audit_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match tx {
            Some(tx) if tx.send(rec.clone()).is_ok() => {}
            _ => {
                let rec = rec.clone();
                if let Err(e) = self.cat.write_blocking(move |c| write_audit(c, &rec)) {
                    tracing::warn!(target: "sr_engine::egress", code = %e.code, "egress audit row not written");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// App-facing shapes (M3-U R-Q5)
// ---------------------------------------------------------------------------------------------

/// Result of the last key check (persisted in catalog `meta`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Working,
    #[default]
    NotChecked,
    Failed,
}

/// `{code, ctx}` of the last failed check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastError {
    pub code: ErrorCode,
    /// Display context (`provider`, `host`, `retry_after`), never the key.
    pub ctx: serde_json::Value,
}

/// A model the user can pick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOut {
    pub id: String,
    pub display_name: String,
}

/// What `meta` keeps about the provider between starts (never the key).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProviderMeta {
    pub provider: String,
    pub check: CheckState,
    pub last_error: Option<LastError>,
    pub default_model: Option<String>,
    pub models: Vec<ModelOut>,
    pub models_are_hints: bool,
}

const PROVIDER_META_KEY: &str = "provider_state";

/// `providers.status`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProvidersStatus {
    pub provider: Option<String>,
    pub display_name: Option<String>,
    pub key_saved: bool,
    /// Last 4 characters of a key of 12+ characters (page 21, OQ-45).
    pub key_last4: Option<String>,
    /// `network.base_url` (local servers, or an Advanced override).
    pub address: Option<String>,
    pub check: CheckState,
    pub last_error: Option<LastError>,
    /// The user's model choice.
    pub model: Option<String>,
    /// The auto-picked model from the last check (else the registry default).
    pub default_model: Option<String>,
    pub models: Vec<ModelOut>,
    pub models_are_hints: bool,
    /// Remote embeddings / rerank are not offered by the engine yet (M3: local only).
    pub remote_embeddings: bool,
    pub remote_rerank: bool,
    /// The chat provider is connected: the app's Ask and Enhance work, MCP `context_answer` is
    /// offered when answers are on.
    pub ready: bool,
}

/// `providers.save` params. `Debug` never prints the key.
#[derive(Clone, Default, Deserialize)]
pub struct ProviderSave {
    pub provider: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub check: bool,
}

impl std::fmt::Debug for ProviderSave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSave")
            .field("provider", &self.provider)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .field("address", &self.address)
            .field("check", &self.check)
            .finish()
    }
}

/// `providers.save` result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderSaved {
    pub status: ProvidersStatus,
    pub validated: bool,
}

/// One consent row (`consent.list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConsentRow {
    pub purpose: Purpose,
    pub allowed: bool,
}

/// `consent.set` params.
#[derive(Debug, Clone, Deserialize)]
pub struct ConsentSet {
    pub provider: String,
    pub purpose: Purpose,
    pub allowed: bool,
}

/// `settings.answers` (page 23).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnswersSettings {
    pub answers_enabled: bool,
    pub deeper_answers: bool,
    pub max_rounds: u32,
}

/// `settings.update_answers` params: only the fields given change.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AnswersUpdate {
    #[serde(default)]
    pub answers_enabled: Option<bool>,
    #[serde(default)]
    pub deeper_answers: Option<bool>,
    #[serde(default)]
    pub max_rounds: Option<u32>,
}

/// Which optional MCP tools a session should list (U17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolFlags {
    pub add_folder: bool,
    pub sync_folder: bool,
    pub answer: bool,
}

/// `last_error.ctx` in the shared `ErrCtx` shape (the app decodes it as `sr_common::error::ErrCtx`):
/// the provider's own wait goes into `secs`; internal detail and paths never leave the service.
fn err_ctx(e: &Error) -> serde_json::Value {
    let mut ctx = (*e.ctx).clone();
    ctx.detail = None;
    ctx.path = None;
    if ctx.secs.is_none() {
        ctx.secs = e
            .retry_after
            .map(|d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX));
    }
    serde_json::to_value(ctx).unwrap_or(serde_json::Value::Null)
}

// ---------------------------------------------------------------------------------------------
// EngineHandle: provider operations
// ---------------------------------------------------------------------------------------------

impl EngineHandle {
    pub(crate) fn llm_state(&self) -> &LlmState {
        &self.inner().llm
    }

    /// The configured chat provider, if connected.
    pub fn llm_slot(&self) -> Option<LlmSlot> {
        self.llm_state()
            .slot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The gate every remote call goes through (also for M3-P's remote embedder).
    pub fn privacy_gate(&self) -> Arc<dyn PrivacyGate> {
        self.llm_state().gate.clone()
    }

    /// `EngineStatus.consent_needed`.
    pub(crate) fn consent_needed(&self) -> Option<ConsentRequest> {
        self.llm_state().gate.consent_needed()
    }

    /// Optional tools a session should list now (U17: `context_answer` only with a connected
    /// provider and "Let connected apps ask for answers" on; `add_folder` and `sync_folder` only
    /// when the user turned them on).
    pub fn tool_flags(&self) -> ToolFlags {
        let s = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let ready = self.llm_slot().is_some();
        ToolFlags {
            add_folder: self.mcp_add_folder_enabled(),
            sync_folder: s.integrations.mcp_sync_folder_enabled,
            answer: ready && s.answers.answers_enabled,
        }
    }

    fn provider_meta(&self) -> ProviderMeta {
        self.inner()
            .cat
            .read(|c| meta::get(c, PROVIDER_META_KEY))
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    async fn save_provider_meta(&self, m: &ProviderMeta) -> Result<(), Error> {
        let v =
            serde_json::to_string(m).map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
        self.inner()
            .cat
            .write(move |c| meta::set(c, PROVIDER_META_KEY, &v))
            .await
    }

    /// Rewrite `settings.toml` with `f` applied (atomic, `.bak` kept), then push
    /// `SettingsChanged`.
    pub(crate) async fn update_settings(
        &self,
        f: impl FnOnce(&mut sr_common::config::Settings),
    ) -> Result<sr_common::config::Settings, Error> {
        let _w = self.inner().settings_write.lock().await;
        let mut next = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        f(&mut next);
        // M5-E: policy wins over every settings change.
        self.inner().policy.apply(&mut next);
        let path = self.inner().layout.settings.clone();
        let to_save = next.clone();
        tokio::task::spawn_blocking(move || sr_common::config::save_atomic(&path, &to_save))
            .await
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))??;
        *self
            .inner()
            .settings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next.clone();
        self.inner().bus.publish(EngineEvent::SettingsChanged);
        Ok(next)
    }

    /// (Re)connect the configured provider from `settings.toml` + Credential Manager. No network
    /// call happens here. Errors leave no provider (the tools disappear) and are logged.
    pub async fn reload_provider(&self) -> Result<Option<LlmSlot>, Error> {
        let s = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let st = self.llm_state();
        // M5-E: a provider the machine policy doesn't allow is never connected.
        let allowed = s.provider.provider.as_deref().filter(|id| {
            id.is_empty()
                || self
                    .check_provider_policy(id, s.network.base_url.as_deref())
                    .is_ok()
        });
        let slot = match (&st.override_, allowed) {
            (_, None) | (_, Some("")) => None,
            (Some(o), Some(id)) if o.provider_id == id => Some(LlmSlot {
                provider_id: o.provider_id.clone(),
                display_name: o.display_name.clone(),
                model: s.provider.default_model.clone().unwrap_or_default(),
                llm: o.llm.clone(),
            }),
            (_, Some(id)) => match self.connect_remote(id, &s).await {
                Ok(slot) => Some(slot),
                Err(e) => {
                    tracing::warn!(target: "sr_engine::providers", provider = id, code = %e.code, "provider not connected");
                    None
                }
            },
        };
        let key =
            |s: &Option<LlmSlot>| s.as_ref().map(|x| (x.provider_id.clone(), x.model.clone()));
        let changed = {
            let mut g = st
                .slot
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let changed = key(&g) != key(&slot);
            *g = slot.clone();
            changed
        };
        if changed {
            // The provider tools may have appeared or gone: MCP sessions re-check their tool list
            // on this event (`tools/list_changed`), after the slot is already in place.
            self.inner().bus.publish(EngineEvent::SettingsChanged);
        }
        Ok(slot)
    }

    #[cfg(feature = "remote")]
    async fn connect_remote(
        &self,
        id: &str,
        s: &sr_common::config::Settings,
    ) -> Result<LlmSlot, Error> {
        // M5-C: "Claude (sign in)" / "Codex (sign in)" run the user's own CLI.
        let spec = sr_providers::manifest().require(id)?.clone();
        if spec.is_cli() {
            return self.connect_cli(&spec, s).await;
        }
        let h = self.remote_handle(id, None, s).await?;
        Ok(LlmSlot {
            provider_id: h.id().to_string(),
            display_name: h.display_name().to_string(),
            model: s.provider.default_model.clone().unwrap_or_default(),
            llm: Arc::new(h.resilient_llm()),
        })
    }

    #[cfg(not(feature = "remote"))]
    async fn connect_remote(
        &self,
        id: &str,
        _s: &sr_common::config::Settings,
    ) -> Result<LlmSlot, Error> {
        Err(Error::new(ErrorCode::ProviderNotConfigured)
            .with_provider(display_name(id))
            .with_detail("built without remote providers"))
    }

    /// A connected [`sr_providers::ProviderHandle`] for `id`, with `key` (or the stored key).
    #[cfg(feature = "remote")]
    async fn remote_handle(
        &self,
        id: &str,
        key: Option<&str>,
        s: &sr_common::config::Settings,
    ) -> Result<sr_providers::ProviderHandle, Error> {
        // M5-E: every connect (start, key check, answers) passes the machine policy first.
        self.check_provider_policy(id, s.network.base_url.as_deref())?;
        let spec = sr_providers::manifest().require(id)?.clone();
        let rs = sr_providers::RemoteSettings::from_settings(s);
        let gate = self.privacy_gate();
        let secrets = self.llm_state().secrets.clone();
        match key.map(str::trim).filter(|k| !k.is_empty()) {
            Some(k) => {
                let creds = secrets
                    .get(sr_providers::secrets::PROXY_ACCOUNT)?
                    .as_deref()
                    .and_then(sr_providers::remote::proxy::parse_credentials);
                sr_providers::ProviderHandle::connect(
                    &spec,
                    sr_providers::ApiKey::new(k),
                    rs,
                    gate,
                    creds,
                )
                .await
            }
            None => {
                sr_providers::ProviderHandle::connect_with_store(&spec, &*secrets, rs, gate).await
            }
        }
    }

    /// `providers.status`.
    pub fn providers_status(&self) -> ProvidersStatus {
        let s = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let id = s.provider.provider.clone().filter(|p| !p.is_empty());
        let m = self.provider_meta();
        let m = if id.as_deref() == Some(m.provider.as_str()) {
            m
        } else {
            ProviderMeta::default()
        };
        let key = id.as_deref().and_then(|p| {
            self.llm_state()
                .secrets
                .get(&sr_providers::secrets::provider_account(p))
                .ok()
                .flatten()
        });
        let spec = id.as_deref().and_then(|p| sr_providers::manifest().get(p));
        // A CLI sign-in row's models come from the provider table, never from the provider (its
        // CLI can't be asked): shown from the table itself, so a refreshed list and default
        // reach a saved provider without [Check again] (Amendment 11). The user's own choice
        // (`model`) is kept as saved.
        let cli = spec.filter(|x| x.is_cli());
        let (default_model, models) = match cli {
            Some(x) => (
                x.default_model.clone(),
                x.model_hints()
                    .into_iter()
                    .map(|h| ModelOut {
                        id: h.id,
                        display_name: h.display_name,
                    })
                    .collect(),
            ),
            None => (
                m.default_model
                    .clone()
                    .or_else(|| spec.and_then(|x| x.default_model.clone())),
                m.models.clone(),
            ),
        };
        ProvidersStatus {
            display_name: spec.map(|x| x.display_name.clone()),
            key_saved: key.as_deref().is_some_and(|k| !k.trim().is_empty()),
            key_last4: key.as_deref().and_then(sr_providers::secrets::key_hint),
            address: s.network.base_url.clone().filter(|u| !u.trim().is_empty()),
            check: m.check,
            last_error: m.last_error.clone(),
            model: s.provider.default_model.clone(),
            default_model,
            models,
            models_are_hints: m.models_are_hints || cli.is_some(),
            remote_embeddings: false,
            remote_rerank: false,
            ready: self.llm_slot().is_some(),
            provider: id,
        }
    }

    /// `providers.save`: store the key and the provider; with `check`, validate the key live and
    /// list the models first (the click is the consent for those two purposes, D-35). The key is
    /// stored only after the provider accepted it.
    pub async fn save_provider(&self, p: ProviderSave) -> Result<ProviderSaved, Error> {
        let spec = sr_providers::manifest().require(p.provider.trim())?.clone();
        spec.ensure_enabled()?;
        if let Some(a) = p
            .address
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            && !sr_common::config::is_valid_provider_url(a)
        {
            return Err(Error::new(ErrorCode::SettingsInvalid)
                .with_section(sr_common::error::Section::Providers)
                .with_detail("provider address must be https, or http on this computer"));
        }
        let address = p
            .address
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string);
        let id = spec.id.clone();
        // `network.base_url` applies to the configured provider only: a new provider never
        // inherits the previous one's address (its key would be sent there).
        let (cur_provider, cur_url) = {
            let s = self
                .inner()
                .settings
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (s.provider.provider.clone(), s.network.base_url.clone())
        };
        let address = match address {
            // A CLI sign-in provider has no address (its program talks to its own service).
            _ if spec.is_cli() => None,
            Some(a) => Some(a),
            None if cur_provider.as_deref() == Some(id.as_str()) => cur_url,
            None => None,
        };
        // M5-E: refused before the key is sent anywhere or stored.
        self.check_provider_policy(&id, address.as_deref())?;
        let mut meta = ProviderMeta {
            provider: id.clone(),
            ..ProviderMeta::default()
        };
        let mut validated = false;
        if p.check {
            // [Check and save] = consent for the key check and the model list (D-35).
            let now = now_ms();
            let id2 = id.clone();
            self.inner()
                .cat
                .write(move |c| {
                    for pu in sr_providers::consent_group(Purpose::ValidateKey) {
                        privacy::grant_consent(c, &id2, *pu, now)?;
                    }
                    Ok(())
                })
                .await?;
            match self
                .check_with(&id, p.key.as_deref(), address.clone())
                .await
            {
                Ok(m) => {
                    meta = m;
                    validated = true;
                }
                Err(e) => {
                    meta.check = CheckState::Failed;
                    meta.last_error = Some(LastError {
                        code: e.code,
                        ctx: err_ctx(&e),
                    });
                    self.save_provider_meta(&meta).await?;
                    return Err(e);
                }
            }
        } else if spec.is_cli() {
            // M5-C: no key to keep; the CLI's own sign-in is used.
        } else if let Some(k) = p.key.as_deref().filter(|k| !k.trim().is_empty()) {
            // "Save without checking" (offline).
            self.save_key_unchecked(&id, k)?;
        }
        self.save_provider_meta(&meta).await?;
        let addr2 = address.clone();
        self.update_settings(move |s| {
            s.provider.provider = Some(id.clone());
            // Page 23: answers are on once a provider is added.
            s.answers.answers_enabled = true;
            s.network.base_url = addr2;
        })
        .await?;
        self.reload_provider().await?;
        Ok(ProviderSaved {
            status: self.providers_status(),
            validated,
        })
    }

    #[cfg(feature = "remote")]
    fn save_key_unchecked(&self, id: &str, key: &str) -> Result<(), Error> {
        sr_providers::remote::discovery::save_unchecked(&*self.llm_state().secrets, id, key)
    }

    #[cfg(not(feature = "remote"))]
    fn save_key_unchecked(&self, id: &str, key: &str) -> Result<(), Error> {
        let k = key.trim();
        if k.is_empty() {
            return Err(Error::new(ErrorCode::InvalidInput).with_detail("empty key"));
        }
        self.llm_state()
            .secrets
            .set(&sr_providers::secrets::provider_account(id), k)
    }

    /// Validate `key` (or the stored one) and list models; stores the key on success.
    #[cfg(feature = "remote")]
    async fn check_with(
        &self,
        id: &str,
        key: Option<&str>,
        address: Option<String>,
    ) -> Result<ProviderMeta, Error> {
        let mut s = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // Exactly the address this provider will be saved with (never another provider's).
        s.network.base_url = address;
        // M5-C: a CLI sign-in provider is checked by asking its CLI (nothing is sent anywhere).
        let spec = sr_providers::manifest().require(id)?.clone();
        if spec.is_cli() {
            self.check_provider_policy(id, None)?;
            let (default_model, models) = self.check_cli(&spec).await?;
            return Ok(ProviderMeta {
                provider: id.to_string(),
                check: CheckState::Working,
                last_error: None,
                default_model,
                models: models
                    .into_iter()
                    .map(|m| ModelOut {
                        id: m.id,
                        display_name: m.display_name,
                    })
                    .collect(),
                models_are_hints: true,
            });
        }
        let h = self.remote_handle(id, key, &s).await?;
        let cancel = self.inner().shutdown.child_token();
        let kc = sr_providers::remote::check_key(&h, &*self.llm_state().secrets, &cancel).await?;
        Ok(ProviderMeta {
            provider: id.to_string(),
            check: CheckState::Working,
            last_error: None,
            default_model: kc.default_model,
            models: kc
                .models
                .into_iter()
                .map(|m| ModelOut {
                    id: m.id,
                    display_name: m.display_name,
                })
                .collect(),
            models_are_hints: kc.models_are_hints,
        })
    }

    /// Without remote providers, the override (tests) "validates" by listing its models.
    #[cfg(not(feature = "remote"))]
    async fn check_with(
        &self,
        id: &str,
        key: Option<&str>,
        _address: Option<String>,
    ) -> Result<ProviderMeta, Error> {
        let Some(o) = self
            .llm_state()
            .override_
            .clone()
            .filter(|o| o.provider_id == id)
        else {
            return Err(Error::new(ErrorCode::ProviderNotConfigured)
                .with_provider(display_name(id))
                .with_detail("built without remote providers"));
        };
        let cancel = self.inner().shutdown.child_token();
        o.llm.validate_key(&cancel).await?;
        if let Some(k) = key.filter(|k| !k.trim().is_empty()) {
            self.save_key_unchecked(id, k)?;
        }
        let models = o.llm.list_models(&cancel).await.unwrap_or_default();
        Ok(ProviderMeta {
            provider: id.to_string(),
            check: CheckState::Working,
            last_error: None,
            default_model: models.first().map(|m| m.id.clone()),
            models: models
                .into_iter()
                .map(|m| ModelOut {
                    id: m.id,
                    display_name: m.display_name,
                })
                .collect(),
            models_are_hints: false,
        })
    }

    /// `providers.check`: re-validate the saved key.
    pub async fn check_provider(&self) -> Result<ProvidersStatus, Error> {
        let id = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .provider
            .provider
            .clone()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| Error::new(ErrorCode::ProviderNotConfigured))?;
        let url = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .network
            .base_url
            .clone();
        let meta = match self.check_with(&id, None, url).await {
            Ok(m) => m,
            Err(e) => {
                let mut m = self.provider_meta();
                m.provider = id.clone();
                m.check = CheckState::Failed;
                m.last_error = Some(LastError {
                    code: e.code,
                    ctx: err_ctx(&e),
                });
                self.save_provider_meta(&m).await?;
                return Err(e);
            }
        };
        self.save_provider_meta(&meta).await?;
        self.reload_provider().await?;
        Ok(self.providers_status())
    }

    /// `providers.set_model`: `None` = automatic (the checked default).
    pub async fn set_provider_model(
        &self,
        model: Option<String>,
    ) -> Result<ProvidersStatus, Error> {
        let model = model
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        self.update_settings(move |s| s.provider.default_model = model)
            .await?;
        self.reload_provider().await?;
        Ok(self.providers_status())
    }

    /// `providers.remove`: delete the key, clear `[provider]`, drop the consents; the provider
    /// tools disappear (`tools/list_changed`).
    pub async fn remove_provider(&self) -> Result<ProvidersStatus, Error> {
        let id = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .provider
            .provider
            .clone()
            .filter(|p| !p.is_empty());
        if let Some(id) = id {
            let acct = sr_providers::secrets::provider_account(&id);
            if let Err(e) = self.llm_state().secrets.delete(&acct) {
                tracing::warn!(target: "sr_engine::providers", code = %e.code, "could not delete the saved key");
            }
            let id2 = id.clone();
            self.inner()
                .cat
                .write(move |c| {
                    for p in Purpose::ALL {
                        privacy::revoke_consent(c, &id2, *p)?;
                    }
                    meta::delete(c, PROVIDER_META_KEY)
                })
                .await?;
            self.llm_state().gate.clear_needed(&id, Purpose::ALL);
        }
        self.update_settings(|s| {
            s.provider.provider = None;
            s.provider.default_model = None;
            s.network.base_url = None;
            s.answers.answers_enabled = false;
            s.answers.deeper_answers = false;
        })
        .await?;
        self.reload_provider().await?;
        Ok(self.providers_status())
    }

    /// `consent.list`: every purpose for the configured provider.
    pub async fn consent_list(&self) -> Result<Vec<ConsentRow>, Error> {
        let Some(id) = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .provider
            .provider
            .clone()
            .filter(|p| !p.is_empty())
        else {
            return Ok(Vec::new());
        };
        self.inner()
            .cat
            .read_async(move |c| {
                Purpose::ALL
                    .iter()
                    .map(|p| {
                        Ok(ConsentRow {
                            purpose: *p,
                            allowed: privacy::has_consent(c, &id, *p)?,
                        })
                    })
                    .collect()
            })
            .await
    }

    /// `consent.set`: one switch covers its consent group (answer covers iterate, OQ-34).
    pub async fn consent_set(&self, s: ConsentSet) -> Result<Vec<ConsentRow>, Error> {
        let group: Vec<Purpose> = sr_providers::consent_group(s.purpose).to_vec();
        let (id, allowed, g2) = (s.provider.clone(), s.allowed, group.clone());
        let now = now_ms();
        self.inner()
            .cat
            .write(move |c| {
                for p in &g2 {
                    if allowed {
                        privacy::grant_consent(c, &id, *p, now)?;
                    } else {
                        privacy::revoke_consent(c, &id, *p)?;
                    }
                }
                Ok(())
            })
            .await?;
        // Allowed or declined, the pending question is answered: clear it so a reconnect doesn't
        // ask again and the next blocked call asks afresh (M3 tester: "Don't allow" kept it).
        self.llm_state().gate.clear_needed(&s.provider, &group);
        for p in group {
            self.inner().bus.publish(EngineEvent::ConsentChanged {
                provider: s.provider.clone(),
                purpose: p,
                allowed: s.allowed,
            });
        }
        self.consent_list().await
    }

    /// `settings.answers`.
    pub fn answers_settings(&self) -> AnswersSettings {
        let s = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        AnswersSettings {
            answers_enabled: s.answers.answers_enabled,
            deeper_answers: s.answers.deeper_answers,
            max_rounds: s.answers.max_rounds.clamp(1, 5),
        }
    }

    /// `settings.update_answers` (pushes `SettingsChanged` → `tools/list_changed`).
    pub async fn update_answers(&self, u: AnswersUpdate) -> Result<AnswersSettings, Error> {
        if let Some(r) = u.max_rounds
            && !(1..=5).contains(&r)
        {
            return Err(Error::new(ErrorCode::InvalidInput).with_detail("max_rounds must be 1-5"));
        }
        self.update_settings(move |s| {
            if let Some(v) = u.answers_enabled {
                s.answers.answers_enabled = v;
            }
            if let Some(v) = u.deeper_answers {
                s.answers.deeper_answers = v;
            }
            if let Some(v) = u.max_rounds {
                s.answers.max_rounds = v;
            }
        })
        .await?;
        Ok(self.answers_settings())
    }
}

/// The production secret store: Windows Credential Manager when available, else memory (tests,
/// builds without the keyring feature).
pub(crate) fn default_secrets() -> Arc<dyn SecretStore> {
    #[cfg(feature = "remote")]
    {
        match sr_providers::KeyringStore::new() {
            Ok(k) => return Arc::new(k),
            Err(e) => {
                tracing::error!(target: "sr_engine::providers", code = %e.code, "Credential Manager unavailable; keys are kept in memory only");
            }
        }
    }
    Arc::new(sr_providers::MemorySecretStore::new())
}
