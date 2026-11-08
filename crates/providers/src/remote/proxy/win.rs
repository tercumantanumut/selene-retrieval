//! WinHTTP proxy queries (M3-P). **Reviewed `unsafe`** (to be added to docs/02 §2.6 "Reviewed
//! unsafe": `sr_providers::remote::proxy::win`).
//!
//! `winhttp.dll` is not a KnownDLL, so (like `version.dll` in M2-G) it is never imported by name:
//! its four exports are resolved from **System32** at run time ([`crate::sys::system32_proc`]).
//! Strings returned by WinHTTP are freed with `GlobalFree` (kernel32, a KnownDLL).

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE, WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_AUTO_DETECT_TYPE_DHCP,
    WINHTTP_AUTO_DETECT_TYPE_DNS_A, WINHTTP_AUTOPROXY_AUTO_DETECT, WINHTTP_AUTOPROXY_CONFIG_URL,
    WINHTTP_AUTOPROXY_OPTIONS, WINHTTP_CURRENT_USER_IE_PROXY_CONFIG, WINHTTP_PROXY_INFO,
};
use windows_sys::core::{BOOL, PCWSTR, PWSTR};

use super::WindowsProxyConfig;
use crate::sys::system32_proc;

type FnIeConfig = unsafe extern "system" fn(*mut WINHTTP_CURRENT_USER_IE_PROXY_CONFIG) -> BOOL;
type FnDefault = unsafe extern "system" fn(*mut WINHTTP_PROXY_INFO) -> BOOL;
type FnOpen =
    unsafe extern "system" fn(PCWSTR, WINHTTP_ACCESS_TYPE, PCWSTR, PCWSTR, u32) -> *mut c_void;
type FnForUrl = unsafe extern "system" fn(
    *mut c_void,
    PCWSTR,
    *mut WINHTTP_AUTOPROXY_OPTIONS,
    *mut WINHTTP_PROXY_INFO,
) -> BOOL;
type FnClose = unsafe extern "system" fn(*mut c_void) -> BOOL;
type FnTimeouts = unsafe extern "system" fn(*mut c_void, i32, i32, i32, i32) -> BOOL;

struct Api {
    ie_config: FnIeConfig,
    default_config: FnDefault,
    open: FnOpen,
    for_url: FnForUrl,
    close: FnClose,
    timeouts: FnTimeouts,
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        let p = |name: &std::ffi::CStr| system32_proc("winhttp.dll", name);
        // SAFETY: each export is resolved from System32's winhttp.dll by its documented name and
        // transmuted to its documented signature (winhttp.h; the same signatures windows-sys
        // declares). The module is never freed (system32_proc), so the pointers stay valid.
        unsafe {
            Some(Api {
                ie_config: std::mem::transmute::<crate::sys::Proc, FnIeConfig>(p(
                    c"WinHttpGetIEProxyConfigForCurrentUser",
                )?),
                default_config: std::mem::transmute::<crate::sys::Proc, FnDefault>(p(
                    c"WinHttpGetDefaultProxyConfiguration",
                )?),
                open: std::mem::transmute::<crate::sys::Proc, FnOpen>(p(c"WinHttpOpen")?),
                for_url: std::mem::transmute::<crate::sys::Proc, FnForUrl>(p(
                    c"WinHttpGetProxyForUrl",
                )?),
                close: std::mem::transmute::<crate::sys::Proc, FnClose>(p(c"WinHttpCloseHandle")?),
                timeouts: std::mem::transmute::<crate::sys::Proc, FnTimeouts>(p(
                    c"WinHttpSetTimeouts",
                )?),
            })
        }
    })
    .as_ref()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Takes ownership of a WinHTTP-allocated string: copies it out and `GlobalFree`s it.
