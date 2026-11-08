//! sr-msi-ca: custom actions of the Selene Retrieval MSI (M5-I, `installer/wix/`).
//!
//! Windows Installer can't re-run a registry search after files are installed, and an EXE custom
//! action can't set a property, so the two "after the fact" decisions the NSIS installer makes in
//! script need a tiny DLL here. All other work stays in `sr-setup.exe` (same commands as NSIS).
//!
//! | Export | Where it runs | What it does |
//! |---|---|---|
//! | `SrWebView2Detect` | execute sequence, immediate, after CostFinalize | sets `SR_WV2_VERSION` (empty when WebView2 is missing; same three registry values as the NSIS script) |
//! | `SrWebView2Result` | UI sequence, after ExecuteAction | re-checks; when WebView2 is still missing, sets the Finish page's `WIXUI_EXITDIALOGOPTIONALTEXT` to the OQ-14 copy and removes the "Open" checkbox |
//! | `SrShowNotice` | execute sequence, immediate, after InstallFinalize (uninstall) | shows sr-setup's uninstall summary (R-UX43) as a user message (a box in full and basic UI, log only with `/qn`), then deletes the file |
//!
//! Every action returns success: none of this may fail an install (search from Claude works
//! without WebView2, and the summary is advice).

/// The Finish-page copy when WebView2 is still missing (OQ-14, page 02; same words as NSIS).
pub const WV2_MISSING_TEXT: &str = "Selene Retrieval needs Microsoft Edge WebView2 to show its \
     window, and it couldn't be installed without an internet connection. Connect to the internet \
     and run this installer again. Search from Claude and other AI apps already works.";

/// The same copy for the offline package (`SR_WV2_OFFLINE=1`): the runtime is bundled, so the
/// internet isn't the cause and isn't the next action.
pub const WV2_MISSING_TEXT_OFFLINE: &str = "Selene Retrieval needs Microsoft Edge WebView2 to \
     show its window, and it couldn't be installed. Restart your computer and run this installer \
     again. Search from Claude and other AI apps already works.";

/// Finish-page copy for the package variant (`SR_WV2_OFFLINE` property, set by the offline build).
pub fn wv2_missing_text(offline_property: &str) -> &'static str {
    if offline_property.trim() == "1" {
        WV2_MISSING_TEXT_OFFLINE
    } else {
        WV2_MISSING_TEXT
    }
}

/// Registry values that hold the installed WebView2 Evergreen version (`pv`), in the order the
/// NSIS script checks them: machine-wide (32-bit view), machine-wide (64-bit view), per user.
pub const WV2_CLIENT: &str = r"Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

/// A `pv` value that means "installed". Microsoft writes `0.0.0.0` (or leaves it empty) after an
/// uninstall.
pub fn is_installed_version(pv: &str) -> bool {
    let pv = pv.trim();
    !pv.is_empty() && pv != "0.0.0.0"
}

/// The first installed version among the candidates.
pub fn first_installed<I: IntoIterator<Item = Option<String>>>(values: I) -> Option<String> {
    values
        .into_iter()
        .flatten()
        .find(|v| is_installed_version(v))
        .map(|v| v.trim().to_string())
}

