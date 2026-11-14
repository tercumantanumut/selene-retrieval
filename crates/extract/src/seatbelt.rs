//! macOS sandbox of the worker (Amendment 18.2): a Seatbelt profile that denies everything by
//! default. The only module of `sr-extract` on macOS allowed to use `unsafe` (three libSystem
//! calls, each with its safety argument).
//!
//! `main` applies it after pdfium is loaded and before the ready frame, so no byte of any document
//! is read outside the sandbox. Loading pdfium first keeps the profile small: dyld, AppKit and
//! CoreGraphics start-up (trusted code, fixed absolute path, no input) need file and Mach access
//! the parser never gets. From then on the worker can:
//! * read and write the three pipes it inherited (already open; nothing new can be opened),
//! * read the system font folders (pdfium maps fonts a PDF does not embed from them, as the
//!   AppContainer reads `C:\Windows\Fonts`),
//! * signal itself (`abort`).
//!
//! Everything else is denied: every other file read or write, network (IP and Unix sockets),
//! fork and exec, Mach service lookups (pasteboard, Launch Services, ...), sysctl, IOKit.
//! The supervisor does not trust this report: it checks the denials itself with `sandbox_check`
//! on the worker's pid before the first request (`sr-extract-client` sandbox/macos.rs).
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int};

/// The profile (SBPL). The `deny` lines repeat what `(deny default)` already does, so a reader
/// sees what is shut and no later `allow` can quietly reopen it.
pub const PROFILE: &str = r#"(version 1)
(deny default)
(allow signal (target self))
(allow file-read* (subpath "/System/Library/Fonts") (subpath "/Library/Fonts"))
(deny file-write*)
(deny network*)
(deny process-fork)
(deny process-exec*)
(deny mach-lookup)
"#;

/// Test-only weaker profile (`--features test-probes`): network allowed. The supervisor must
/// refuse a worker running under it (supervisor test `weak_or_missing_sandbox_is_refused`).
#[cfg(feature = "test-probes")]
const WEAK_PROFILE: &str = r#"(version 1)
(deny default)
(allow signal (target self))
(allow network*)
"#;

/// What `token_info` reports once the profile is in force.
pub const INTEGRITY: &str = "seatbelt";

unsafe extern "C" {
    // <sandbox.h>; deprecated in the SDK but supported (Chromium, Firefox and WebKit's helper
    // processes use the same family). `flags == 0`: `profile` is SBPL source.
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
    // libsystem_sandbox (not in the public header): with a null operation, 1 = this process is
    // sandboxed.
    fn sandbox_check(pid: libc::pid_t, operation: *const c_char, filter_type: c_int, ...) -> c_int;
}

/// Applies [`PROFILE`] to this process for good (it can't be lifted or loosened). `Err` holds
/// the reason; the caller must then exit without reading a request.
pub fn apply() -> Result<(), String> {
    apply_profile(profile_for_this_build())
}

#[cfg(not(feature = "test-probes"))]
fn profile_for_this_build() -> Option<&'static str> {
    Some(PROFILE)
}

/// Test builds only: a worker staged under a name with `-nosandbox-test` skips the sandbox and one
/// with `-weak-test` takes [`WEAK_PROFILE`]; both still claim to be sandboxed, so the tests show
/// that the supervisor checks the kernel, not the claim.
#[cfg(feature = "test-probes")]
fn profile_for_this_build() -> Option<&'static str> {
    let name = std::env::args_os()
        .next()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.contains("-nosandbox-test") {
        None
    } else if name.contains("-weak-test") {
        Some(WEAK_PROFILE)
    } else {
        Some(PROFILE)
    }
}

fn apply_profile(profile: Option<&str>) -> Result<(), String> {
    let Some(profile) = profile else {
        return Ok(());
    };
    let p = CString::new(profile).map_err(|_| "profile contains a NUL byte".to_string())?;
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: `p` is a NUL-terminated string that outlives the call; `err` is a valid out-pointer.
    // On failure `err` is null or a C string we free once with sandbox_free_error.
    let rc = unsafe { sandbox_init(p.as_ptr(), 0, &mut err) };
    if rc != 0 {
        let why = if err.is_null() {
            format!("sandbox_init returned {rc}")
        } else {
            // SAFETY: non-null `err` is a NUL-terminated string owned by libsandbox.
            let s = unsafe { CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: freed exactly once, as the header requires.
            unsafe { sandbox_free_error(err) };
            s
        };
        return Err(format!("the Seatbelt profile was refused: {why}"));
    }
    if !is_sandboxed() {
        return Err("sandbox_init succeeded but the process is not sandboxed".into());
    }
    Ok(())
}

/// Whether the kernel has this process under a sandbox.
pub fn is_sandboxed() -> bool {
    // SAFETY: getpid has no preconditions; a null operation with filter type 0 (none) takes no
    // variadic arguments.
    unsafe { sandbox_check(libc::getpid(), std::ptr::null(), 0) == 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_denies_by_default_and_reopens_nothing_risky() {
        assert!(PROFILE.contains("(deny default)"));
        for line in PROFILE.lines().filter(|l| l.starts_with("(allow")) {
            assert!(
                line.starts_with("(allow signal (target self))")
                    || line.starts_with("(allow file-read* (subpath \"/System/Library/Fonts\")"),
                "unexpected allow rule: {line}"
            );
        }
    }

    #[test]
    fn the_test_process_is_not_sandboxed() {
        // The check itself works (and is false here, so `apply` is what changes it).
        assert!(!is_sandboxed());
    }
}