fn take(p: PWSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: WinHTTP returns NUL-terminated wide strings allocated with GlobalAlloc; we read up
    // to the NUL, then free the block exactly once (the pointer is not used afterwards).
    unsafe {
        let mut n = 0usize;
        while *p.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
        GlobalFree(p.cast());
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

/// The user's proxy settings plus the machine WinHTTP proxy. `None` if WinHTTP is unavailable.
pub(super) fn snapshot() -> Option<WindowsProxyConfig> {
    let api = api()?;
    let mut cfg = WindowsProxyConfig::default();
    let mut ie = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
    // SAFETY: `ie` is a valid, zeroed out-struct; on success its strings are owned by us and
    // released by `take`.
    if unsafe { (api.ie_config)(&mut ie) } != 0 {
        cfg.auto_detect = ie.fAutoDetect != 0;
        cfg.auto_config_url = take(ie.lpszAutoConfigUrl);
        cfg.proxy = take(ie.lpszProxy);
        cfg.bypass = take(ie.lpszProxyBypass);
    }
    let mut info = WINHTTP_PROXY_INFO::default();
    // SAFETY: as above, for the machine-wide configuration.
    if unsafe { (api.default_config)(&mut info) } != 0 {
        let p = take(info.lpszProxy);
        let b = take(info.lpszProxyBypass);
        if info.dwAccessType != WINHTTP_ACCESS_TYPE_NO_PROXY {
            cfg.machine_proxy = p;
            cfg.machine_bypass = b;
        }
    }
    Some(cfg)
}

/// Evaluates WPAD (`auto_detect`) and/or the PAC file at `pac` for `url`. Bounded: WinHTTP's
/// resolve/connect/send/receive timeouts are 5 s each. `None` when nothing applies or it fails
/// (the caller then uses the static settings).
pub(super) fn proxy_for_url(
    url: &str,
    auto_detect: bool,
    pac: Option<&str>,
) -> Option<(Option<String>, Option<String>)> {
    let api = api()?;
    let agent = wide("SeleneRetrieval");
    // SAFETY: `agent` is NUL-terminated; null proxy / bypass are allowed with NO_PROXY.
    let session = unsafe {
        (api.open)(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    if session.is_null() {
        return None;
    }
    // SAFETY: `session` is a valid WinHTTP session handle.
    unsafe { (api.timeouts)(session, 5000, 5000, 5000, 5000) };
    let url_w = wide(url);
    let pac_w = pac.map(wide);
    // S-07: no automatic Windows sign-in (NTLM / Negotiate) towards the PAC host on the first
    // try. Only a PAC URL the user or an administrator configured (never WPAD auto-detect) gets
    // a second try with sign-in, and only if the server asked for it.
    let mut out = query(api, session, &url_w, auto_detect, pac_w.as_deref(), false);
    if let Err(ERROR_WINHTTP_LOGIN_FAILURE) = out
        && pac_w.is_some()
    {
        out = query(api, session, &url_w, false, pac_w.as_deref(), true);
    }
    // SAFETY: closes the session opened above, once.
    unsafe { (api.close)(session) };
    out.ok()
}

/// `ERROR_WINHTTP_LOGIN_FAILURE`: the PAC server wanted a Windows sign-in.
const ERROR_WINHTTP_LOGIN_FAILURE: u32 = 12015;

/// One `WinHttpGetProxyForUrl` call. `Err(GetLastError())` on failure.
fn query(
    api: &Api,
    session: *mut c_void,
    url_w: &[u16],
    auto_detect: bool,
    pac_w: Option<&[u16]>,
    auto_logon: bool,
) -> Result<(Option<String>, Option<String>), u32> {
    let mut opts = WINHTTP_AUTOPROXY_OPTIONS {
        dwFlags: if auto_detect {
            WINHTTP_AUTOPROXY_AUTO_DETECT
        } else {
            0
        } | if pac_w.is_some() {
            WINHTTP_AUTOPROXY_CONFIG_URL
        } else {
            0
        },
        dwAutoDetectFlags: if auto_detect {
            WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A
        } else {
            0
        },
        lpszAutoConfigUrl: pac_w.map_or(std::ptr::null(), <[u16]>::as_ptr),
        lpvReserved: std::ptr::null_mut(),
        dwReserved: 0,
        fAutoLogonIfChallenged: i32::from(auto_logon),
    };
    let mut info = WINHTTP_PROXY_INFO::default();
    // SAFETY: valid session, NUL-terminated URL, initialised options; `info` strings are owned
    // by us on success and released by `take`. `url_w` / `pac_w` outlive the call.
    let ok = unsafe { (api.for_url)(session, url_w.as_ptr(), &mut opts, &mut info) } != 0;
    if !ok {
        // SAFETY: reads this thread's last error right after the failed call.
        return Err(unsafe { windows_sys::Win32::Foundation::GetLastError() });
    }
    let p = take(info.lpszProxy);
    let b = take(info.lpszProxyBypass);
    Ok(if info.dwAccessType == WINHTTP_ACCESS_TYPE_NO_PROXY {
        (None, None)
    } else {
        (p, b)
    })
}
