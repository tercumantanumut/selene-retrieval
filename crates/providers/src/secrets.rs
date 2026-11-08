//! Secrets: API keys and proxy credentials live **only** in Windows Credential Manager
//! (P1.7, §2.6). Never in `settings.toml`, never in logs, never in error text.
//!
//! * Every credential is a *generic* credential named `SeleneRetrieval/<account>`
//!   ([`TARGET_PREFIX`]), persistence **Local** (stays on this computer; the keyring default
//!   `Enterprise` would roam with a domain profile).
//! * Accounts: `provider:<id>` for a provider key ([`provider_account`]), `proxy` for the
//!   proxy's `user:password` ([`PROXY_ACCOUNT`]).
//! * [`purge_all`] deletes every `SeleneRetrieval/*` credential: "Delete all my data" and the
//!   uninstaller (see the final report for the installer hook).
//! * [`ApiKey`] redacts itself in `Debug`/`Display`; [`key_hint`] gives the "ends in 1234" text.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use sr_common::Result;

use crate::privacy::SecretStore;

/// Target-name prefix of every credential we create. Stable: the uninstaller matches on it.
pub const TARGET_PREFIX: &str = "SeleneRetrieval/";
/// Account of the proxy credentials (`user:password`, Basic auth).
pub const PROXY_ACCOUNT: &str = "proxy";

/// Account of a provider's API key.
pub fn provider_account(provider_id: &str) -> String {
    format!("provider:{provider_id}")
}

/// Full Credential Manager target name of `account`.
pub fn target_name(account: &str) -> String {
    format!("{TARGET_PREFIX}{account}")
}

/// An API key. `Debug` and `Display` never print it.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ApiKey(String);

impl ApiKey {
    /// Trims whitespace (pasted keys often carry a newline).
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into().trim().to_string())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// "ends in …" text for page 21.
    pub fn hint(&self) -> Option<String> {
        key_hint(&self.0)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() {
            "ApiKey(<empty>)"
        } else {
            "ApiKey(<redacted>)"
        })
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Last 4 characters for "Saved · ends in {last4}" (page 21, OQ-45). `None` for keys shorter
/// than 12 characters, where 4 characters would give away too much of the key.
pub fn key_hint(secret: &str) -> Option<String> {
    let chars: Vec<char> = secret.trim().chars().collect();
    (chars.len() >= 12).then(|| chars[chars.len() - 4..].iter().collect())
}

