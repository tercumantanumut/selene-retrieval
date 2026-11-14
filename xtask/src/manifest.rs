//! `cargo xtask check-manifest` (M5-I): every shipped exe carries the application manifest from
//! `installer/manifests/` (embedded by `installer/manifests/embed.rs`, or by tauri-build for
//! sr-app; NSIS writes its own for the setup exe).
//!
//! Required in each embedded `RT_MANIFEST`:
//! * `requestedExecutionLevel level="asInvoker"` (never an elevation prompt, README P1.7);
//! * `supportedOS` Windows 10/11 (`{8e0f7a12-…}`);
//! * `longPathAware` = true (user folders deeper than 260 characters; the data directory stays
//!   short by rule, Amendment 3.12);
//! * `dpiAwareness` PerMonitorV2;
//! * sr-app only: the Common-Controls v6 dependency (Tauri dialogs).
//!
//! ```text
//! cargo xtask check-manifest [--profile <p>] [--no-build] [--exe <path>]...
//! ```
//! It also runs a source guard for S-08 first ([`check_dll_search`]): the `main()` of every
//! shipped exe opens with its DLL-search hardening call (`SetDefaultDllDirectories`).
//!
//! Never builds (run it after `check-crt` / a package build). Without `--exe` it checks the six
//! shipped exes in `<target>/<profile>` (default `release`). `cargo xtask package` runs it on the
//! staged files and on the finished NSIS setup exe.

use crate::pe::manifests_of_file;
use std::path::{Path, PathBuf};

/// The shipped exes: `(name, needs the Common-Controls v6 dependency)`.
pub const SHIPPED: &[(&str, bool)] = &[
    ("sr-app", true),
    ("sr-bridge", false),
    ("sr-launch", false),
    ("sr-extract", false),
    ("sr-daemon", false),
    ("sr-setup", false),
];

const WIN10_GUID: &str = "{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}";

/// Lower-case and without comments, so the checks don't depend on formatting.
fn normalise(xml: &str) -> String {
    let mut s = xml.to_ascii_lowercase();
    while let Some(a) = s.find("<!--") {
        match s[a..].find("-->") {
            Some(b) => s.replace_range(a..a + b + 3, ""),
            None => s.truncate(a),
        }
    }
    s
}

/// Text content of the first element named `name` (lower-case), with or without a namespace
/// prefix: link.exe rewrites merged elements as `<ms_compatibility:supportedOS …>`.
fn text_of<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let b = s.as_bytes();
    let mut from = 0;
    while let Some(k) = s[from..].find(name) {
        let i = from + k;
        from = i + name.len();
        let before_ok = match i.checked_sub(1).map(|j| b[j]) {
            Some(b'<') => true,
            // `<prefix:name`: the prefix runs back to a `<` with no `>` or space in between.
            Some(b':') => s[..i]
                .rfind(|c: char| c == '<' || c == '>' || c.is_whitespace())
                .is_some_and(|j| b[j] == b'<'),
            _ => false,
        };
        let after_ok = b
            .get(from)
            .is_some_and(|c| *c == b'>' || *c == b'/' || c.is_ascii_whitespace());
        if before_ok && after_ok {
            let open_end = s[from..].find('>')? + from + 1;
            let close = s[open_end..].find('<')? + open_end;
            return Some(s[open_end..close].trim());
        }
    }
    None
}

/// Problems with one manifest (empty = pass).
pub fn manifest_problems(xml: &str, needs_comctl: bool) -> Vec<String> {
    let s = normalise(xml);
    let flat: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let mut p = Vec::new();
    if !flat.contains("level=\"asinvoker\"") && !flat.contains("level='asinvoker'") {
        p.push("requestedExecutionLevel is not asInvoker".to_string());
    }
    if !flat.contains(WIN10_GUID) {
        p.push("supportedOS Windows 10/11 missing".into());
    }
    if text_of(&s, "longpathaware") != Some("true") {
        p.push("longPathAware is not true".into());
    }
    let dpi = text_of(&s, "dpiawareness")
        .and_then(|v| v.split(',').next())
        .is_some_and(|v| v.trim() == "permonitorv2");
    if !dpi {
        p.push("dpiAwareness does not start with PerMonitorV2".into());
    }
    if needs_comctl && !flat.contains("name=\"microsoft.windows.common-controls\"") {
        p.push("Common-Controls v6 dependency missing".into());
    }
    p
}

