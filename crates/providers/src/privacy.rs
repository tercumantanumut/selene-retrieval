//! Egress consent and secrets (§2.5, §2.6). Remote providers call the gate before every
//! request; **there is no fallback across the privacy boundary**.
//!
//! M3-P: every remote HTTP attempt goes through [`crate::remote::egress::Egress`], which calls
//! [`PrivacyGate::check`] before a single byte leaves the process and
//! [`PrivacyGate::audit_ctx`] for each attempt that is sent (retries are separate egress and are
//! audited separately). The store behind the trait is the catalog (`egress_consent`,
//! `egress_audit`); the engine implements the trait over `sr_catalog::repo::privacy`.

use std::collections::HashSet;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sr_common::Result;
use sr_common::error::{Error, ErrorCode, Purpose};

/// Who asked, for the audit row (`egress_audit.collection_id`, `client_id`). Filled by the
/// engine: per request for chat (`ChatRequest::egress`), per embedder instance for embeddings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressCtx {
    /// Collection id (ULID string) the data came from, if any.
    pub collection_id: Option<String>,
    /// MCP / app client id that triggered the call, if any.
    pub client_id: Option<String>,
}

/// One audited egress (one HTTP request sent to a provider).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressRecord {
    /// Provider id (`openrouter`), the same key as `egress_consent.provider`.
    pub provider: String,
    pub purpose: Purpose,
    /// Request body bytes sent (0 for GETs).
    pub bytes: u64,
    /// Host the request went to (diagnostics; not a catalog column).
    pub host: String,
    pub ctx: EgressCtx,
}

/// Consent check before any remote call.
pub trait PrivacyGate: Send + Sync {
    /// `Err(PrivacyBlocked)` unless the user allowed `purpose` for `provider` (provider **id**).
    fn check(&self, provider: &str, purpose: Purpose) -> Result<()>;
    /// Append to `egress_audit`.
    fn audit(&self, provider: &str, purpose: Purpose, bytes: u64);
    /// Append to `egress_audit` with the collection / client context. The default drops the
    /// context; the engine's catalog-backed gate overrides it.
    fn audit_ctx(&self, rec: &EgressRecord) {
        self.audit(&rec.provider, rec.purpose, rec.bytes);
    }
}

impl<G: PrivacyGate + ?Sized> PrivacyGate for std::sync::Arc<G> {
    fn check(&self, provider: &str, purpose: Purpose) -> Result<()> {
        (**self).check(provider, purpose)
    }
    fn audit(&self, provider: &str, purpose: Purpose, bytes: u64) {
        (**self).audit(provider, purpose, bytes)
    }
    fn audit_ctx(&self, rec: &EgressRecord) {
        (**self).audit_ctx(rec)
    }
}

/// `PrivacyBlocked` (#26) for (`provider` display name, `purpose`).
pub fn privacy_blocked(provider_display: &str, purpose: Purpose) -> Error {
    let mut e = Error::new(ErrorCode::PrivacyBlocked).with_provider(provider_display);
    e.ctx.purpose = Some(purpose);
    e
}

/// Gate that refuses every remote call (the default until consent exists).
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalOnly;

impl PrivacyGate for LocalOnly {
    fn check(&self, provider: &str, purpose: Purpose) -> Result<()> {
        Err(privacy_blocked(provider, purpose))
    }
    fn audit(&self, _provider: &str, _purpose: Purpose, _bytes: u64) {}
}

/// Callback that persists one audit row (the engine queues it to the catalog writer).
pub type AuditSink = Box<dyn Fn(&EgressRecord) + Send + Sync>;

/// In-memory consent gate.
///
/// * Tests: `MemoryGate::new()` keeps every audited record ([`records`](Self::records)).
/// * Engine: `MemoryGate::with_sink(grants, sink)` is the production shape. `check` is
///   synchronous and must not touch SQLite on every request, so the engine loads the
///   `egress_consent` rows once, mirrors every `consent.set` with [`set`](Self::set), and passes a
///   sink that queues `egress_audit` rows to the catalog writer. Nothing is kept in memory then.
#[derive(Default)]
pub struct MemoryGate {
    granted: Mutex<HashSet<(String, Purpose)>>,
    log: Mutex<Vec<EgressRecord>>,
    sink: Option<AuditSink>,
}

impl std::fmt::Debug for MemoryGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryGate")
            .field("granted", &self.granted)
            .field("sink", &self.sink.is_some())
            .finish_non_exhaustive()
    }
}

