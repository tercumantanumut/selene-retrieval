//! Store-root rule (Amendment 3.12, M0-B test 8).
//!
//! LanceDB on Windows fails on `\\?\` verbatim paths, `file:///` URIs, UNC shares and paths over
//! 260 characters, and **panics** on `//?/C:/…`. The data directory is installer-controlled
//! (`%LOCALAPPDATA%\SeleneRetrieval`), so every store root must be a short, plain, local
//! drive path; anything else is refused with a plain error before any store call.

use std::path::{Component, Path, PathBuf};

use sr_common::Result;
use sr_common::error::{Error, ErrorCode, Section};

/// Longest accepted store root. Lance appends `<table>.lance/_versions/<20 digits>.manifest`,
/// `data/<uuid>.lance` and index paths (≈ 100 chars) and the daemon has no `longPathAware`
/// manifest, so the root must leave room under MAX_PATH (260).
pub const MAX_STORE_ROOT_CHARS: usize = 150;

fn refused(why: &str) -> Error {
    Error::new(ErrorCode::SettingsInvalid)
        .with_section(Section::System)
        .with_detail(format!(
            "The data folder must be a short folder on this computer (not a network or \
             special path, under {MAX_STORE_ROOT_CHARS} characters): {why}"
        ))
}

/// Validate a store root and return it unchanged. Never canonicalises (that would produce a
/// `\\?\` path on Windows).
pub fn validate_store_root(path: &Path) -> Result<PathBuf> {
    let s = path.to_str().ok_or_else(|| refused("not valid Unicode"))?;
    if s.is_empty() {
        return Err(refused("empty"));
    }
    if s.chars().count() > MAX_STORE_ROOT_CHARS {
        return Err(refused("too long"));
    }
    if s.chars().any(|c| c.is_control()) {
        return Err(refused("contains control characters"));
    }
    if s.contains("://") || s.to_ascii_lowercase().starts_with("file:") {
        return Err(refused("a URI, not a folder"));
    }
    let norm = s.replace('/', "\\");
    if norm.starts_with("\\\\") {
        return Err(refused("network or verbatim (\\\\?\\) path"));
    }
    if !path.is_absolute() {
        return Err(refused("not an absolute path"));
    }
    if cfg!(windows) {
        let b = norm.as_bytes();
        let drive_ok = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
        if !drive_ok {
            return Err(refused("not a drive-letter path"));
        }
        // An NTFS alternate data stream (`C:\dir:stream`) or device names never belong here.
        if norm[2..].contains(':') {
            return Err(refused("contains ':'"));
        }
    }
    for c in path.components() {
        match c {
            Component::ParentDir => return Err(refused("contains '..'")),
            Component::Normal(seg) => {
                let seg = seg.to_string_lossy();
                if seg.ends_with('.') || seg.ends_with(' ') {
                    return Err(refused("a folder name ends with a dot or space"));
                }
            }
            _ => {}
        }
    }
    Ok(path.to_path_buf())
}

// Windows path forms only (the only test here).
#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn windows_forms_from_m0_test_8() {
        let ok = [
            r"C:\Users\me\AppData\Local\SeleneRetrieval\lance",
            "C:/Users/me/AppData/Local/SR/lance",
            r"D:\ünïcødé 数据\lance",
        ];
        for p in ok {
            assert!(validate_store_root(Path::new(p)).is_ok(), "{p}");
        }
        let long = format!(r"C:\{}", "a".repeat(200));
        let bad = [
            r"\\?\C:\Users\me\lance",
            "//?/C:/Users/me/lance",
            r"\\localhost\C$\Users\me\lance",
            "file:///C:/Users/me/lance",
            "memory://x",
            r"relative\lance",
            r"C:\Users\..\lance",
            r"C:\Users\me\lance.",
            r"C:\Users\me\lance:ads",
            long.as_str(),
        ];
        for p in bad {
            let e = validate_store_root(Path::new(p)).unwrap_err();
            assert_eq!(e.code, ErrorCode::SettingsInvalid, "{p}");
            let m = e.user_message();
            assert!(!m.title.is_empty());
        }
    }
}
