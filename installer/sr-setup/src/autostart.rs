//! Autostart values (M5-I): tauri-plugin-autostart writes `HKCU\…\Run` "Selene Retrieval", and
//! Explorer's Startup apps page adds `…\Explorer\StartupApproved\Run` with the same name (R-UX25).
//! The NSIS uninstaller also deletes both itself; the MSI has no "delete a value on uninstall"
//! table, so `sr-setup uninstall` does it for both installers (idempotent).
#![allow(unsafe_code)]

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteKeyValueW};

/// Value name used by tauri-plugin-autostart (the product name) and by NSIS `RUNKEY`.
pub const VALUE: &str = "Selene Retrieval";
pub const KEYS: &[&str] = &[
    r"Software\Microsoft\Windows\CurrentVersion\Run",
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run",
];

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Deletes `HKCU\<key>\<VALUE>`. `Ok(true)` = deleted, `Ok(false)` = wasn't there.
pub fn delete(key: &str) -> Result<bool, String> {
    let k = wide(key);
    let v = wide(VALUE);
    // SAFETY: both arguments are valid null-terminated wide strings that outlive the call;
    // HKEY_CURRENT_USER is a predefined handle.
    let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr()) };
    match rc {
        ERROR_SUCCESS => Ok(true),
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(false),
        e => Err(format!("registry error {e}")),
    }
}
