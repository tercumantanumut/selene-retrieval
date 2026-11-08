//! Live key validation and model discovery (pages 20/21; M3 first-run check "paste one key:
//! validated, models auto-listed, default chosen").
//!
//! * **Model list.** Providers with `discovery = live` (OpenRouter, Ollama, vLLM) are asked
//!   (`GET {base}models`, purpose `list_models`). The others return the registry's default and
//!   utility models as hints, with **no** network call; the UI shows them as "e.g. …" and still
//!   accepts any model id.
//! * **Key check** (purpose `validate_key`), by the manifest's `validation`:
//!   `models` = `GET {base}models` with the key (free); `openrouter_key` = `GET {base}key`
//!   (OpenRouter's model list is public, so it can't reject a key); `chat_probe` = one-token chat
//!   with the utility model (at most one output token is billed).
//! * [`check_key`] is the wizard's [Check and save]: check → store the key in Credential Manager
//!   → list models → pick the default. The click is the consent for `validate_key` +
//!   `list_models` (D-35): the caller records it **before** calling.

use serde::{Deserialize, Serialize};
use sr_common::Result;
use sr_common::error::{Error, ErrorCode, Purpose};
use tokio_util::sync::CancellationToken;

use super::{ProviderHandle, errors};
use crate::llm::ModelInfo;
use crate::privacy::{EgressCtx, SecretStore};
use crate::registry::{Discovery, ProviderSpec, Validation};
use crate::secrets::provider_account;

/// Registry hints: the row's model list (CLI sign-in rows), else the default model then the
/// utility model (deduplicated). See [`ProviderSpec::model_hints`].
pub fn registry_models(spec: &ProviderSpec) -> Vec<ModelInfo> {
    spec.model_hints()
}

