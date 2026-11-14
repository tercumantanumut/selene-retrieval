//! Pinned third-party binaries for packaging (M2-I). Each archive and each file taken out of it is
//! sha256-pinned here; a mismatch fails and nothing unverified is ever staged.
//!
//! * `cargo xtask fetch-pdfium  [--dir <d>]`: bblanchon/pdfium-binaries `chromium/7881`
//!   (`pdfium-render` feature `pdfium_7881`, the build the hostile corpus ran on). `pdfium.dll` is
//!   **unsigned upstream**; the pin is its integrity check (m1-b B1). On macOS (Apple silicon) the
//!   same release's `libpdfium.dylib` ([`PDFIUM_MAC_ARM64`]), fetched with `/usr/bin/curl` and
//!   `/usr/bin/tar`.
//! * `cargo xtask fetch-runtime [--dir <d>]`: `DirectML.dll` 1.15.4 (NuGet `Microsoft.AI.DirectML`,
//!   app-local next to `sr-daemon.exe`, m1-d O2) and the four VC++ runtime DLLs 14.44.35211 from
//!   the MSVC 14.44 (VS 17.14) redistributable package `Microsoft.VC.14.44.17.14.CRT.Redist.X64.base`
//!   (Amendment 3.1: sr-daemon only; redist ≥ build toolset).
//! * `cargo xtask fetch-nsis    [--dir <d>]`: NSIS 3.13, the exact zip the Tauri bundler pins
//!   (`tauri-apps/binary-releases`, sha1 `DB14F8AF…` in tauri-bundler, sha256 below).
//!
//! Default directory: `<workspace>/vendor-bin` (gitignored). Downloads use the inbox `curl.exe`
//! (`--proto =https`), extraction the inbox `tar.exe` (bsdtar: tgz, zip, nupkg, vsix). Upstream
//! versions were checked on 2026-10-18 (docs/research/m2-i-installer.md §1).

use sr_providers::models::sha256_hex;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One pinned download and the files staged from it.
pub struct Archive {
    /// Directory under the vendor dir (`pdfium-7881`).
    pub dir: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    /// Archive member → staged file name, with the staged file's sha256.
    pub files: &'static [(&'static str, &'static str, &'static str)],
    /// Build tools that need their whole folder (makensis, wix): this archive folder becomes
    /// `<vendor>/<dir>`, and `files` names are relative to it. `None`: only `files` are copied.
    pub tree: Option<&'static str>,
}

pub const PDFIUM: Archive = Archive {
    tree: None,
    dir: "pdfium-7881",
    url: "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/7881/pdfium-win-x64.tgz",
    sha256: "73cc0de638ac2095e7445bf56a38200a5b7c7ca0e9f4ba144598f2457377ac08",
    files: &[
        (
            "bin/pdfium.dll",
            "pdfium.dll",
            "79d4676b656cfb1abcea88f9ade3b4b0826c5200382db5f4ec72a636c598c118",
        ),
        ("LICENSE", "LICENSE-pdfium-binaries.txt", ""),
        ("licenses/pdfium.txt", "LICENSE-pdfium.txt", ""),
    ],
};

/// macOS on Apple silicon (Amendment 18): the same bblanchon/pdfium-binaries `chromium/7881`
/// release, `pdfium-mac-arm64.tgz`. Archive pin = the asset `digest` of the GitHub release API,
/// cross-checked by download (2026-11-14); the API's digest for `pdfium-win-x64.tgz` equals the
/// [`PDFIUM`] pin above. `libpdfium.dylib` is ad-hoc (linker) signed upstream and links only
/// system frameworks (AppKit, CoreFoundation, Foundation, CoreGraphics, libSystem); install name
/// `./libpdfium.dylib`, which does not matter because the worker opens it by absolute path.
/// Packaging (phase 4) re-signs it with the app's identity.
pub const PDFIUM_MAC_ARM64: Archive = Archive {
    tree: None,
    dir: "pdfium-7881",
    url: "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/7881/pdfium-mac-arm64.tgz",
    sha256: "52e94ca5aa8847934330daf3f8150c190682c5ca93831468794f8b90d4392e40",
    files: &[
        (
            "lib/libpdfium.dylib",
            "libpdfium.dylib",
            "1bc45b15466b34cef96641ce25c77a876e70010c6b114f909dda2f5325fc5bd7",
        ),
        (
            "LICENSE",
            "LICENSE-pdfium-binaries.txt",
            "ba26c1263131696b86c10496b5066b918a20b7161822a80c274d0080105f6c93",
        ),
        (
            "licenses/pdfium.txt",
            "LICENSE-pdfium.txt",
            "1fe9dea718fbd75cf149adaf4d8a22a4335604d964ddb76d1b45383dec8668c9",
        ),
    ],
};

