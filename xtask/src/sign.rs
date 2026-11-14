//! Code signing hooks (M2-I). `cargo xtask sign <file>...` signs with `signtool` **only when signing
//! is configured**, verifies every signature it made, and otherwise reports the files as UNSIGNED.
//! It never fakes a signature, never self-signs, and never reads a secret: credentials stay in the
//! environment of the signing tool.
//!
//! Configuration (environment variable *names* only are inspected, values are paths/ids):
//!
//! | Mode | Variables | signtool arguments |
//! |---|---|---|
//! | Artifact Signing (Azure, formerly Trusted Signing) | `SR_SIGN_DLIB` = path of `Azure.CodeSigning.Dlib.dll`, `SR_SIGN_METADATA` = path of its `metadata.json` (endpoint, account, certificate profile). Azure auth comes from the dlib's own credential chain (`AZURE_*`, managed identity, `az login`) | `/fd SHA256 /tr <ts> /td SHA256 /dlib <dlib> /dmdf <metadata>` |
//! | Certificate in the user store (HSM / EV token) | `SR_SIGN_CERT_SHA1` = thumbprint | `/fd SHA256 /tr <ts> /td SHA256 /sha1 <thumbprint>` |
//!
//! Optional: `SR_SIGNTOOL` (signtool path; default: newest Windows 10/11 SDK x64 signtool),
//! `SR_SIGN_TIMESTAMP_URL` (default `http://timestamp.acs.microsoft.com`, Artifact Signing's
//! RFC 3161 service), `SR_REQUIRE_SIGNING=1` (release CI: fail instead of producing unsigned
//! artifacts).
//!
//! Tauri can call the same entry point as its `bundle.windows.signCommand` (`cargo xtask sign %1`),
//! and the standalone NSIS script uses it through `!finalize` / `!uninstfinalize`.

use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_TIMESTAMP: &str = "http://timestamp.acs.microsoft.com";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    ArtifactSigning { dlib: PathBuf, metadata: PathBuf },
    CertStore { sha1: String },
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::ArtifactSigning { .. } => "Artifact Signing",
            Mode::CertStore { .. } => "certificate store",
        }
    }
}

/// Signing configuration from an environment lookup (injectable for tests).
pub fn mode_from(get: impl Fn(&str) -> Option<String>) -> Result<Option<Mode>, String> {
    let nonempty = |k: &str| get(k).filter(|v| !v.trim().is_empty());
    match (nonempty("SR_SIGN_DLIB"), nonempty("SR_SIGN_METADATA")) {
        (Some(d), Some(m)) => {
            return Ok(Some(Mode::ArtifactSigning {
                dlib: d.into(),
                metadata: m.into(),
            }));
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err("Artifact Signing needs both SR_SIGN_DLIB and SR_SIGN_METADATA".into());
        }
        (None, None) => {}
    }
    if let Some(t) = nonempty("SR_SIGN_CERT_SHA1") {
        let t: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        if t.len() != 40 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("SR_SIGN_CERT_SHA1 must be a 40-hex-digit thumbprint".into());
        }
        return Ok(Some(Mode::CertStore { sha1: t }));
    }
    Ok(None)
}

pub fn mode() -> Result<Option<Mode>, String> {
    mode_from(|k| std::env::var(k).ok())
}

pub fn signing_required() -> bool {
    std::env::var("SR_REQUIRE_SIGNING").is_ok_and(|v| v == "1")
}