impl MemoryGate {
    pub fn new() -> Self {
        Self::default()
    }
    /// Production gate: initial `(provider id, purpose)` grants from the catalog, audit rows go
    /// to `sink`.
    pub fn with_sink(grants: impl IntoIterator<Item = (String, Purpose)>, sink: AuditSink) -> Self {
        Self {
            granted: Mutex::new(grants.into_iter().collect()),
            log: Mutex::new(Vec::new()),
            sink: Some(sink),
        }
    }
    /// Consent for (`provider`, every purpose in `purposes`).
    pub fn grant(&self, provider: &str, purposes: &[Purpose]) {
        if let Ok(mut g) = self.granted.lock() {
            for p in purposes {
                g.insert((provider.to_string(), *p));
            }
        }
    }
    pub fn revoke(&self, provider: &str, purpose: Purpose) {
        if let Ok(mut g) = self.granted.lock() {
            g.remove(&(provider.to_string(), purpose));
        }
    }
    /// Mirrors `consent.set`: the whole [`consent_group`] of `purpose` is switched together.
    pub fn set(&self, provider: &str, purpose: Purpose, allowed: bool) {
        for p in consent_group(purpose) {
            if allowed {
                self.grant(provider, &[*p]);
            } else {
                self.revoke(provider, *p);
            }
        }
    }
    /// Drops every consent of `provider` ("Remove provider…").
    pub fn revoke_all(&self, provider: &str) {
        if let Ok(mut g) = self.granted.lock() {
            g.retain(|(p, _)| p != provider);
        }
    }
    pub fn is_allowed(&self, provider: &str, purpose: Purpose) -> bool {
        self.granted
            .lock()
            .map(|g| g.contains(&(provider.to_string(), purpose)))
            .unwrap_or(false)
    }
    /// Audited records (only kept when there is no sink).
    pub fn records(&self) -> Vec<EgressRecord> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

impl PrivacyGate for MemoryGate {
    fn check(&self, provider: &str, purpose: Purpose) -> Result<()> {
        if self.is_allowed(provider, purpose) {
            Ok(())
        } else {
            Err(privacy_blocked(provider, purpose))
        }
    }
    fn audit(&self, provider: &str, purpose: Purpose, bytes: u64) {
        self.audit_ctx(&EgressRecord {
            provider: provider.to_string(),
            purpose,
            bytes,
            host: String::new(),
            ctx: EgressCtx::default(),
        });
    }
    fn audit_ctx(&self, rec: &EgressRecord) {
        match &self.sink {
            Some(sink) => sink(rec),
            None => {
                if let Ok(mut l) = self.log.lock() {
                    l.push(rec.clone());
                }
            }
        }
    }
}

/// Purposes that one consent dialog / switch covers (page 22, OQ-34: answer and iterate are one
/// purpose for the user; the wizard's [Check and save] covers key check + model list, D-35).
pub fn consent_group(purpose: Purpose) -> &'static [Purpose] {
    match purpose {
        Purpose::Answer | Purpose::Iterate => &[Purpose::Answer, Purpose::Iterate],
        Purpose::ValidateKey | Purpose::ListModels => &[Purpose::ValidateKey, Purpose::ListModels],
        Purpose::EmbedDoc => &[Purpose::EmbedDoc],
        Purpose::EmbedQuery => &[Purpose::EmbedQuery],
        Purpose::Rerank => &[Purpose::Rerank],
        Purpose::Enhance => &[Purpose::Enhance],
    }
}

/// API keys and tokens (Windows Credential Manager via keyring-core, M3). Never in settings.toml.
pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn set(&self, account: &str, secret: &str) -> Result<()>;
    fn delete(&self, account: &str) -> Result<()>;
}

impl<S: SecretStore + ?Sized> SecretStore for std::sync::Arc<S> {
    fn get(&self, account: &str) -> Result<Option<String>> {
        (**self).get(account)
    }
    fn set(&self, account: &str, secret: &str) -> Result<()> {
        (**self).set(account, secret)
    }
    fn delete(&self, account: &str) -> Result<()> {
        (**self).delete(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn grouped_consent_and_sink() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let g = MemoryGate::with_sink(
            [("openrouter".to_string(), Purpose::ValidateKey)],
            Box::new(move |r| {
                if let Ok(mut v) = s2.lock() {
                    v.push(r.purpose);
                }
            }),
        );
        assert!(g.check("openrouter", Purpose::ValidateKey).is_ok());
        assert!(g.check("openrouter", Purpose::Iterate).is_err());
        g.set("openrouter", Purpose::Answer, true);
        assert!(
            g.check("openrouter", Purpose::Iterate).is_ok(),
            "answer covers iterate"
        );
        g.audit("openrouter", Purpose::Answer, 10);
        assert_eq!(seen.lock().map(|v| v.len()).unwrap_or(0), 1);
        assert!(g.records().is_empty(), "nothing kept in memory with a sink");
        g.revoke_all("openrouter");
        assert!(g.check("openrouter", Purpose::ValidateKey).is_err());
        let e = LocalOnly.check("OpenRouter", Purpose::EmbedDoc).err();
        assert_eq!(e.map(|e| e.code), Some(ErrorCode::PrivacyBlocked));
    }
}
