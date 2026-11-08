//! Win32 calls of the worker process. The only module of `sr-extract` allowed to use `unsafe`
//! (reviewed FFI; each call documents its safety argument).
#![allow(unsafe_code)]

/// DLL search: System32 only (pdfium is loaded by absolute path), and no error dialogs.
pub fn harden_process() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX, SetErrorMode,
        };
        use windows_sys::Win32::System::LibraryLoader::{
            LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
        };
        // SAFETY: plain flag-setting calls without pointers.
        unsafe {
            SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
            SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
        }
    }
}

/// Whether stdin is an interactive console (someone double-clicked the exe).
pub fn stdin_is_console() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, GetFileType};
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
        // SAFETY: GetStdHandle has no preconditions; GetFileType accepts any handle value
        // (invalid ones return FILE_TYPE_UNKNOWN).
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            !h.is_null() && GetFileType(h) == FILE_TYPE_CHAR
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// `(integrity level, is AppContainer)` of this process's token.
pub fn token_info() -> (String, bool) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
            TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel, TokenIsAppContainer,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: the token handle is checked and closed; the label buffer is u64-aligned, large
        // enough for TOKEN_MANDATORY_LABEL + SID, and only read after the call succeeds; the SID
        // pointer stays inside that buffer.
        unsafe {
            let mut tok = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
                return ("unknown".into(), false);
            }
            let mut buf = [0u64; 32];
            let mut n = 0u32;
            let mut il = "unknown".to_string();
            if GetTokenInformation(
                tok,
                TokenIntegrityLevel,
                buf.as_mut_ptr().cast(),
                std::mem::size_of_val(&buf) as u32,
                &mut n,
            ) != 0
            {
                let lab = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
                let cnt = *GetSidSubAuthorityCount(lab.Label.Sid);
                if cnt > 0 {
                    let rid = *GetSidSubAuthority(lab.Label.Sid, u32::from(cnt) - 1);
                    il = match rid {
                        0x0000 => "untrusted".into(),
                        0x1000 => "low".into(),
                        0x2000 => "medium".into(),
                        0x2100 => "medium+".into(),
                        0x3000 => "high".into(),
                        0x4000 => "system".into(),
                        r => format!("rid 0x{r:x}"),
                    };
                }
            }
            let mut ac = 0u32;
            GetTokenInformation(
                tok,
                TokenIsAppContainer,
                (&mut ac as *mut u32).cast(),
                4,
                &mut n,
            );
            CloseHandle(tok);
            (il, ac != 0)
        }
    }
    #[cfg(not(windows))]
    {
        ("unknown".into(), false)
    }
}