/// sr-setup writes the summary as UTF-16LE without BOM (a BOM is tolerated).
pub fn decode_notice(bytes: &[u8]) -> Option<String> {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let units = units.strip_prefix(&[0xfeff]).unwrap_or(&units);
    let s = String::from_utf16_lossy(units);
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod ffi {
    use super::*;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::ApplicationInstallationAndServicing::{
        INSTALLMESSAGE_USER, MSIHANDLE, MsiCloseHandle, MsiCreateRecord, MsiGetPropertyW,
        MsiProcessMessage, MsiRecordSetStringW, MsiSetPropertyW,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY,
        RRF_SUBKEY_WOW6464KEY, RegGetValueW,
    };

    /// `MB_ICONINFORMATION` (winuser.h); `MB_OK` is 0.
    const MB_ICONINFORMATION: i32 = 0x40;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `hkey\subkey\value` as a string (REG_SZ), in the given registry view.
    fn reg_sz(hkey: HKEY, subkey: &str, value: &str, view: u32) -> Option<String> {
        let k = wide(subkey);
        let v = wide(value);
        let mut buf = vec![0u16; 128];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: valid null-terminated inputs; `buf` has `bytes` writable bytes; RegGetValueW
        // writes at most that many and null-terminates REG_SZ data (RRF_RT_REG_SZ).
        let rc = unsafe {
            RegGetValueW(
                hkey,
                k.as_ptr(),
                v.as_ptr(),
                RRF_RT_REG_SZ | view,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let n = (bytes as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..n]);
        Some(s.trim_end_matches('\0').to_string())
    }

    pub fn webview2_version() -> Option<String> {
        let machine = format!(r"SOFTWARE\{WV2_CLIENT}");
        let user = format!(r"Software\{WV2_CLIENT}");
        first_installed([
            reg_sz(HKEY_LOCAL_MACHINE, &machine, "pv", RRF_SUBKEY_WOW6432KEY),
            reg_sz(HKEY_LOCAL_MACHINE, &machine, "pv", RRF_SUBKEY_WOW6464KEY),
            reg_sz(HKEY_CURRENT_USER, &user, "pv", 0),
        ])
    }

    pub fn set(h: MSIHANDLE, name: &str, value: &str) {
        let n = wide(name);
        let v = wide(value);
        // SAFETY: valid handle from msiexec and null-terminated strings.
        unsafe { MsiSetPropertyW(h, n.as_ptr(), v.as_ptr()) };
    }

    pub fn get(h: MSIHANDLE, name: &str) -> String {
        let n = wide(name);
        let mut len: u32 = 0;
        let mut empty = [0u16; 1];
        // SAFETY: size query: a 0-length buffer makes the call return the needed length.
        unsafe { MsiGetPropertyW(h, n.as_ptr(), empty.as_mut_ptr(), &mut len) };
        let mut buf = vec![0u16; len as usize + 1];
        let mut cap = buf.len() as u32;
        // SAFETY: `buf` holds `cap` u16s including the terminator.
        let rc = unsafe { MsiGetPropertyW(h, n.as_ptr(), buf.as_mut_ptr(), &mut cap) };
        if rc != ERROR_SUCCESS {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..(cap as usize).min(buf.len())])
    }

    /// Shows `text` as a user message (OK button, information icon); log only in silent mode.
    pub fn message(h: MSIHANDLE, text: &str) {
        // SAFETY: plain record API; the record handle is closed below.
        unsafe {
            let rec = MsiCreateRecord(1);
            if rec == 0 {
                return;
            }
            // Template "[1]" so brackets in a path are never treated as properties.
            let t = wide("[1]");
            let s = wide(text);
            MsiRecordSetStringW(rec, 0, t.as_ptr());
            MsiRecordSetStringW(rec, 1, s.as_ptr());
            MsiProcessMessage(h, INSTALLMESSAGE_USER | MB_ICONINFORMATION, rec);
            MsiCloseHandle(rec);
        }
    }
}

/// Custom action: `SR_WV2_VERSION` = installed WebView2 version, or empty.
#[cfg(windows)]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "system" fn SrWebView2Detect(h: u32) -> u32 {
    let v = ffi::webview2_version().unwrap_or_default();
    ffi::set(h, "SR_WV2_VERSION", &v);
    0
}

/// Custom action (UI sequence, after ExecuteAction): when WebView2 is still missing, the Finish
/// page says what to do and doesn't offer to open a window that can't show (OQ-14).
#[cfg(windows)]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "system" fn SrWebView2Result(h: u32) -> u32 {
    match ffi::webview2_version() {
        Some(v) => ffi::set(h, "SR_WV2_VERSION", &v),
        None => {
            ffi::set(h, "SR_WV2_VERSION", "");
            let text = wv2_missing_text(&ffi::get(h, "SR_WV2_OFFLINE"));
            ffi::set(h, "WIXUI_EXITDIALOGOPTIONALTEXT", text);
            ffi::set(h, "WIXUI_EXITDIALOGOPTIONALCHECKBOXTEXT", "");
            ffi::set(h, "WIXUI_EXITDIALOGOPTIONALCHECKBOX", "");
        }
    }
    0
}

/// Custom action (uninstall, after InstallFinalize): shows the summary sr-setup wrote to
/// `[SR_NOTICE_FILE]` (R-UX43), then deletes it.
#[cfg(windows)]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "system" fn SrShowNotice(h: u32) -> u32 {
    let path = ffi::get(h, "SR_NOTICE_FILE");
    if path.is_empty() {
        return 0;
    }
    if let Ok(bytes) = std::fs::read(&path) {
        let _ = std::fs::remove_file(&path);
        if let Some(text) = decode_notice(&bytes) {
            ffi::message(h, &text);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_rules_match_the_nsis_script() {
        assert!(!is_installed_version(""));
        assert!(!is_installed_version("0.0.0.0"));
        assert!(is_installed_version("141.0.3537.71"));
        assert_eq!(
            first_installed([None, Some("0.0.0.0".into()), Some(" 141.0.1.2 ".into())]),
            Some("141.0.1.2".into())
        );
        assert_eq!(first_installed([None, Some(String::new()), None]), None);
    }

    #[test]
    fn notice_is_utf16le_with_or_without_bom() {
        let text = "Selene Retrieval was removed.\r\n\r\nVS Code: C:\\Users\\Zoë\\mcp.json";
        let le: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_notice(&le).as_deref(), Some(text));
        let mut bom = vec![0xff, 0xfe];
        bom.extend(&le);
        assert_eq!(decode_notice(&bom).as_deref(), Some(text));
        assert_eq!(decode_notice(b""), None);
    }

    #[test]
    fn finish_page_copy_has_one_action() {
        assert!(WV2_MISSING_TEXT.contains("run this installer again"));
        assert!(!WV2_MISSING_TEXT.contains("  "));
        assert!(WV2_MISSING_TEXT_OFFLINE.contains("run this installer again"));
        assert!(!WV2_MISSING_TEXT_OFFLINE.contains("  "));
        // The offline package bundles the runtime: never blame the connection there.
        assert!(!WV2_MISSING_TEXT_OFFLINE.contains("internet"));
        assert_eq!(wv2_missing_text("1"), WV2_MISSING_TEXT_OFFLINE);
        assert_eq!(wv2_missing_text(""), WV2_MISSING_TEXT);
    }
}