/// The pdfium build for the platform xtask runs on.
pub fn pdfium_for_host() -> Result<&'static Archive, String> {
    if cfg!(windows) {
        Ok(&PDFIUM)
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok(&PDFIUM_MAC_ARM64)
    } else {
        Err(
            "fetch-pdfium: no pinned pdfium build for this platform (Windows x64 and macOS on \
             Apple silicon only)"
                .into(),
        )
    }
}

pub const DIRECTML: Archive = Archive {
    tree: None,
    dir: "directml-1.15.4",
    url: "https://api.nuget.org/v3-flatcontainer/microsoft.ai.directml/1.15.4/microsoft.ai.directml.1.15.4.nupkg",
    sha256: "4e7cb7ddce8cf837a7a75dc029209b520ca0101470fcdf275c1f49736a3615b9",
    files: &[
        (
            "bin/x64-win/DirectML.dll",
            "DirectML.dll",
            "9c9e6d822561c6c41b90e6994b3e8857cf1d66dbfb1e0c4c799c7c89b4e92da1",
        ),
        (
            "LICENSE.txt",
            "LICENSE-DirectML.txt",
            "a05138e3a085ff60a44881eedfa58dccb03ecc1d7b1f6ae888418e8c2fec4b8d",
        ),
        (
            "ThirdPartyNotices.txt",
            "ThirdPartyNotices-DirectML.txt",
            "2c95795c13ff48a58b6ed916f37901c23d964b5d9d601af422f17ad2172e7950",
        ),
    ],
};

/// Member path inside the redist vsix (redist folder 14.44.35112 holds the 14.44.35211 files).
macro_rules! crt {
    ($f:literal) => {
        concat!(
            "Contents/VC/Redist/MSVC/14.44.35112/x64/Microsoft.VC143.CRT/",
            $f
        )
    };
}
#[cfg(test)]
const CRT_DIR: &str = crt!("");

pub const VC_RUNTIME: Archive = Archive {
    tree: None,
    dir: "vcredist-14.44.35211",
    url: "https://download.visualstudio.microsoft.com/download/pr/45d3b8dd-bced-4b37-9974-142f748d710c/4aaf54db0bfc9435f7c3660e1a00237a4b556042bfeea64bde44c2e0194e6ee5/Microsoft.VC.14.44.17.14.CRT.Redist.X64.base.vsix",
    sha256: "4aaf54db0bfc9435f7c3660e1a00237a4b556042bfeea64bde44c2e0194e6ee5",
    files: &[
        (
            crt!("vcruntime140.dll"),
            "vcruntime140.dll",
            "d5e4d9a3e835fa679450145d6a7d94e36573a509317111904d9b3712c30d9066",
        ),
        (
            crt!("vcruntime140_1.dll"),
            "vcruntime140_1.dll",
            "1f2d41c4aa5db0bc33ebf7b66d72943a817d7ce6cbe880502a9403823633093f",
        ),
        (
            crt!("msvcp140.dll"),
            "msvcp140.dll",
            "0f885b509a685d2bbfa652fed26b5fb31d88fbdab0a978c641d1c7b8aa460aa9",
        ),
        (
            crt!("msvcp140_1.dll"),
            "msvcp140_1.dll",
            "bfad5aef4c63a669e3c140655cdfdf395b6c979b400a447bd5dcb65ed8826c3d",
        ),
    ],
};

