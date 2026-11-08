//! Machine policy (M5-E): loading `%ProgramData%\SeleneRetrieval\policy.toml` and the registry
//! policy keys, and the engine's checks. The rules (restrict-only, most restrictive wins, fail
//! closed) live in [`sr_common::config::policy`].
//!
//! Where the policy is enforced:
//! * settings: [`Policy::apply`] after the load and after every settings write (policy wins);
//! * AI providers: `providers.save` / `providers.check` and the provider connect at start;
//! * HTTP listener and MCP `add_folder` / `sync_folder`: the settings lock plus an explicit
//!   refusal on the switches;
//! * folders: the process-wide policy feeds `DangerousRoots::from_env`, so adding, scanning and
//!   reading a folder outside the allowed roots is refused, and an existing one turns
//!   unavailable (nothing is deleted);
//! * model downloads: refused in `sr_providers::download` before any network request, plus an
//!   early refusal on the app's switches.

use std::path::{Path, PathBuf};

use sr_common::config::policy::{MANAGED_TEXT, POLICY_DIR, POLICY_FILE, POLICY_REG_KEY, Policy};
use sr_common::error::{Error, ErrorCode};

use crate::EngineHandle;

/// `%ProgramData%\SeleneRetrieval\policy.toml` (debug builds: `SR_POLICY_FILE` overrides it for
/// tests; ignored in release like every dev hook).
pub fn policy_file() -> Option<PathBuf> {
    if let Some(p) = sr_client::naming::dev_env("SR_POLICY_FILE") {
        return Some(PathBuf::from(p));
    }
    crate::win::program_data().map(|d| d.join(POLICY_DIR).join(POLICY_FILE))
}

/// Read one policy file. Missing = unmanaged; present but unreadable = fail closed.
pub fn load_file(path: &Path) -> Option<Policy> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(Policy::from_toml_str(&text, POLICY_FILE)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(Policy::locked_down(
            POLICY_FILE,
            format!(
                "{POLICY_FILE} couldn't be opened ({}); every managed feature is turned off until it can be read",
                e.kind()
            ),
        )),
    }
}

/// The registry policy of one hive (`HKLM` when `machine`, else `HKCU`).
pub fn load_registry(machine: bool) -> Policy {
    let name = if machine { "HKLM" } else { "HKCU" };
    Policy::from_registry(name, |v| {
        crate::win::policy_value(machine, POLICY_REG_KEY, v)
    })
}

/// The effective machine policy: the file, HKLM and HKCU merged (most restrictive wins).
/// Debug builds skip the registry with `SR_POLICY_NO_REGISTRY` (tests).
pub fn load_machine() -> Policy {
    let mut p = Policy::unmanaged();
    if let Some(f) = policy_file()
        && let Some(file) = load_file(&f)
    {
        p = p.merge(file);
    }
    if sr_client::naming::dev_env("SR_POLICY_NO_REGISTRY").is_none() {
        for machine in [true, false] {
            let r = load_registry(machine);
            if r.is_managed() || !r.issues.is_empty() {
                p = p.merge(r);
            }
        }
    }
    p
}

/// Log the policy once at start (plain notes; never secrets: the policy holds none).
pub fn log(p: &Policy) {
    if p.is_managed() {
        tracing::info!(target: "sr_engine::policy", sources = ?p.sources, locked = p.managed_fields().len(), "settings are managed by your organisation");
    }
    for i in &p.issues {
        tracing::warn!(target: "sr_engine::policy", "{i}");
    }
}

pub use sr_common::api::PolicyStatus;

/// The `ManagedByPolicy` error for one refused thing.
pub fn refused(detail: &str) -> Error {
    Error::new(ErrorCode::ManagedByPolicy).with_detail(format!("{MANAGED_TEXT}: {detail}"))
}

impl EngineHandle {
    /// The machine policy this engine runs under.
    pub fn policy(&self) -> &Policy {
        &self.inner().policy
    }

    /// `policy.status`.
    pub fn policy_status(&self) -> PolicyStatus {
        let p = self.policy();
        PolicyStatus {
            managed: p.is_managed(),
            fields: p.managed_fields(),
            issues: p.issues.clone(),
            sources: p.sources.clone(),
        }
    }

    /// Whether provider `id` (reached at `address`, else its built-in address) may be used.
    pub(crate) fn check_provider_policy(
        &self,
        id: &str,
        address: Option<&str>,
    ) -> Result<(), Error> {
        let p = self.policy();
        if !p.is_managed() {
            return Ok(());
        }
        let builtin = sr_providers::manifest()
            .get(id)
            .and_then(|s| s.base_url.clone());
        let url = address.map(str::to_string).or(builtin);
        if p.provider_allowed(id, url.as_deref()) {
            Ok(())
        } else {
            let name = sr_providers::manifest()
                .get(id)
                .map_or_else(|| id.to_string(), |s| s.display_name.clone());
            Err(Error::new(ErrorCode::ManagedByPolicy)
                .with_provider(name)
                .with_detail(format!("{MANAGED_TEXT}: AI provider {id}")))
        }
    }

    /// Refuse an optional model download (search model, multilingual, reranker).
    pub(crate) fn check_download_policy(&self, what: &str) -> Result<(), Error> {
        if self.policy().model_downloads_allowed() {
            Ok(())
        } else {
            Err(refused(&format!("model download ({what})")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_missing_unreadable_and_broken() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(load_file(&dir.path().join("none.toml")), None);
        // A directory where the file should be: present but unreadable → fail closed.
        let as_dir = dir.path().join("policy.toml");
        std::fs::create_dir(&as_dir).unwrap_or_else(|e| panic!("{e}"));
        let p = load_file(&as_dir).unwrap_or_default();
        assert!(!p.http_server_allowed() && !p.model_downloads_allowed());
        // Not UTF-8 → unreadable → fail closed.
        let bin = dir.path().join("bin.toml");
        std::fs::write(&bin, [0xff, 0xfe, 0x00, 0x41]).unwrap_or_else(|e| panic!("{e}"));
        let p = load_file(&bin).unwrap_or_default();
        assert!(!p.remote_providers_allowed());
        let ok = dir.path().join("ok.toml");
        std::fs::write(&ok, "[integrations]\nallow_http_server = false\n")
            .unwrap_or_else(|e| panic!("{e}"));
        let p = load_file(&ok).unwrap_or_default();
        assert!(!p.http_server_allowed() && p.mcp_add_folder_allowed());
    }
}