/// Checks one image: exactly one embedded manifest, and it passes [`manifest_problems`].
pub fn check_file(path: &Path, needs_comctl: bool) -> Result<(), String> {
    let found = manifests_of_file(path)?;
    let problems = match found.as_slice() {
        [] => vec!["no embedded application manifest".to_string()],
        [one] => manifest_problems(one, needs_comctl),
        many => vec![format!("{} manifests embedded (expected one)", many.len())],
    };
    if problems.is_empty() {
        println!("  OK   {}", path.display());
        Ok(())
    } else {
        Err(format!("{}: {}", path.display(), problems.join("; ")))
    }
}

/// Checks `(path, needs_comctl)` pairs; all failures are reported together.
pub fn check_all(files: &[(PathBuf, bool)]) -> Result<(), String> {
    println!("check-manifest: asInvoker, supportedOS Win10/11, longPathAware, PerMonitorV2 in:");
    let failures: Vec<String> = files
        .iter()
        .filter_map(|(p, c)| check_file(p, *c).err())
        .collect();
    if failures.is_empty() {
        println!("check-manifest: OK ({} exes)", files.len());
        Ok(())
    } else {
        Err(format!(
            "check-manifest failed:\n  {}",
            failures.join("\n  ")
        ))
    }
}

/// S-08: the `main()` source of each shipped exe and the DLL-search hardening call that must open
/// it (paths relative to the workspace root).
pub const DLL_SEARCH_MAINS: &[(&str, &str)] = &[
    ("crates/app/src/main.rs", "sr_client::harden_dll_search()"),
    (
        "crates/bridge/src/main.rs",
        "sr_client::harden_dll_search()",
    ),
    (
        "crates/bridge/src/launch.rs",
        "sr_client::harden_dll_search()",
    ),
    (
        "installer/sr-setup/src/main.rs",
        "sr_client::harden_dll_search()",
    ),
    (
        "crates/mcp-server/src/bin/sr-daemon.rs",
        "sr_providers::dml::harden_dll_search()",
    ),
    ("crates/extract/src/main.rs", "win::harden_process()"),
];

/// Statements of `fn main` (code lines, comments skipped) allowed before the hardening call:
/// sr-extract takes a start `Instant` first, which loads no DLL.
const DLL_SEARCH_MAX_POS: usize = 2;

/// Problem with one `main()` source: the hardening `marker` is not among its first code lines.
pub fn dll_search_problem(src: &str, marker: &str) -> Option<String> {
    let Some(start) = src.find("\nfn main(") else {
        return Some("no `fn main(` found".into());
    };
    let body = src[start..].split_once('{').map_or("", |(_, b)| b);
    let first: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//"))
        .take(DLL_SEARCH_MAX_POS)
        .collect();
    if first.iter().any(|l| l.contains(marker)) {
        None
    } else {
        Some(format!(
            "DLL search hardening (`{marker}`) is not the first statement of main (S-08)"
        ))
    }
}

/// Source guard for S-08 over [`DLL_SEARCH_MAINS`] (cheap: no build, no binaries).
pub fn check_dll_search(root: &Path) -> Result<(), String> {
    let failures: Vec<String> = DLL_SEARCH_MAINS
        .iter()
        .filter_map(|(rel, marker)| {
            let p = root.join(rel);
            match std::fs::read_to_string(&p) {
                Ok(src) => dll_search_problem(&src, marker).map(|e| format!("{rel}: {e}")),
                Err(e) => Some(format!("{rel}: {e}")),
            }
        })
        .collect();
    if failures.is_empty() {
        println!(
            "check-manifest: DLL search hardened first in all {} mains (S-08)",
            DLL_SEARCH_MAINS.len()
        );
        Ok(())
    } else {
        Err(format!(
            "check-manifest failed:\n  {}",
            failures.join("\n  ")
        ))
    }
}