pub const NSIS: Archive = Archive {
    tree: Some("nsis-3.13"),
    dir: "nsis-3.13",
    url: "https://github.com/tauri-apps/binary-releases/releases/download/nsis-3.13/nsis-3.13.zip",
    sha256: "ba63dffc4410ee89193e1cb5a41989991bd77c61068da17e3156d136b7b0b3d8",
    // The whole tree is extracted (makensis needs its Stubs/Include/Plugins); makensis is pinned.
    files: &[(
        "nsis-3.13/makensis.exe",
        "makensis.exe",
        "4ec390d0f96f9728507fbc2f4e91311dd90bfc6c8ddfe2159357b3651ae151c0",
    )],
};

/// WiX Toolset **v5.0.2** (M5-I, MSI build tool only; nothing of it is installed). NuGet `wix`
/// 5.0.2 (2024-10-05), licence **MS-RL**, no licence-acceptance step. Not v6/v7: from v6 the
/// binaries fall under the Open Source Maintenance Fee EULA (organisations above $10k annual
/// revenue must sponsor the project) and v7 enforces it with `-acceptEula wix7`
/// (docs.firegiant.com/wix/osmf, checked 2026-10-19); accepting that is an owner decision.
/// The .NET tool runs on any installed .NET runtime >= 6 (`rollForward: Major`). The assemblies
/// are not Authenticode-signed, so the archive and content pins are the integrity check.
pub const WIX: Archive = Archive {
    tree: Some("tools/net6.0/any"),
    dir: "wix-5.0.2",
    url: "https://api.nuget.org/v3-flatcontainer/wix/5.0.2/wix.5.0.2.nupkg",
    sha256: "f30ef0c74e2a986126539c5780be93ac24e8136eaf723b1937b26272703ae173",
    files: &[
        (
            "wix.exe",
            "wix.exe",
            "8f76d8c2119853f2fa98e52a92af689b1ecaa1ebc42d47199265b1fa1b5c9560",
        ),
        (
            "wix.dll",
            "wix.dll",
            "3e7e0bb70c7d945f2ada2e902ddb5f36e99075a7b065de5499b9cd92aa6db117",
        ),
        (
            "WixToolset.Core.dll",
            "WixToolset.Core.dll",
            "823583620ebb75fbdd124c7dc952d88b2ef7a9863b0a696f2c13173887be7c65",
        ),
        (
            "WixToolset.Core.WindowsInstaller.dll",
            "WixToolset.Core.WindowsInstaller.dll",
            "da589e8ec95c75b80d74e5dcae04e74b20091682178cbb7b8f4f51be273091ed",
        ),
        (
            "WixToolset.Data.dll",
            "WixToolset.Data.dll",
            "ad5326009e384a81c29b3f4b9b451e6863b402521acfa74866c908022bdeccbb",
        ),
        (
            "wix.runtimeconfig.json",
            "wix.runtimeconfig.json",
            "ec90df72c69bb4788924ac4569085efbfab3bec675ba756e7c8b9ee62fcaf4da",
        ),
    ],
};

/// WiX UI extension 5.0.2 (the standard dialogs: Welcome, Progress, Finish, maintenance). MS-RL.
pub const WIX_UI: Archive = Archive {
    tree: None,
    dir: "wixext-ui-5.0.2",
    url: "https://api.nuget.org/v3-flatcontainer/wixtoolset.ui.wixext/5.0.2/wixtoolset.ui.wixext.5.0.2.nupkg",
    sha256: "5ef2c707614b9f70b6bbadd2d4abcb4124efee215e9b16bfbc80113079a604c7",
    files: &[(
        "wixext5/WixToolset.UI.wixext.dll",
        "WixToolset.UI.wixext.dll",
        "5ce7f958b6a2b57bcb98c19615c5a3da46976a6e78a6af055fdef99e9e9e8f06",
    )],
};

/// WebView2 Evergreen **bootstrapper** (Microsoft, about 1.8 MB), bundled into the standalone
/// installer so a Windows 10 machine without WebView2 can open the app window (owner decision:
/// bundle it, +1.8 MB). Microsoft replaces this file in place behind a permanent link, so unlike
/// the other inputs it can't be content-pinned: it is accepted only with a **valid Authenticode
/// signature by Microsoft Corporation** (checked with System32 PowerShell's
/// `Get-AuthenticodeSignature`), and its sha256 and version go into `package-report.json`.
pub const WEBVIEW2_URL: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";
pub const WEBVIEW2_DIR: &str = "webview2";
pub const WEBVIEW2_FILE: &str = "MicrosoftEdgeWebview2Setup.exe";