/// Newest SDK `x64\signtool.exe` (or `SR_SIGNTOOL`).
pub fn signtool() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("SR_SIGNTOOL") {
        return Ok(p.into());
    }
    let pf86 = std::env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"));
    let bin = pf86.join("Windows Kits").join("10").join("bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&bin)
        .map_err(|e| format!("{}: {e}", bin.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("x64").join("signtool.exe").is_file())
        .collect();
    versions.sort_by_key(|p| sdk_version_key(p));
    versions
        .pop()
        .map(|p| p.join("x64").join("signtool.exe"))
        .ok_or_else(|| "signtool.exe not found (install the Windows SDK or set SR_SIGNTOOL)".into())
}

fn sdk_version_key(p: &Path) -> Vec<u32> {
    p.file_name()
        .map(|n| {
            n.to_string_lossy()
                .split('.')
                .map(|x| x.parse().unwrap_or(0))
                .collect()
        })
        .unwrap_or_default()
}

pub fn sign_args(mode: &Mode, timestamp: &str) -> Vec<String> {
    let mut a: Vec<String> = [
        "sign", "/v", "/fd", "SHA256", "/tr", timestamp, "/td", "SHA256",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    match mode {
        Mode::ArtifactSigning { dlib, metadata } => {
            a.push("/dlib".into());
            a.push(dlib.display().to_string());
            a.push("/dmdf".into());
            a.push(metadata.display().to_string());
        }
        Mode::CertStore { sha1 } => {
            a.push("/sha1".into());
            a.push(sha1.clone());
        }
    }
    a
}

/// Result of a signing request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Signed(&'static str),
    Unsigned,
}

/// Signs `files` if configured (and verifies them); otherwise reports them unsigned.
pub fn sign_files(files: &[PathBuf]) -> Result<Outcome, String> {
    let Some(mode) = mode()? else {
        if signing_required() {
            return Err("SR_REQUIRE_SIGNING=1 but no signing is configured \
                        (SR_SIGN_DLIB + SR_SIGN_METADATA, or SR_SIGN_CERT_SHA1)"
                .into());
        }
        for f in files {
            println!(
                "sign: UNSIGNED {} (no signing credentials configured)",
                f.display()
            );
        }
        return Ok(Outcome::Unsigned);
    };
    if files.is_empty() {
        return Ok(Outcome::Signed(mode.label()));
    }
    let tool = signtool()?;
    let ts = std::env::var("SR_SIGN_TIMESTAMP_URL").unwrap_or_else(|_| DEFAULT_TIMESTAMP.into());
    let status = Command::new(&tool)
        .args(sign_args(&mode, &ts))
        .args(files)
        .status()
        .map_err(|e| format!("could not run {}: {e}", tool.display()))?;
    if !status.success() {
        return Err(format!(
            "signtool sign failed ({status}); nothing is reported as signed"
        ));
    }
    let status = Command::new(&tool)
        .args(["verify", "/pa", "/all"])
        .args(files)
        .status()
        .map_err(|e| format!("could not run {}: {e}", tool.display()))?;
    if !status.success() {
        return Err(format!("signtool verify failed after signing ({status})"));
    }
    for f in files {
        println!("sign: SIGNED ({}) {}", mode.label(), f.display());
    }
    Ok(Outcome::Signed(mode.label()))
}

/// `cargo xtask sign <file>...`
pub fn main(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: xtask sign <file>...".into());
    }
    let files: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();
    for f in &files {
        if !f.is_file() {
            return Err(format!("{}: not a file", f.display()));
        }
    }
    sign_files(&files).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn no_configuration_means_unsigned() {
        assert_eq!(mode_from(env(&[])), Ok(None));
        assert_eq!(mode_from(env(&[("SR_SIGN_CERT_SHA1", "  ")])), Ok(None));
    }

    #[test]
    fn artifact_signing_needs_both_paths() {
        assert!(mode_from(env(&[("SR_SIGN_DLIB", r"C:\d.dll")])).is_err());
        assert!(mode_from(env(&[("SR_SIGN_METADATA", r"C:\m.json")])).is_err());
        let m = mode_from(env(&[
            ("SR_SIGN_DLIB", r"C:\d.dll"),
            ("SR_SIGN_METADATA", r"C:\m.json"),
        ]));
        assert!(matches!(m, Ok(Some(Mode::ArtifactSigning { .. }))));
        let Ok(Some(m)) = m else { return };
        let a = sign_args(&m, DEFAULT_TIMESTAMP);
        for want in [
            "/fd",
            "SHA256",
            "/tr",
            "/td",
            "/dlib",
            r"C:\d.dll",
            "/dmdf",
            r"C:\m.json",
        ] {
            assert!(a.iter().any(|x| x == want), "{want} missing from {a:?}");
        }
    }

    #[test]
    fn thumbprint_is_validated() {
        assert!(mode_from(env(&[("SR_SIGN_CERT_SHA1", "abc")])).is_err());
        let t = "0123456789abcdef0123456789ABCDEF01234567";
        let m = mode_from(env(&[("SR_SIGN_CERT_SHA1", t)]));
        assert_eq!(m, Ok(Some(Mode::CertStore { sha1: t.into() })));
    }

    // `C:\K\10.0.…` paths only have a Windows file name (the version) on Windows.
    #[cfg(windows)]
    #[test]
    fn sdk_versions_sort_numerically() {
        let mut v = [
            PathBuf::from(r"C:\K\10.0.9200.0"),
            PathBuf::from(r"C:\K\10.0.22621.0"),
            PathBuf::from(r"C:\K\10.0.19041.0"),
        ];
        v.sort_by_key(|p| sdk_version_key(p));
        assert!(v[2].ends_with("10.0.22621.0"));
    }
}