pub fn main(args: &[String]) -> Result<(), String> {
    check_dll_search(&crate::fetch::workspace_root())?;
    let mut profile = "release".to_string();
    let mut exes: Vec<PathBuf> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--profile" => profile = it.next().ok_or("--profile needs a value")?.clone(),
            "--no-build" => {} // never builds; accepted for symmetry with check-crt
            "--exe" => exes.push(it.next().ok_or("--exe needs a path")?.into()),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    let files: Vec<(PathBuf, bool)> = if exes.is_empty() {
        let dir = crate::package::profile_dir(&profile);
        SHIPPED
            .iter()
            .map(|(n, c)| (dir.join(format!("{n}.exe")), *c))
            .collect()
    } else {
        exes.into_iter()
            .map(|p| {
                let comctl = p
                    .file_stem()
                    .is_some_and(|s| s.eq_ignore_ascii_case("sr-app"));
                (p, comctl)
            })
            .collect()
    };
    check_all(&files)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONSOLE: &str = include_str!("../../installer/manifests/console.manifest");
    const APP: &str = include_str!("../../installer/manifests/sr-app.manifest");

    #[test]
    fn every_shipped_main_hardens_dll_search_first() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap_or_else(|| panic!("xtask has a parent"));
        if let Err(e) = check_dll_search(root) {
            panic!("{e}");
        }
        let marker = "sr_client::harden_dll_search()";
        let ok = "x\nfn main() -> ExitCode {\n    // why\n    sr_client::harden_dll_search();\n    let a = 1;\n}";
        assert_eq!(dll_search_problem(ok, marker), None);
        let late = "fn x() {}\nfn main() {\n    let a = 1;\n    let b = 2;\n    sr_client::harden_dll_search();\n}";
        assert!(dll_search_problem(late, marker).is_some());
        assert!(dll_search_problem("fn other() {}", marker).is_some());
    }

    #[test]
    fn our_manifests_pass() {
        assert_eq!(manifest_problems(CONSOLE, false), Vec::<String>::new());
        assert_eq!(manifest_problems(APP, true), Vec::<String>::new());
        assert_eq!(
            manifest_problems(CONSOLE, true).len(),
            1,
            "console manifest has no comctl dependency"
        );
    }

    /// What link.exe actually embeds for console.manifest (read back from a test exe, M5-I).
    const MERGED: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0"><trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"></requestedExecutionLevel></requestedPrivileges></security></trustInfo><application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings><dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware><dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness><longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware></windowsSettings></application><ms_compatibility:compatibility xmlns:ms_compatibility="urn:schemas-microsoft-com:compatibility.v1" xmlns="urn:schemas-microsoft-com:compatibility.v1"><ms_compatibility:application xmlns:ms_compatibility="urn:schemas-microsoft-com:compatibility.v1"><ms_compatibility:supportedOS xmlns:ms_compatibility="urn:schemas-microsoft-com:compatibility.v1" Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"></ms_compatibility:supportedOS></ms_compatibility:application></ms_compatibility:compatibility></assembly>"#;

    #[test]
    fn linker_merged_and_prefixed_forms_pass() {
        assert_eq!(manifest_problems(MERGED, false), Vec::<String>::new());
        let prefixed = MERGED
            .replace("<longPathAware ", "<ws2:longPathAware ")
            .replace("</longPathAware>", "</ws2:longPathAware>");
        assert_eq!(manifest_problems(&prefixed, false), Vec::<String>::new());
        // tauri-build's default (what M4 shipped in sr-app) fails everything but comctl.
        let tauri_default = r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0"><dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency></assembly>"#;
        assert_eq!(manifest_problems(tauri_default, true).len(), 4);
    }

    #[test]
    fn each_rule_fails_on_its_own() {
        let off = |from: &str, to: &str| APP.replace(from, to);
        for (bad, why) in [
            (off("asInvoker", "requireAdministrator"), "asInvoker"),
            (off("8e0f7a12", "1f676c76"), "supportedOS"),
            (
                off(">true</longPathAware>", ">false</longPathAware>"),
                "longPathAware",
            ),
            (off("PerMonitorV2, PerMonitor", "System"), "PerMonitorV2"),
            (
                off("Microsoft.Windows.Common-Controls", "Other"),
                "Common-Controls",
            ),
        ] {
            let p = manifest_problems(&bad, true);
            assert_eq!(p.len(), 1, "{why}: {p:?}");
            assert!(p[0].contains(why), "{why}: {p:?}");
        }
        // A setting only mentioned in a comment does not count.
        let commented = "<assembly><!-- <longPathAware>true</longPathAware> --></assembly>";
        assert!(
            manifest_problems(commented, false)
                .iter()
                .any(|p| p.contains("longPathAware"))
        );
    }

    // Reads the test binary as a PE file: Windows only.
    #[cfg(windows)]
    #[test]
    fn xtask_itself_has_no_manifest_yet_and_is_reported() {
        // xtask has no build.rs, so rustc embeds no manifest: the reader must say so, not pass.
        let exe = std::env::current_exe().unwrap_or_else(|e| panic!("{e}"));
        let got = manifests_of_file(&exe).unwrap_or_else(|e| panic!("{e}"));
        assert!(got.is_empty(), "{got:?}");
        assert!(check_file(&exe, false).is_err());
    }
}