/// WebView2 Evergreen **Standalone** x64 runtime installer (about 214 MB, no internet needed),
/// for the separate offline installer variant (`cargo xtask package --webview2-offline`; owner
/// default 2026-10-19: the online bootstrapper is the default, the offline file is a variant).
///
/// **Pinned** (M5-I): Microsoft's permanent link [`WEBVIEW2_OFFLINE_LATEST`] redirects to a
/// per-release URL, which is pinned here with its sha256, and the file must also carry a valid
/// Microsoft Authenticode signature. The installer stub inside is Evergreen: after install the
/// runtime updates itself, so the pin only fixes what ships offline. When Microsoft retires the
/// pinned URL (download fails) run `cargo xtask fetch-webview2 --offline --latest`: it resolves
/// the permanent link, checks the signature and prints the new constants to paste here.
pub const WEBVIEW2_OFFLINE_URL: &str = "https://msedge.sf.dl.delivery.mp.microsoft.com/filestreamingservice/files/35871e19-d11b-432d-9d1e-7be0ff286540/MicrosoftEdgeWebView2RuntimeInstallerX64.exe";
pub const WEBVIEW2_OFFLINE_SHA256: &str =
    "0625fb5c1e75be58a164e14f95aa58a3e5badb859b3d17c952385f3e4a9ff3e8";
/// Size of the pinned file (214 156 024 bytes, fetched 2026-10-19; stub version 1.3.287.9).
pub const WEBVIEW2_OFFLINE_BYTES: u64 = 214_156_024;
pub const WEBVIEW2_OFFLINE_LATEST: &str = "https://go.microsoft.com/fwlink/?linkid=2124701";
pub const WEBVIEW2_OFFLINE_FILE: &str = "MicrosoftEdgeWebView2RuntimeInstallerX64.exe";

/// What `fetch_webview2` verified.
#[derive(Debug, Clone)]
pub struct Webview2 {
    pub path: PathBuf,
    pub sha256: String,
    pub version: String,
}

fn powershell() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