/// Parses an OpenAI-style `{"data":[{"id":…}]}` model list (OpenRouter adds `name` and
/// `context_length`; Anthropic uses `display_name` and pages with `has_more`). Ollama's native
/// `{"models":[{"name":…}]}` is accepted as well.
pub fn parse_models(json: &serde_json::Value) -> Vec<ModelInfo> {
    let rows = json
        .get("data")
        .or_else(|| json.get("models"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<ModelInfo> = Vec::with_capacity(rows.len());
    for r in rows {
        let Some(id) = r
            .get("id")
            .or_else(|| r.get("model"))
            .or_else(|| r.get("name"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if out.iter().any(|m| m.id == id) {
            continue;
        }
        let display = r
            .get("name")
            .or_else(|| r.get("display_name"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(id);
        let ctx = r
            .get("context_length")
            .or_else(|| r.get("context_window"))
            .or_else(|| r.get("max_model_len"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok());
        out.push(ModelInfo {
            id: id.to_string(),
            display_name: display.to_string(),
            context_tokens: ctx,
        });
    }
    out
}

/// The default answer model: the registry default if the provider offers it (or if we can't
/// tell), else the first listed model. `None` only for an empty list with no registry default.
pub fn pick_default_model(spec: &ProviderSpec, models: &[ModelInfo]) -> Option<String> {
    match &spec.default_model {
        Some(d) if models.is_empty() || models.iter().any(|m| &m.id == d) => Some(d.clone()),
        _ => models.first().map(|m| m.id.clone()),
    }
}

/// `GET url` through the egress door; returns the JSON body of a 2xx answer.
async fn get_json(
    h: &ProviderHandle,
    purpose: Purpose,
    url: &str,
    cancel: &CancellationToken,
) -> Result<serde_json::Value> {
    let site = h.site_for(url);
    h.inner
        .egress
        .call(
            purpose,
            0,
            &site.host,
            &EgressCtx::default(),
            cancel,
            || async {
                let resp = h
                    .get(url)
                    .send()
                    .await
                    .map_err(|e| errors::from_reqwest(&site.site(), &e))?;
                let status = resp.status();
                let headers = resp.headers().clone();
                let body = resp
                    .text()
                    .await
                    .map_err(|e| errors::from_reqwest(&site.site(), &e))?;
                if !status.is_success() {
                    return Err(errors::from_status(
                        &site.site(),
                        status.as_u16(),
                        &headers,
                        &body,
                    ));
                }
                serde_json::from_str(&body).map_err(|_| {
                    let html = body.trim_start().starts_with('<');
                    Error::new(if html {
                        ErrorCode::NetworkBlocked
                    } else {
                        ErrorCode::ProviderUnavailable
                    })
                    .with_provider(site.provider.clone())
                    .with_detail(format!("{} did not answer with JSON", site.host))
                })
            },
        )
        .await
}

/// Models offered by the provider (live or registry hints, see the module docs).
pub async fn list_models(h: &ProviderHandle, cancel: &CancellationToken) -> Result<Vec<ModelInfo>> {
    match h.spec().discovery {
        Discovery::Registry => Ok(registry_models(h.spec())),
        Discovery::Live => {
            let url = format!("{}models", h.base_url_for("")?);
            let json = get_json(h, Purpose::ListModels, &url, cancel).await?;
            Ok(parse_models(&json))
        }
    }
}

/// Live key check (`Ok(())` = the provider accepted the key / the local server answered).
pub async fn validate_key(h: &ProviderHandle, cancel: &CancellationToken) -> Result<()> {
    let base = h.base_url_for("")?;
    match h.spec().validation {
        Some(Validation::Models) => {
            get_json(h, Purpose::ValidateKey, &format!("{base}models"), cancel).await?;
            Ok(())
        }
        Some(Validation::OpenrouterKey) => {
            get_json(h, Purpose::ValidateKey, &format!("{base}key"), cancel).await?;
            Ok(())
        }
        Some(Validation::ChatProbe) => super::chat::probe(h, cancel).await,
        // CLI sign-in providers never get an HTTP handle (`ProviderHandle::connect` refuses).
        Some(Validation::CliStatus) | None => Err(Error::new(ErrorCode::ProviderNotConfigured)
            .with_provider(h.display_name().to_string())
            .with_detail("no key check for this provider")),
    }
}

/// Result of [Check and save].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCheck {
    /// "ends in 1234" (None for short keys and keyless local providers).
    pub key_hint: Option<String>,
    pub models: Vec<ModelInfo>,
    /// Auto-picked answer model ("Key works. Answers will use {model}.").
    pub default_model: Option<String>,
    /// The models are registry hints, not a live list.
    pub models_are_hints: bool,
}

/// The wizard / Providers & Keys [Check and save]: validate the key live, then store it in
/// `secrets`, then list models and pick the default. The key is stored **only** after the
/// provider accepted it; on `Offline` the caller may offer "Save without checking"
/// ([`save_unchecked`]). Consent for `validate_key` and `list_models` must already be recorded.
pub async fn check_key(
    h: &ProviderHandle,
    secrets: &dyn SecretStore,
    cancel: &CancellationToken,
) -> Result<KeyCheck> {
    validate_key(h, cancel).await?;
    let key = &h.inner.key;
    if !key.is_empty() {
        secrets.set(&provider_account(h.id()), key.expose())?;
    }
    // A failed model list after a good key is not fatal: fall back to the registry hints.
    let (models, hints) = match list_models(h, cancel).await {
        Ok(m) => (m, h.spec().discovery == Discovery::Registry),
        Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
        Err(e) => {
            tracing::info!(code = %e.code, provider = h.id(), "model list failed after a good key");
            (registry_models(h.spec()), true)
        }
    };
    Ok(KeyCheck {
        key_hint: key.hint(),
        default_model: pick_default_model(h.spec(), &models),
        models,
        models_are_hints: hints,
    })
}

/// "Save without checking" (page 20, Offline state): stores the key unvalidated.
pub fn save_unchecked(secrets: &dyn SecretStore, provider_id: &str, key: &str) -> Result<()> {
    let k = key.trim();
    if k.is_empty() {
        return Err(Error::new(ErrorCode::InvalidInput).with_detail("empty key"));
    }
    secrets.set(&provider_account(provider_id), k)
}

/// "Remove provider…" (page 21): deletes the key from Credential Manager. Consent rows and
/// settings are the engine's part.
pub fn forget_key(secrets: &dyn SecretStore, provider_id: &str) -> Result<()> {
    secrets.delete(&provider_account(provider_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::manifest;

    #[test]
    fn parses_model_lists() {
        let or = serde_json::json!({"data":[
            {"id":"openrouter/auto","name":"Auto Router","context_length":2000000},
            {"id":"google/gemini-2.5-flash","name":"Gemini 2.5 Flash","context_length":1048576},
            {"id":"openrouter/auto"}
        ]});
        let m = parse_models(&or);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].display_name, "Auto Router");
        assert_eq!(m[1].context_tokens, Some(1_048_576));
        let ollama =
            serde_json::json!({"object":"list","data":[{"id":"llama3.1:8b","object":"model"}]});
        assert_eq!(parse_models(&ollama)[0].id, "llama3.1:8b");
        let native = serde_json::json!({"models":[{"name":"nomic-embed-text:latest"}]});
        assert_eq!(parse_models(&native)[0].id, "nomic-embed-text:latest");
        assert!(parse_models(&serde_json::json!({"error":"x"})).is_empty());
    }

    #[test]
    fn default_model_choice() {
        let Some(spec) = manifest().get("ollama") else {
            return;
        };
        let listed = |ids: &[&str]| -> Vec<ModelInfo> {
            ids.iter()
                .map(|i| ModelInfo {
                    id: (*i).into(),
                    display_name: (*i).into(),
                    context_tokens: None,
                })
                .collect()
        };
        let d = spec.default_model.clone();
        assert_eq!(pick_default_model(spec, &[]), d);
        assert_eq!(
            pick_default_model(spec, &listed(&["qwen3:4b", "mistral"])).as_deref(),
            Some("qwen3:4b")
        );
        if let Some(d) = &d {
            assert_eq!(
                pick_default_model(spec, &listed(&["x", d])).as_ref(),
                Some(d)
            );
        }
        let Some(a) = manifest().get("anthropic") else {
            return;
        };
        let hints = registry_models(a);
        assert_eq!(hints.first().map(|m| &m.id), a.default_model.as_ref());
    }
}
