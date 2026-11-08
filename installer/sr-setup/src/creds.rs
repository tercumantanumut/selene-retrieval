//! AI provider keys in Windows Credential Manager (M3): uninstall removes every one of ours, so no
//! key outlives the app (00 U22, m2-i I5). Only the current user's **generic** credentials whose
//! target name starts with [`CRED_PREFIX`] are touched; nothing else is enumerated or deleted, and
//! no secret is ever read (only target names) or printed (only a count).
//!
//! The prefix is the contract with sr-providers' keyring store (request R-P2 in
//! `docs/research/m3-u-progress.md`). If sr-providers names its credentials differently, change
//! [`CRED_PREFIX`] here, in one place.
#![allow(unsafe_code)]

use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
use windows_sys::Win32::Security::Credentials::{
    CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredEnumerateW, CredFree,
};

/// Target-name prefix of every credential Selene Retrieval creates.
pub const CRED_PREFIX: &str = "SeleneRetrieval/";

/// Whether a credential target name is one of ours. Covers every account sr-providers stores
/// (`secrets::target_name` always prepends the same prefix): provider API keys
/// (`provider:<id>`), the proxy password, HTTP tool tokens (`http-token:<client>`) and the
/// "Claude (sign in)" OAuth tokens (owner decision #2: kept under the same prefix).
pub fn is_ours(target: &str) -> bool {
    target.starts_with(CRED_PREFIX) && target.len() > CRED_PREFIX.len()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Target names (null-terminated UTF-16) of our generic credentials.
fn our_targets() -> Result<Vec<Vec<u16>>, String> {
    let filter = wide(&format!("{CRED_PREFIX}*"));
    let mut count: u32 = 0;
    let mut list: *mut *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `filter` is a valid null-terminated wide string that outlives the call; on success
    // the API returns `count` valid credential pointers in one block, freed below with CredFree.
    let ok = unsafe { CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut list) };
    if ok == 0 {
        // SAFETY: plain thread-local error read right after the failing call.
        let err = unsafe { GetLastError() };
        return if err == ERROR_NOT_FOUND {
            Ok(Vec::new())
        } else {
            Err(format!(
                "Credential Manager could not be read (error {err})"
            ))
        };
    }
    let mut out = Vec::new();
    for i in 0..count as usize {
        // SAFETY: `i < count`, so the pointer array entry is valid; each entry points to a valid
        // CREDENTIALW whose TargetName is a null-terminated wide string (or null).
        let (kind, target) = unsafe {
            let c = &**list.add(i);
            (c.Type, c.TargetName)
        };
        if kind != CRED_TYPE_GENERIC || target.is_null() {
            continue;
        }
        let mut name = Vec::new();
        let mut p = target;
        // SAFETY: walks the null-terminated string the API returned, stopping at the terminator.
        unsafe {
            while *p != 0 {
                name.push(*p);
                p = p.add(1);
            }
        }
        // Belt and braces: the filter already matched, but re-check the prefix ourselves.
        if is_ours(&String::from_utf16_lossy(&name)) {
            name.push(0);
            out.push(name);
        }
    }
    // SAFETY: frees the single block returned by CredEnumerateW exactly once.
    unsafe { CredFree(list as *const core::ffi::c_void) };
    Ok(out)
}

/// How many of our provider keys exist (for `sr-setup status`).
pub fn count() -> Result<usize, String> {
    our_targets().map(|t| t.len())
}

/// Deletes every provider key of ours. Returns how many were deleted.
pub fn delete_all() -> Result<usize, String> {
    let targets = our_targets()?;
    let mut deleted = 0;
    let mut failed = 0;
    for t in &targets {
        // SAFETY: `t` is a null-terminated wide string; deleting a generic credential by name.
        if unsafe { CredDeleteW(t.as_ptr(), CRED_TYPE_GENERIC, 0) } != 0 {
            deleted += 1;
        } else {
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(format!(
            "{failed} saved AI provider key(s) could not be removed"
        ));
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_specific() {
        // A broad prefix could delete other apps' credentials.
        assert!(CRED_PREFIX.len() >= 10 && CRED_PREFIX.ends_with('/'));
        assert_eq!(wide("ab").last(), Some(&0));
    }

    /// The prefix is a contract with sr-providers' keyring store: the uninstaller must match every
    /// target it can create, including the Claude (sign in) tokens (M5-I).
    #[test]
    fn prefix_matches_the_service_store() {
        let secrets = include_str!("../../../crates/providers/src/secrets.rs");
        assert!(
            secrets.contains(&format!(
                "pub const TARGET_PREFIX: &str = \"{CRED_PREFIX}\";"
            )),
            "sr-providers TARGET_PREFIX changed: update CRED_PREFIX"
        );
        assert!(secrets.contains("format!(\"{TARGET_PREFIX}{account}\")"));
        for t in [
            "SeleneRetrieval/provider:openrouter",
            "SeleneRetrieval/proxy",
            "SeleneRetrieval/http-token:cursor-1",
            "SeleneRetrieval/oauth:claude",
            "SeleneRetrieval/claude_signin/refresh",
        ] {
            assert!(is_ours(t), "{t}");
        }
        for t in [
            "SeleneRetrieval/",
            "SeleneRetrievalX/provider:openai",
            "Selene/SeleneRetrieval/x",
            "Claude Code-credentials",
            "",
        ] {
            assert!(!is_ours(t), "{t}");
        }
    }
}