/// `(valid Microsoft signature, file version)` of `path`.
fn microsoft_signed(path: &Path) -> Result<(bool, String), String> {
    let script = "$s = Get-AuthenticodeSignature -LiteralPath $env:SR_WV2; \
         $v = (Get-Item -LiteralPath $env:SR_WV2).VersionInfo.FileVersion; \
         Write-Output (\"$($s.Status)|$($s.SignerCertificate.Subject)|$v\")";
    let out = Command::new(powershell())
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("SR_WV2", path)
        .output()
        .map_err(|e| format!("could not run PowerShell: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut it = text.split('|');
    let status = it.next().unwrap_or_default();
    let subject = it.next().unwrap_or_default();
    let version = it.next().unwrap_or_default().to_string();
    let ok = status == "Valid"
        && subject.contains("O=Microsoft Corporation")
        && subject.contains("CN=Microsoft Corporation");
    Ok((ok, version))
}

/// Ensures `<vendor>/webview2/MicrosoftEdgeWebview2Setup.exe` exists and is Microsoft-signed
/// (downloads it when missing or not valid).
pub fn fetch_webview2(vendor: &Path) -> Result<Webview2, String> {
    fetch_wv2(vendor, WEBVIEW2_URL, WEBVIEW2_FILE, "bootstrapper")
}

/// Ensures `<vendor>/webview2/MicrosoftEdgeWebView2RuntimeInstallerX64.exe` (the standalone
/// runtime for the offline installer variant) exists and is Microsoft-signed.
/// Pinned: the per-release URL and sha256 above (M5-I), plus the Microsoft signature.
pub fn fetch_webview2_offline(vendor: &Path) -> Result<Webview2, String> {
    let dir = vendor.join(WEBVIEW2_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let dest = dir.join(WEBVIEW2_OFFLINE_FILE);
    download(WEBVIEW2_OFFLINE_URL, &dest, WEBVIEW2_OFFLINE_SHA256).map_err(|e| {
        format!(
            "{e}\n  The pinned WebView2 standalone runtime is no longer available or changed. Run \
             `cargo xtask fetch-webview2 --offline --latest`, check its output and update the \
             WEBVIEW2_OFFLINE_* pins in xtask/src/fetch.rs."
        )
    })?;
    let (ok, version) = microsoft_signed(&dest)?;
    if !ok {
        return Err(format!(
            "{}: matches the pin but is not validly signed by Microsoft; refusing it",
            dest.display()
        ));
    }
    println!("fetch: webview2 standalone runtime ok (pinned sha256, Microsoft-signed, {version})");
    Ok(Webview2 {
        path: dest,
        sha256: WEBVIEW2_OFFLINE_SHA256.into(),
        version,
    })
}

/// `fetch-webview2 --offline --latest`: resolves Microsoft's permanent link to today's
/// per-release URL, downloads it to `<vendor>/webview2/latest/`, checks the signature and prints
/// the constants for a pin update. Never changes the pin by itself. Downloads to
/// `<vendor>/webview2-latest/`.
fn webview2_offline_latest(vendor: &Path) -> Result<(), String> {
    let out = Command::new(system_tool("curl.exe"))
        .args(["--silent", "--show-error", "--proto", "=https", "--tlsv1.2"])
        .args(["--output", "NUL", "--write-out", "%{redirect_url}"])
        .arg(WEBVIEW2_OFFLINE_LATEST)
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !url.starts_with("https://") || !url.ends_with(WEBVIEW2_OFFLINE_FILE) {
        return Err(format!(
            "unexpected redirect from {WEBVIEW2_OFFLINE_LATEST}: {url:?}"
        ));
    }
    // Kept apart from the pinned file: <vendor>/webview2-latest/webview2/<file>.
    let w = fetch_wv2(
        &vendor.join("webview2-latest"),
        &url,
        WEBVIEW2_OFFLINE_FILE,
        "standalone runtime (latest)",
    )?;
    let bytes = std::fs::metadata(&w.path).map(|m| m.len()).unwrap_or(0);
    println!("fetch: new pin candidates (paste into xtask/src/fetch.rs after review):");
    println!("  WEBVIEW2_OFFLINE_URL    = \"{url}\"");
    println!("  WEBVIEW2_OFFLINE_SHA256 = \"{}\"", w.sha256);
    println!(
        "  WEBVIEW2_OFFLINE_BYTES  = {bytes}  (stub version {})",
        w.version
    );
    Ok(())
}

fn fetch_wv2(vendor: &Path, url: &str, file: &str, what: &str) -> Result<Webview2, String> {
    let dir = vendor.join(WEBVIEW2_DIR);
    let dest = dir.join(file);
    if dest.is_file() {
        let (ok, version) = microsoft_signed(&dest)?;
        if ok {
            let sha256 = file_sha(&dest)?.unwrap_or_default();
            println!("fetch: webview2 {what} ok (cached, Microsoft-signed, {version})");
            return Ok(Webview2 {
                path: dest,
                sha256,
                version,
            });
        }
        let _ = std::fs::remove_file(&dest);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let part = dest.with_extension("part");
    println!("fetch: {} <- {url}", dest.display());
    let status = Command::new(system_tool("curl.exe"))
        .args(["--fail", "--location", "--silent", "--show-error"])
        .args(["--retry", "3", "--proto", "=https", "--tlsv1.2", "--output"])
        .arg(&part)
        .arg(url)
        .status()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&part);
        return Err(format!("download of the WebView2 {what} failed ({status})"));
    }
    let (ok, version) = microsoft_signed(&part)?;
    if !ok {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "the WebView2 {what} is not validly signed by Microsoft; refusing it"
        ));
    }
    std::fs::rename(&part, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let sha256 = file_sha(&dest)?.unwrap_or_default();
    println!("fetch: webview2 {what} verified (Microsoft-signed, {version}, sha256 {sha256})");
    Ok(Webview2 {
        path: dest,
        sha256,
        version,
    })
}

/// `fetch-webview2 [--offline [--latest]] [--dir <d>]`.
pub fn fetch_webview2_cmd(args: &[String]) -> Result<(), String> {
    let offline = args.iter().any(|a| a == "--offline");
    let latest = args.iter().any(|a| a == "--latest");
    let rest: Vec<String> = args
        .iter()
        .filter(|a| *a != "--offline" && *a != "--latest")
        .cloned()
        .collect();
    let vendor = vendor_arg(&rest)?;
    match (offline, latest) {
        (false, false) => fetch_webview2(&vendor).map(|_| ()),
        (true, false) => fetch_webview2_offline(&vendor).map(|_| ()),
        (true, true) => webview2_offline_latest(&vendor),
        (false, true) => Err("--latest only applies to --offline (the bootstrapper is \
                              signature-checked, not content-pinned)"
            .into()),
    }
}

pub fn fetch_wix(args: &[String]) -> Result<(), String> {
    let v = vendor_arg(args)?;
    fetch(&WIX, &v)?;
    fetch(&WIX_UI, &v).map(|_| ())
}

pub fn workspace_root() -> PathBuf {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    here.parent().unwrap_or(here).to_path_buf()
}

pub fn default_vendor_dir() -> PathBuf {
    workspace_root().join("vendor-bin")
}

fn file_sha(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(sha256_hex(&b))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// System32 tool by absolute path (never PATH: Git's own `tar`/`curl` must not be picked up).
#[cfg(windows)]
fn system_tool(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("System32").join(name)
}

/// The system's own tool by absolute path (`/usr/bin/curl`, `/usr/bin/tar`; never PATH, so a
/// Homebrew or other copy is not picked up). `name` is the Windows file name.
#[cfg(not(windows))]
fn system_tool(name: &str) -> PathBuf {
    PathBuf::from("/usr/bin").join(name.trim_end_matches(".exe"))
}

/// Downloads `url` to `dest` (via `<dest>.part`) unless `dest` already has `sha256`.
pub fn download(url: &str, dest: &Path, sha256: &str) -> Result<(), String> {
    if file_sha(dest)?.as_deref() == Some(sha256) {
        return Ok(());
    }
    let part = dest.with_extension("part");
    println!("fetch: {} <- {url}", dest.display());
    let status = Command::new(system_tool("curl.exe"))
        .args(["--fail", "--location", "--silent", "--show-error"])
        .args(["--retry", "3", "--proto", "=https", "--tlsv1.2", "--output"])
        .arg(&part)
        .arg(url)
        .status()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&part);
        return Err(format!("download of {url} failed ({status})"));
    }
    let got = file_sha(&part)?.unwrap_or_default();
    if got != sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "{url}: sha256 {got} does not match the pin {sha256}"
        ));
    }
    std::fs::rename(&part, dest).map_err(|e| format!("{}: {e}", dest.display()))
}