/// Removes `secret` (and any long token-looking run) from provider text before it goes to an
/// error's Details. Provider error bodies sometimes echo part of the key.
pub fn scrub(text: &str, secret: Option<&str>) -> String {
    let mut s = text.to_string();
    if let Some(k) = secret.filter(|k| k.len() >= 4) {
        s = s.replace(k, "<redacted>");
    }
    // Long runs of key alphabet (sk-…, bearer tokens, JWTs) are replaced as well.
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 24 && run.chars().any(|c| c.is_ascii_digit()) {
            out.push_str("<redacted>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// In-memory store for tests and for the app's "Save without checking" preview (never used for
/// real keys in the daemon).
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    map: Mutex<HashMap<String, String>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn accounts(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .map
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        v.sort();
        v
    }
}

impl SecretStore for MemorySecretStore {
    fn get(&self, account: &str) -> Result<Option<String>> {
        Ok(self.map.lock().ok().and_then(|m| m.get(account).cloned()))
    }
    fn set(&self, account: &str, secret: &str) -> Result<()> {
        if let Ok(mut m) = self.map.lock() {
            m.insert(account.to_string(), secret.to_string());
        }
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<()> {
        if let Ok(mut m) = self.map.lock() {
            m.remove(account);
        }
        Ok(())
    }
}

#[cfg(feature = "keyring")]
pub use credman::{KeyringStore, purge_all};

#[cfg(feature = "keyring")]
mod credman {
    use std::collections::HashMap;
    use std::sync::Arc;

    use keyring_core::api::CredentialStoreApi;
    use keyring_core::{Entry, Error as KError};
    use sr_common::error::{Error, ErrorCode};

    use super::*;

    /// Windows Credential Manager store.
    pub struct KeyringStore {
        store: Arc<windows_native_keyring_store::Store>,
    }

    impl fmt::Debug for KeyringStore {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("KeyringStore(Windows Credential Manager)")
        }
    }

    /// Error without any secret material: only the variant name goes to Details
    /// (`BadEncoding` / `BadDataFormat` carry raw secret bytes and are never printed).
    fn kerr(op: &str, e: &KError) -> Error {
        let kind = match e {
            KError::PlatformFailure(_) => "platform failure",
            KError::NoStorageAccess(_) => "no storage access",
            KError::NoEntry => "no entry",
            KError::BadEncoding(_) => "bad encoding",
            KError::BadDataFormat(..) => "bad data format",
            KError::BadStoreFormat(_) => "bad store format",
            KError::TooLong(..) => "too long",
            KError::Invalid(..) => "invalid",
            KError::Ambiguous(_) => "ambiguous",
            KError::NoDefaultStore => "no default store",
            KError::NotSupportedByStore(_) => "not supported",
            _ => "other",
        };
        Error::new(ErrorCode::Internal).with_detail(format!("credential manager {op}: {kind}"))
    }

    impl KeyringStore {
        pub fn new() -> Result<Self> {
            let store = windows_native_keyring_store::Store::new().map_err(|e| kerr("open", &e))?;
            Ok(Self { store })
        }

        fn entry(&self, account: &str) -> Result<Entry> {
            let target = target_name(account);
            let mods: HashMap<&str, &str> =
                HashMap::from([("target", target.as_str()), ("persistence", "Local")]);
            self.store
                .build("SeleneRetrieval", account, Some(&mods))
                .map_err(|e| kerr("build", &e))
        }
    }

    impl SecretStore for KeyringStore {
        fn get(&self, account: &str) -> Result<Option<String>> {
            match self.entry(account)?.get_password() {
                Ok(s) => Ok(Some(s)),
                Err(KError::NoEntry) => Ok(None),
                Err(e) => Err(kerr("read", &e)),
            }
        }
        fn set(&self, account: &str, secret: &str) -> Result<()> {
            self.entry(account)?
                .set_password(secret)
                .map_err(|e| kerr("write", &e))
        }
        fn delete(&self, account: &str) -> Result<()> {
            match self.entry(account)?.delete_credential() {
                Ok(()) | Err(KError::NoEntry) => Ok(()),
                Err(e) => Err(kerr("delete", &e)),
            }
        }
    }

    /// Deletes every `SeleneRetrieval/*` generic credential of the current user. Returns how
    /// many were deleted. Idempotent. Used by "Delete all my data" and by the uninstaller.
    pub fn purge_all() -> Result<usize> {
        let store = windows_native_keyring_store::Store::new().map_err(|e| kerr("open", &e))?;
        // The prefix has no regex metacharacters.
        let pattern = format!("^{TARGET_PREFIX}");
        let spec: HashMap<&str, &str> = HashMap::from([("pattern", pattern.as_str())]);
        let entries = store.search(&spec).map_err(|e| kerr("search", &e))?;
        let mut n = 0;
        for e in entries {
            match e.delete_credential() {
                Ok(()) => n += 1,
                Err(KError::NoEntry) => {}
                Err(err) => return Err(kerr("delete", &err)),
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_never_prints() {
        let k = ApiKey::new("  sk-or-v1-0123456789abcdef\n");
        assert_eq!(k.expose(), "sk-or-v1-0123456789abcdef");
        assert!(!format!("{k:?}{k}").contains("0123"));
        assert_eq!(k.hint().as_deref(), Some("cdef"));
        assert_eq!(key_hint("short-key"), None);
    }

    #[test]
    fn scrub_removes_key_and_tokens() {
        let body = r#"{"error":"Incorrect API key provided: sk-abc123SECRETxyz. token eyJhbGciOiJIUzI1NiJ9abc123def456"}"#;
        let s = scrub(body, Some("sk-abc123SECRETxyz"));
        assert!(!s.contains("SECRET"));
        assert!(!s.contains("eyJhbGciOiJIUzI1NiJ9abc123def456"));
        assert!(s.contains("Incorrect API key provided"));
    }

    #[test]
    fn memory_store_roundtrip() {
        let s = MemorySecretStore::new();
        let a = provider_account("openrouter");
        assert_eq!(s.get(&a).ok().flatten(), None);
        assert!(s.set(&a, "k").is_ok());
        assert_eq!(s.get(&a).ok().flatten().as_deref(), Some("k"));
        assert!(s.delete(&a).is_ok());
        assert_eq!(s.get(&a).ok().flatten(), None);
        assert_eq!(target_name(&a), "SeleneRetrieval/provider:openrouter");
    }
}