/// Ensures every staged file of `a` exists in `<vendor>/<a.dir>` with its pinned hash. Returns
/// that directory.
pub fn fetch(a: &Archive, vendor: &Path) -> Result<PathBuf, String> {
    let dir = vendor.join(a.dir);
    let all_ok = a.files.iter().all(|(_, out, sha)| {
        let p = dir.join(out);
        if sha.is_empty() {
            p.is_file()
        } else {
            matches!(file_sha(&p), Ok(Some(h)) if h == *sha)
        }
    });
    if all_ok {
        println!("fetch: {} ok (cached)", a.dir);
        return Ok(dir);
    }
    std::fs::create_dir_all(vendor).map_err(|e| format!("{}: {e}", vendor.display()))?;
    let name = a.url.rsplit('/').next().unwrap_or("download");
    let archive = vendor.join(format!("{}-{name}", a.dir));
    download(a.url, &archive, a.sha256)?;
    let tmp = vendor.join(format!("{}.extract", a.dir));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let status = Command::new(system_tool("tar.exe"))
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&tmp)
        .status()
        .map_err(|e| format!("could not run tar: {e}"))?;
    if !status.success() {
        return Err(format!(
            "extracting {} failed ({status})",
            archive.display()
        ));
    }
    let _ = std::fs::remove_dir_all(&dir);
    if let Some(tree) = a.tree {
        // Whole tree (e.g. the archive's top folder `nsis-3.13` becomes <vendor>/nsis-3.13).
        std::fs::rename(tmp.join(tree), &dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    } else {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for (member, out, _) in a.files {
            std::fs::copy(tmp.join(member), dir.join(out))
                .map_err(|e| format!("{} missing from {}: {e}", member, archive.display()))?;
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    for (_, out, sha) in a.files {
        let p = dir.join(out);
        let got = file_sha(&p)?.ok_or_else(|| format!("{} not extracted", p.display()))?;
        if !sha.is_empty() && got != *sha {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!(
                "{}: sha256 {got} does not match the pin {sha}",
                p.display()
            ));
        }
    }
    println!("fetch: {} verified in {}", a.dir, dir.display());
    Ok(dir)
}

fn vendor_arg(args: &[String]) -> Result<PathBuf, String> {
    let mut dir = default_vendor_dir();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => dir = it.next().ok_or("--dir needs a path")?.into(),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(dir)
}

pub fn fetch_pdfium(args: &[String]) -> Result<(), String> {
    fetch(pdfium_for_host()?, &vendor_arg(args)?).map(|_| ())
}

pub fn fetch_runtime(args: &[String]) -> Result<(), String> {
    let v = vendor_arg(args)?;
    fetch(&DIRECTML, &v)?;
    fetch(&VC_RUNTIME, &v).map(|_| ())
}

pub fn fetch_nsis(args: &[String]) -> Result<(), String> {
    fetch(&NSIS, &vendor_arg(args)?).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_are_well_formed() {
        for a in [
            &PDFIUM,
            &PDFIUM_MAC_ARM64,
            &DIRECTML,
            &VC_RUNTIME,
            &NSIS,
            &WIX,
            &WIX_UI,
        ] {
            assert_eq!(a.sha256.len(), 64, "{}", a.dir);
            assert!(a.url.starts_with("https://"), "{}", a.url);
            assert!(!a.files.is_empty());
            for (_, _, sha) in a.files {
                assert!(sha.is_empty() || sha.len() == 64, "{}", a.dir);
            }
        }
        // Every DLL we ship from a third party is pinned by content, not just by archive.
        for a in [&PDFIUM, &PDFIUM_MAC_ARM64, &DIRECTML, &VC_RUNTIME] {
            for (_, out, sha) in a.files {
                if out.ends_with(".dll") || out.ends_with(".dylib") {
                    assert_eq!(sha.len(), 64, "{out} must be content-pinned");
                }
            }
        }
        for (member, _, _) in VC_RUNTIME.files {
            assert!(member.starts_with(CRT_DIR), "{member}");
        }
        // Build tools that need their whole folder; every pinned file of WiX is content-pinned.
        assert_eq!(NSIS.tree, Some("nsis-3.13"));
        assert!(WIX.tree.is_some() && WIX_UI.tree.is_none() && PDFIUM.tree.is_none());
        // Both pdfium builds come from the same release, into the folder the tests look in.
        assert_eq!(PDFIUM_MAC_ARM64.dir, PDFIUM.dir);
        assert!(PDFIUM_MAC_ARM64.tree.is_none());
        let release = |u: &str| u.rsplit_once('/').map(|(r, _)| r.to_string());
        assert_eq!(release(PDFIUM_MAC_ARM64.url), release(PDFIUM.url));
        for (_, out, sha) in WIX.files.iter().chain(WIX_UI.files) {
            assert_eq!(sha.len(), 64, "{out}");
        }
        assert!(WIX.url.contains("/wix/5.0.2/") && WIX_UI.url.contains("/5.0.2/"));
    }

    #[test]
    fn webview2_offline_is_pinned() {
        assert!(WEBVIEW2_OFFLINE_URL.starts_with("https://"));
        assert!(WEBVIEW2_OFFLINE_URL.ends_with(WEBVIEW2_OFFLINE_FILE));
        assert_ne!(
            WEBVIEW2_OFFLINE_URL, WEBVIEW2_OFFLINE_LATEST,
            "pin the release URL, not the fwlink"
        );
        assert_eq!(WEBVIEW2_OFFLINE_SHA256.len(), 64);
        assert!(
            WEBVIEW2_OFFLINE_SHA256
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
        const { assert!(WEBVIEW2_OFFLINE_BYTES > 100_000_000) };
    }

    #[test]
    fn the_four_vc_runtime_dlls() {
        let names: Vec<&str> = VC_RUNTIME.files.iter().map(|f| f.1).collect();
        assert_eq!(
            names,
            [
                "vcruntime140.dll",
                "vcruntime140_1.dll",
                "msvcp140.dll",
                "msvcp140_1.dll"
            ]
        );
    }
}
