//! `cargo xtask package`: release build, staging and the per-user NSIS installer (M2-I), or the
//! per-user MSI (M5-I).
//!
//! ```text
//! cargo xtask package [--profile <p>] [--opt-size] [--no-build] [--stage-only] [--vendor <dir>]
//!                     [--webview2-offline] [--msi]
//! ```
//! `--msi` (M5-I): the same staged layout, signing hook and guards, packaged with **WiX 5.0.2**
//! (MS-RL; [`crate::fetch::WIX`] explains why not v6/v7) from `installer/wix/selene-retrieval.wxs`
//! plus a generated `files.wxs`, then ICE-validated: `SeleneRetrieval-<v>-x64[-offline].msi`,
//! with `SHA256SUMS-msi[-offline].txt` and `package-report-msi[-offline].json`. Per-user (no
//! admin prompt), the same sr-setup commands as the NSIS hooks, and the small custom-action DLL
//! `installer/msi-ca`.
//! Every exe is also checked for its application manifest (`check-manifest`).
//! WebView2 (owner default, M3): the installer bundles Microsoft's small online **bootstrapper**
//! (about 1.8 MB) and runs it only when WebView2 is missing. `--webview2-offline` builds the
//! separate **offline variant** `SeleneRetrieval-<v>-x64-offline-setup.exe` with the standalone
//! runtime (about +180 MB) instead, for PCs with no internet (LTSC, locked-down networks).
//! `--stage-only` stops after staging + signing (for the Tauri bundler's `beforeBundleCommand`;
//! its `bundle.resources` then maps `<target>/package/stage/*`, see m2-i-installer.md §6).
//!
//! 1. Pinned inputs: `fetch-pdfium`, `fetch-runtime` (DirectML + VC++ ×4), `fetch-nsis` and
//!    `fetch-models` (the bundled Minimum model), all sha256-checked ([`crate::fetch`]).
//! 2. Build, in **separate cargo invocations** so resolver-3 feature unification cannot drag the
//!    daemon's features (rayon/mmap via `sr-common/parallel-hash`) into the static exes (W2-H R-a):
//!    `sr-bridge` + `sr-launch` + `sr-extract` + `sr-app` together (as `check-crt` does), then
//!    `sr-daemon`, then `installer/sr-setup` (its own workspace).
//!    Default profile `dist` = `release` settings spelled out (thin LTO, one codegen unit; fat LTO
//!    runs LLVM out of memory on sr-daemon), defined on the command line with `--config` so the
//!    root `Cargo.toml` is untouched. `--opt-size` builds `dist-size` (additionally
//!    `opt-level = "s"`, the R25 size experiment).
//! 3. Guards on the exact files that ship: `check-crt` (5 static-CRT exes) and `check-delayload`
//!    (sr-daemon).
//! 4. Stage the install layout (below), sign our exes when signing is configured
//!    ([`crate::sign`]; otherwise they are reported UNSIGNED), generate the NSIS file list and run
//!    makensis on `installer/nsis/selene-retrieval.nsi` with `crates/app/nsis/hooks.nsh`.
//! 5. Write `SHA256SUMS[-msi][-offline].txt` and `package-report[-msi][-offline].json` (sizes vs R25,
//!    signing state, pins); the default NSIS build keeps the plain names CI uploads.
//!
//! Install layout (`%LOCALAPPDATA%\Selene Retrieval\`):
//! ```text
//! sr-app.exe sr-bridge.exe sr-launch.exe sr-extract.exe sr-setup.exe   static VC runtime
//! sr-daemon.exe + vcruntime140.dll vcruntime140_1.dll msvcp140.dll msvcp140_1.dll (app-local)
//! DirectML.dll (delay-loaded, signature-checked)   pdfium.dll (loaded by sr-extract, abs. path)
//! models\bge-small-en-v1.5\{model_optimized.onnx, tokenizer.json, config.json, ...}
//! licenses\...                                     uninstall.exe (written by NSIS)
//! ```

use crate::fetch::{self, Archive};
use crate::sign;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Exes with the static VC runtime (built together, without sr-daemon), with their package.
const STATIC_EXES: &[(&str, &str)] = &[
    ("sr-bridge", "sr-bridge"),
    ("sr-launch", "sr-bridge"),
    ("sr-extract", "sr-extract"),
    ("sr-app", "sr-app"),
];
const DAEMON: (&str, &str) = ("sr-daemon", "sr-mcp-server");
const SETUP: &str = "sr-setup";
/// The MSI custom-action DLL (installer/msi-ca, `sr_msi_ca.dll`; MSI Binary table only).
const MSI_CA: &str = "sr_msi_ca";
const MODEL_DIR: &str = "bge-small-en-v1.5";
/// R25 (docs/04-roadmap.md): measured `dist` installers ≈ 121.5–122.6 MB download, ≈ 334–339 MiB
/// installed (M2/M3 testers; M0's 106–114 MB / 285 MiB estimate is superseded, Amendment 6.3).
const R25_DOWNLOAD_MB: (f64, f64) = (121.0, 124.0);
const R25_INSTALLED_MIB: f64 = 339.0;

/// What sr-app is in this checkout (M2-A owns it; packaging adapts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum App {
    /// Tauri app with the built UI (`ui/dist`): built with `custom-protocol` (embedded assets).
    Tauri,
    /// Tauri app but no `ui/dist`: would load the dev server, so it ships without "Open" on the
    /// finish page and the report says so.
    TauriNoUi,
    /// The M0 console stub.
    Stub,
}

impl App {
    fn detect(root: &Path) -> App {
        let conf = root.join("crates").join("app").join("tauri.conf.json");
        if !conf.is_file() {
            App::Stub
        } else if root.join("ui").join("dist").join("index.html").is_file() {
            App::Tauri
        } else {
            App::TauriNoUi
        }
    }
    fn label(self) -> &'static str {
        match self {
            App::Tauri => "tauri",
            App::TauriNoUi => "tauri-without-ui",
            App::Stub => "stub",
        }
    }
}

/// `identifier` from tauri.conf.json (its WebView2/app-data folder is removed on uninstall).
fn bundle_id(root: &Path) -> Option<String> {
    let conf =
        std::fs::read_to_string(root.join("crates").join("app").join("tauri.conf.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&conf).ok()?;
    let id = v.get("identifier")?.as_str()?;
    // Used inside an NSIS path: plain reverse-DNS only.
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        .then(|| id.to_string())
}

struct Opts {
    profile: String,
    opt_size: bool,
    build: bool,
    stage_only: bool,
    webview2_offline: bool,
    /// Per-user MSI (WiX) instead of the NSIS setup (M5-I).
    msi: bool,
    vendor: PathBuf,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        profile: "dist".into(),
        opt_size: false,
        build: true,
        stage_only: false,
        webview2_offline: false,
        msi: false,
        vendor: fetch::default_vendor_dir(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--profile" => o.profile = it.next().ok_or("--profile needs a value")?.clone(),
            "--opt-size" => o.opt_size = true,
            "--no-build" => o.build = false,
            "--stage-only" => o.stage_only = true,
            "--webview2-offline" => o.webview2_offline = true,
            "--msi" => o.msi = true,
            "--vendor" => o.vendor = it.next().ok_or("--vendor needs a path")?.into(),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    if o.opt_size {
        if o.profile != "dist" {
            return Err("--opt-size selects the `dist-size` profile; drop --profile".into());
        }
        o.profile = "dist-size".into();
    }
    Ok(o)
}

/// `--config` flags that define the packaging profiles (cargo allows profiles in config).
fn profile_flags(profile: &str) -> Vec<String> {
    // Fat LTO is not usable for sr-daemon: LLVM ran out of memory after 69 min (~15 GB) on the
    // dev box (m2-i-installer.md §4). Thin LTO + one codegen unit is the packaging baseline;
    // `dist-size` adds opt-level "s" (R25 experiment, measured before it becomes the default).
    let mut kv: Vec<(&str, &str)> = vec![
        ("inherits", "\"release\""),
        ("lto", "\"thin\""),
        ("codegen-units", "1"),
    ];
    match profile {
        "dist" => {}
        "dist-size" => kv.push(("opt-level", "\"s\"")),
        _ => return Vec::new(),
    }
    kv.into_iter()
        .flat_map(|(k, v)| ["--config".to_string(), format!("profile.{profile}.{k}={v}")])
        .collect()
}

pub(crate) fn target_dir() -> PathBuf {
    let root = fetch::workspace_root();
    let t = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    if t.is_absolute() { t } else { root.join(t) }
}

pub(crate) fn profile_dir(profile: &str) -> PathBuf {
    let d = match profile {
        "dev" | "test" => "debug",
        "bench" => "release",
        p => p,
    };
    target_dir().join(d)
}

fn exe(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.exe"))
}

pub(crate) fn cargo(profile: &str, extra: &[String]) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(fetch::workspace_root())
        .args(["build", "--profile", profile])
        .args(profile_flags(profile))
        .args(extra);
    println!("package: running {cmd:?}");
    let st = cmd
        .status()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("cargo build failed ({st})"))
    }
}

fn build(profile: &str, app: App, msi: bool) -> Result<(), String> {
    if std::env::var_os("SR_DYNAMIC_CRT").is_some() {
        return Err("SR_DYNAMIC_CRT is set (local diagnostics only); unset it to package".into());
    }
    let s = |x: &str| x.to_string();
    // 1. Static-CRT exes, one invocation, no sr-daemon (feature unification, W2-H R-a).
    let mut a = Vec::new();
    for (bin, pkg) in STATIC_EXES {
        a.extend([s("-p"), s(pkg), s("--bin"), s(bin)]);
    }
    if app == App::Tauri {
        // What `tauri build` sets: embed ui/dist instead of loading the dev server.
        a.extend([s("--features"), s("sr-app/custom-protocol")]);
    }
    cargo(profile, &a)?;
    // 2. The daemon on its own.
    cargo(profile, &[s("-p"), s(DAEMON.1), s("--bin"), s(DAEMON.0)])?;
    // 3. The installer helper (own workspace; same target dir so all outputs sit together), and
    //    for the MSI its custom-action DLL (also its own workspace).
    let nested = |name: &str| -> Result<(), String> {
        let manifest = fetch::workspace_root()
            .join("installer")
            .join(name)
            .join("Cargo.toml");
        cargo(
            profile,
            &[
                s("--manifest-path"),
                manifest.display().to_string(),
                s("--target-dir"),
                target_dir().display().to_string(),
            ],
        )
    };
    nested("sr-setup")?;
    if msi {
        nested("msi-ca")?;
    }
    Ok(())
}

/// The shipped exes run through the binary-import guards (`--no-build --exe ...`).
fn guards(out: &Path) -> Result<(), String> {
    let mut a = vec!["--no-build".to_string()];
    for (bin, _) in STATIC_EXES {
        a.extend(["--exe".into(), exe(out, bin).display().to_string()]);
    }
    a.extend(["--exe".into(), exe(out, SETUP).display().to_string()]);
    crate::crt::check_crt(&a)?;
    crate::crt::check_delayload(&[
        "--no-build".into(),
        "--exe".into(),
        exe(out, DAEMON.0).display().to_string(),
    ])?;
    // M5-I: every shipped exe embeds the application manifest (installer/manifests/).
    let shipped: Vec<(PathBuf, bool)> = crate::manifest::SHIPPED
        .iter()
        .map(|(n, comctl)| (exe(out, n), *comctl))
        .collect();
    crate::manifest::check_all(&shipped)
}

/// One staged file: path relative to the install dir, and where it comes from.
struct Staged {
    rel: PathBuf,
    src: PathBuf,
}

fn plan(out: &Path, vendor: &Path) -> Vec<Staged> {
    let mut v = Vec::new();
    let mut add = |rel: PathBuf, src: PathBuf| v.push(Staged { rel, src });
    for (bin, _) in STATIC_EXES {
        add(format!("{bin}.exe").into(), exe(out, bin));
    }
    add(format!("{}.exe", DAEMON.0).into(), exe(out, DAEMON.0));
    add(format!("{SETUP}.exe").into(), exe(out, SETUP));
    let take = |a: &Archive, add: &mut dyn FnMut(PathBuf, PathBuf)| {
        for (_, name, _) in a.files {
            let rel = if name.ends_with(".dll") {
                PathBuf::from(name)
            } else {
                Path::new("licenses").join(name)
            };
            add(rel, vendor.join(a.dir).join(name));
        }
    };
    take(&fetch::VC_RUNTIME, &mut add);
    take(&fetch::DIRECTML, &mut add);
    take(&fetch::PDFIUM, &mut add);
    let models = vendor.join("models").join(MODEL_DIR);
    for f in model_files() {
        add(
            Path::new("models").join(MODEL_DIR).join(&f),
            models.join(&f),
        );
    }
    add(
        Path::new("licenses").join("LICENSE-selene-retrieval.txt"),
        fetch::workspace_root().join("LICENSE"),
    );
    v
}

/// The bundled model's five pinned files (same list as `fetch-models`).
pub(crate) fn model_files() -> Vec<String> {
    use sr_common::tier::{MINIMUM, Tier};
    let f = MINIMUM.gpu_file;
    let mut v = vec![f.path.rsplit('/').next().unwrap_or(f.path).to_string()];
    let t = sr_providers::models::tokenizer_pins(Tier::Minimum);
    v.extend(t.files().into_iter().map(|a| a.name.to_string()));
    v
}

fn stage(files: &[Staged], dir: &Path) -> Result<u64, String> {
    let _ = std::fs::remove_dir_all(dir);
    let mut total = 0;
    for f in files {
        let dst = dir.join(&f.rel);
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        total += std::fs::copy(&f.src, &dst)
            .map_err(|e| format!("stage {} from {}: {e}", f.rel.display(), f.src.display()))?;
    }
    Ok(total)
}

/// `!macro SR_INSTALL_FILES` / `SR_UNINSTALL_FILES` for the standalone script: an explicit list,
/// so the uninstaller deletes exactly what was installed (never `RMDir /r $INSTDIR`).
fn files_nsh(files: &[Staged], stage_dir: &Path) -> String {
    let mut dirs: Vec<PathBuf> = files
        .iter()
        .map(|f| f.rel.parent().map(Path::to_path_buf).unwrap_or_default())
        .collect();
    dirs.sort();
    dirs.dedup();
    let win = |p: &Path| p.display().to_string().replace('/', "\\");
    let mut s = String::from(
        "; Generated by `cargo xtask package`. Do not edit.\n!macro SR_INSTALL_FILES\n",
    );
    for d in &dirs {
        let out = if d.as_os_str().is_empty() {
            "$INSTDIR".to_string()
        } else {
            format!("$INSTDIR\\{}", win(d))
        };
        s.push_str(&format!("  SetOutPath \"{out}\"\n"));
        for f in files
            .iter()
            .filter(|f| f.rel.parent().unwrap_or(Path::new("")) == d)
        {
            s.push_str(&format!("  File \"{}\"\n", win(&stage_dir.join(&f.rel))));
        }
    }
    s.push_str("  SetOutPath \"$INSTDIR\"\n!macroend\n\n!macro SR_UNINSTALL_FILES\n");
    for f in files {
        s.push_str(&format!("  Delete \"$INSTDIR\\{}\"\n", win(&f.rel)));
    }
    // Deepest first.
    let mut rm: Vec<&PathBuf> = dirs.iter().filter(|d| !d.as_os_str().is_empty()).collect();
    rm.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in rm {
        s.push_str(&format!("  RMDir \"$INSTDIR\\{}\"\n", win(d)));
        let mut p = d.parent();
        while let Some(x) = p.filter(|x| !x.as_os_str().is_empty()) {
            s.push_str(&format!("  RMDir \"$INSTDIR\\{}\"\n", win(x)));
            p = x.parent();
        }
    }
    s.push_str("!macroend\n");
    s
}

fn makensis(vendor: &Path, defines: &[(String, String)], script: &Path) -> Result<(), String> {
    let tool = vendor.join(fetch::NSIS.dir).join("makensis.exe");
    let mut cmd = Command::new(&tool);
    cmd.args(["/V3", "/WX", "/INPUTCHARSET", "UTF8"]);
    for (k, v) in defines {
        cmd.arg(if v.is_empty() {
            format!("/D{k}")
        } else {
            format!("/D{k}={v}")
        });
    }
    cmd.arg(script);
    println!("package: running {cmd:?}");
    let st = cmd
        .status()
        .map_err(|e| format!("could not run {}: {e}", tool.display()))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("makensis failed ({st})"))
    }
}

fn sha_line(path: &Path, name: &str) -> Result<String, String> {
    let b = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{}  {name}", sr_providers::models::sha256_hex(&b)))
}

pub fn main(args: &[String]) -> Result<(), String> {
    let o = parse(args)?;
    let version = env!("CARGO_PKG_VERSION");

    // 1. Pinned inputs (the installer tool: NSIS, or WiX + its UI extension for --msi).
    for a in [&fetch::PDFIUM, &fetch::DIRECTML, &fetch::VC_RUNTIME] {
        fetch::fetch(a, &o.vendor)?;
    }
    if o.msi {
        fetch::fetch(&fetch::WIX, &o.vendor)?;
        fetch::fetch(&fetch::WIX_UI, &o.vendor)?;
    } else {
        fetch::fetch(&fetch::NSIS, &o.vendor)?;
    }
    let webview2 = if o.webview2_offline {
        fetch::fetch_webview2_offline(&o.vendor)?
    } else {
        fetch::fetch_webview2(&o.vendor)?
    };
    let models = o.vendor.join("models").join(MODEL_DIR);
    crate::models::fetch_models(&["--dir".into(), models.display().to_string()])?;

    // 2. Build, 3. guards.
    let root = fetch::workspace_root();
    let app = App::detect(&root);
    if o.build {
        build(&o.profile, app, o.msi)?;
    }
    let out = profile_dir(&o.profile);
    guards(&out)?;

    // 4. Stage, sign, installer.
    let pkg = target_dir().join("package");
    let stage_dir = pkg.join("stage");
    let files = plan(&out, &o.vendor);
    let staged_bytes = stage(&files, &stage_dir)?;
    let ours: Vec<PathBuf> = files
        .iter()
        .filter(|f| f.rel.extension().is_some_and(|e| e == "exe"))
        .map(|f| stage_dir.join(&f.rel))
        .collect();
    let exe_signing = sign::sign_files(&ours)?;
    let signed = exe_signing != sign::Outcome::Unsigned;
    if o.stage_only {
        println!(
            "package: staged {} files ({:.1} MiB) in {} ({})",
            files.len(),
            staged_bytes as f64 / (1024.0 * 1024.0),
            stage_dir.display(),
            if signed { "signed" } else { "UNSIGNED" }
        );
        return Ok(());
    }
    let variant = if o.webview2_offline { "-offline" } else { "" };
    let installer = if o.msi {
        pkg.join(format!("SeleneRetrieval-{version}-x64{variant}.msi"))
    } else {
        pkg.join(format!("SeleneRetrieval-{version}-x64{variant}-setup.exe"))
    };
    let _ = std::fs::remove_file(&installer);
    if o.msi {
        build_msi(
            &o, &root, &out, &pkg, &files, &stage_dir, &webview2, &installer,
        )?;
    } else {
        build_nsis(
            &o, &root, app, signed, &pkg, &files, &stage_dir, &webview2, &installer,
        )?;
    }
    // The finished installer goes through the same signing hook (NSIS signs inside makensis via
    // !finalize; signing an already-signed exe again is skipped here).
    let installer_signing = if o.msi || !signed {
        sign::sign_files(std::slice::from_ref(&installer))?
    } else {
        exe_signing.clone()
    };
    if (installer_signing != sign::Outcome::Unsigned) != signed {
        return Err("signing state changed between the exes and the installer".into());
    }

    // 5. Report.
    // One report per package kind and variant, so building all four keeps every report.
    let suffix = format!("{}{variant}", if o.msi { "-msi" } else { "" });
    let installer_bytes = std::fs::metadata(&installer)
        .map_err(|e| format!("{}: {e}", installer.display()))?
        .len();
    let mut sums = vec![sha_line(&installer, &file_name(&installer))?];
    let mut per_file = serde_json::Map::new();
    for f in &files {
        let p = stage_dir.join(&f.rel);
        let n = f.rel.display().to_string().replace('/', "\\");
        sums.push(sha_line(&p, &n)?);
        let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        per_file.insert(n, len.into());
    }
    let sums_name = format!("SHA256SUMS{suffix}.txt");
    std::fs::write(pkg.join(&sums_name), sums.join("\n") + "\n")
        .map_err(|e| format!("{sums_name}: {e}"))?;
    let mb = installer_bytes as f64 / 1e6;
    let mib = staged_bytes as f64 / (1024.0 * 1024.0);
    let report = serde_json::json!({
        "version": version,
        "profile": o.profile,
        "kind": if o.msi { "msi (WiX 5.0.2, per-user)" } else { "nsis (per-user)" },
        "installer": file_name(&installer),
        "installer_bytes": installer_bytes,
        "installer_mb": (mb * 10.0).round() / 10.0,
        "staged_bytes": staged_bytes,
        "staged_mib": (mib * 10.0).round() / 10.0,
        "r25": { "download_mb": [R25_DOWNLOAD_MB.0, R25_DOWNLOAD_MB.1], "installed_mib": R25_INSTALLED_MIB },
        "signed": signed,
        "signing": match &exe_signing { sign::Outcome::Signed(m) => *m, sign::Outcome::Unsigned => "none" },
        "app": app.label(),
        "manifests": "check-manifest OK (asInvoker, supportedOS Win10/11, longPathAware, PerMonitorV2)",
        "pins": {
            "pdfium": fetch::PDFIUM.url, "directml": fetch::DIRECTML.url,
            "vc_runtime": fetch::VC_RUNTIME.url,
            "installer_tool": if o.msi { fetch::WIX.url } else { fetch::NSIS.url },
        },
        "webview2": {
            "variant": if o.webview2_offline { "offline (standalone runtime)" } else { "online bootstrapper" },
            "url": if o.webview2_offline { fetch::WEBVIEW2_OFFLINE_URL } else { fetch::WEBVIEW2_URL },
            "sha256": webview2.sha256, "version": webview2.version,
            "check": if o.webview2_offline {
                "pinned sha256 + Authenticode: Microsoft Corporation"
            } else {
                "Authenticode: Microsoft Corporation (not content-pinned: Microsoft updates it in place)"
            },
            "pinned_bytes": if o.webview2_offline { Some(fetch::WEBVIEW2_OFFLINE_BYTES) } else { None },
        },
        "files": per_file,
    });
    let body = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    let report_name = format!("package-report{suffix}.json");
    std::fs::write(pkg.join(&report_name), body + "\n")
        .map_err(|e| format!("{report_name}: {e}"))?;

    println!("package: {}", installer.display());
    println!(
        "package: installer {mb:.1} MB (R25 {}-{} MB); installed {mib:.1} MiB (R25 {R25_INSTALLED_MIB} MiB)",
        R25_DOWNLOAD_MB.0, R25_DOWNLOAD_MB.1
    );
    if signed {
        println!("package: exes and installer SIGNED and verified");
    } else {
        println!(
            "package: UNSIGNED build (no signing credentials configured). SmartScreen will warn; \
             do not publish this installer."
        );
    }
    match app {
        App::Tauri => println!("package: sr-app built with the embedded UI (custom-protocol)"),
        App::TauriNoUi => println!("package: WARNING ui/dist missing: sr-app has no embedded UI"),
        App::Stub => println!("package: sr-app is the console stub (no tauri.conf.json)"),
    }
    Ok(())
}

/// makensis on `installer/nsis/selene-retrieval.nsi` (M2-I), then the manifest check on the setup.
#[allow(clippy::too_many_arguments)]
fn build_nsis(
    o: &Opts,
    root: &Path,
    app: App,
    signed: bool,
    pkg: &Path,
    files: &[Staged],
    stage_dir: &Path,
    webview2: &fetch::Webview2,
    installer: &Path,
) -> Result<(), String> {
    let nsh = pkg.join("files.nsh");
    std::fs::write(&nsh, files_nsh(files, stage_dir))
        .map_err(|e| format!("{}: {e}", nsh.display()))?;
    let mut defines: Vec<(String, String)> = vec![
        ("SR_VERSION".into(), env!("CARGO_PKG_VERSION").into()),
        ("SR_STAGE".into(), stage_dir.display().to_string()),
        ("SR_FILES".into(), nsh.display().to_string()),
        ("SR_OUTFILE".into(), installer.display().to_string()),
        (
            "SR_HOOKS".into(),
            root.join("crates")
                .join("app")
                .join("nsis")
                .join("hooks.nsh")
                .display()
                .to_string(),
        ),
    ];
    if app != App::Tauri {
        defines.push(("SR_STUB_APP".into(), String::new()));
    }
    if let Some(id) = bundle_id(root) {
        defines.push(("BUNDLEID".into(), id));
    }
    // Bundled, never installed as a file: the installer runs it only when WebView2 is missing.
    // Online bootstrapper by default; the standalone runtime only in the offline variant.
    let wv2_define = if o.webview2_offline {
        "SR_WEBVIEW2_OFFLINE"
    } else {
        "SR_WEBVIEW2"
    };
    defines.push((wv2_define.into(), webview2.path.display().to_string()));
    if signed {
        let me = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
        defines.push(("SR_SIGN_CMD".into(), format!("\"{}\" sign", me.display())));
    }
    makensis(
        &o.vendor,
        &defines,
        &root
            .join("installer")
            .join("nsis")
            .join("selene-retrieval.nsi"),
    )?;
    crate::manifest::check_all(&[(installer.to_path_buf(), false)])
}

/// The per-user MSI (M5-I): `installer/wix/selene-retrieval.wxs` + the generated file list +
/// `en-us.wxl`, WiX 5.0.2 with the UI extension, then ICE validation.
#[allow(clippy::too_many_arguments)]
fn build_msi(
    o: &Opts,
    root: &Path,
    out: &Path,
    pkg: &Path,
    files: &[Staged],
    stage_dir: &Path,
    webview2: &fetch::Webview2,
    installer: &Path,
) -> Result<(), String> {
    let wix_dir = root.join("installer").join("wix");
    // The custom-action DLL (Binary table, never installed), signed like our exes.
    let ca_src = out.join(format!("{MSI_CA}.dll"));
    let ca = pkg.join(format!("{MSI_CA}.dll"));
    std::fs::copy(&ca_src, &ca).map_err(|e| format!("{}: {e}", ca_src.display()))?;
    crate::crt::check_crt(&[
        "--no-build".into(),
        "--exe".into(),
        ca.display().to_string(),
    ])?;
    sign::sign_files(std::slice::from_ref(&ca))?;
    let files_wxs = pkg.join("files.wxs");
    std::fs::write(&files_wxs, files_wxs_text(files, stage_dir)?)
        .map_err(|e| format!("{}: {e}", files_wxs.display()))?;
    let app_id = bundle_id(root).unwrap_or_else(|| "ai.selene.retrieval".into());
    let mut defines = vec![
        ("SrVersion", env!("CARGO_PKG_VERSION").to_string()),
        ("SrAppId", app_id),
        (
            "SrIcon",
            root.join("crates")
                .join("app")
                .join("icons")
                .join("icon.ico")
                .display()
                .to_string(),
        ),
        ("SrCa", ca.display().to_string()),
        ("SrWebView2", webview2.path.display().to_string()),
    ];
    // The offline package bundles the standalone runtime: its Finish-page text for a failed
    // WebView2 setup must not send the user to the internet (sr-msi-ca `wv2_missing_text`).
    if o.webview2_offline {
        defines.push(("SrWebView2Offline", "1".to_string()));
    }
    let tool = o.vendor.join(fetch::WIX.dir).join("wix.exe");
    let ui_ext = o
        .vendor
        .join(fetch::WIX_UI.dir)
        .join("WixToolset.UI.wixext.dll");
    let mut cmd = Command::new(&tool);
    cmd.args(["build", "-nologo", "-arch", "x64", "-culture", "en-US"])
        .arg("-ext")
        .arg(&ui_ext)
        .arg("-loc")
        .arg(wix_dir.join("en-us.wxl"))
        .arg("-intermediatefolder")
        .arg(pkg.join("wixobj"));
    for (k, v) in &defines {
        cmd.arg("-d").arg(format!("{k}={v}"));
    }
    cmd.arg("-o")
        .arg(installer)
        .arg(wix_dir.join("selene-retrieval.wxs"))
        .arg(&files_wxs);
    run_wix(cmd, &tool)?;
    // ICE validation. Suppressed, by design: ICE61 (same-version upgrades replace a rebuild),
    // ICE69 (Start menu shortcut targets sr-app.exe in another component of the same feature),
    // ICE91 (per-user files: this package is per-user only). Any other warning fails.
    let mut v = Command::new(&tool);
    v.args(["msi", "validate", "-nologo", "-wx"])
        .args(["-sice", "ICE61", "-sice", "ICE69", "-sice", "ICE91"])
        .arg(installer);
    run_wix(v, &tool)
}

fn run_wix(mut cmd: Command, tool: &Path) -> Result<(), String> {
    // No .NET first-run banner or telemetry from the build tool.
    cmd.env("DOTNET_NOLOGO", "1")
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1");
    println!("package: running {cmd:?}");
    let st = cmd.status().map_err(|e| {
        format!(
            "could not run {} ({e}); WiX needs a .NET runtime 6 or later",
            tool.display()
        )
    })?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("wix failed ({st})"))
    }
}

/// WiX identifier for a staged path: `<prefix>_` + letters/digits, `_` for everything else
/// (max 72 characters for MSI keys; longer names get a hash suffix).
fn wix_id(prefix: &str, rel: &Path) -> String {
    let flat: String = rel
        .display()
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let id = format!("{prefix}_{flat}");
    if id.len() <= 70 {
        id
    } else {
        let h = sr_providers::models::sha256_hex(id.as_bytes());
        format!("{}_{}", &id[..58], &h[..10])
    }
}

/// Stable component GUID per install folder (the same folder keeps its GUID in every build).
fn component_guid(dir_key: &str) -> String {
    let h = sr_providers::models::sha256_hex(
        format!("selene-retrieval/msi/component/{dir_key}").as_bytes(),
    );
    // Name-based layout: version nibble 5, RFC 4122 variant.
    let v = |i: usize| &h[i..i + 1];
    let variant = match u8::from_str_radix(v(16), 16).unwrap_or(0) & 0x3 {
        0 => "8",
        1 => "9",
        2 => "a",
        _ => "b",
    };
    format!(
        "{}-{}-5{}-{}{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        variant,
        &h[17..20],
        &h[20..32]
    )
    .to_ascii_uppercase()
}

/// The generated `files.wxs`: every staged folder as a `Directory` and one component per folder
/// (an HKCU registry value as key path and a `RemoveFolder`, which per-user packages need), with
/// its files listed explicitly, so uninstall removes exactly what was installed.
fn files_wxs_text(files: &[Staged], stage_dir: &Path) -> Result<String, String> {
    use std::collections::BTreeSet;
    let win = |p: &Path| p.display().to_string().replace('/', "\\");
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    // Every folder, including file-less parents such as `models`.
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for f in files {
        let mut p = f.rel.parent();
        while let Some(d) = p.filter(|d| !d.as_os_str().is_empty()) {
            dirs.insert(d.to_path_buf());
            p = d.parent();
        }
    }
    let dir_id = |d: &Path| {
        if d.as_os_str().is_empty() {
            "INSTALLFOLDER".to_string()
        } else {
            wix_id("d", d)
        }
    };
    let mut ids = BTreeSet::new();
    for f in files {
        if !ids.insert(wix_id("f", &f.rel)) {
            return Err(format!(
                "two staged files map to one WiX id: {}",
                f.rel.display()
            ));
        }
    }
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!-- Generated by xtask package (MSI). Do not edit. -->\n\
         <Wix xmlns=\"http://wixtoolset.org/schemas/v4/wxs\">\n  <Fragment>\n    <DirectoryRef Id=\"INSTALLFOLDER\">\n",
    );
    fn tree(
        s: &mut String,
        dirs: &BTreeSet<PathBuf>,
        parent: &Path,
        depth: usize,
        dir_id: &dyn Fn(&Path) -> String,
        esc: &dyn Fn(&str) -> String,
    ) {
        for d in dirs.iter().filter(|d| d.parent() == Some(parent)) {
            let pad = "  ".repeat(depth + 3);
            let name = d
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let kids = dirs.iter().any(|x| x.parent() == Some(d.as_path()));
            if kids {
                s.push_str(&format!(
                    "{pad}<Directory Id=\"{}\" Name=\"{}\">\n",
                    dir_id(d),
                    esc(&name)
                ));
                tree(s, dirs, d, depth + 1, dir_id, esc);
                s.push_str(&format!("{pad}</Directory>\n"));
            } else {
                s.push_str(&format!(
                    "{pad}<Directory Id=\"{}\" Name=\"{}\" />\n",
                    dir_id(d),
                    esc(&name)
                ));
            }
        }
    }
    tree(&mut s, &dirs, Path::new(""), 0, &dir_id, &esc);
    s.push_str("    </DirectoryRef>\n    <ComponentGroup Id=\"SrFiles\">\n");
    let all: Vec<PathBuf> = std::iter::once(PathBuf::new())
        .chain(dirs.iter().cloned())
        .collect();
    for d in &all {
        let key = if d.as_os_str().is_empty() {
            "root".to_string()
        } else {
            win(d)
        };
        let cid = if d.as_os_str().is_empty() {
            "c_root".to_string()
        } else {
            wix_id("c", d)
        };
        s.push_str(&format!(
            "      <Component Id=\"{cid}\" Directory=\"{}\" Guid=\"{}\">\n        <RegistryValue Root=\"HKCU\" Key=\"Software\\Selene\\Selene Retrieval\\Components\" Name=\"{}\" Type=\"integer\" Value=\"1\" KeyPath=\"yes\" />\n",
            dir_id(d),
            component_guid(&key),
            esc(&key)
        ));
        if !d.as_os_str().is_empty() {
            s.push_str(&format!(
                "        <RemoveFolder Id=\"{}\" On=\"uninstall\" />\n",
                wix_id("r", d)
            ));
        }
        for f in files
            .iter()
            .filter(|f| f.rel.parent().unwrap_or(Path::new("")) == d.as_path())
        {
            s.push_str(&format!(
                "        <File Id=\"{}\" Source=\"{}\" />\n",
                wix_id("f", &f.rel),
                esc(&win(&stage_dir.join(&f.rel)))
            ));
        }
        s.push_str("      </Component>\n");
    }
    s.push_str("    </ComponentGroup>\n  </Fragment>\n</Wix>\n");
    Ok(s)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opts_and_profiles() {
        let a = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let o = parse(&a(&[])).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(o.profile, "dist");
        let o = parse(&a(&["--opt-size", "--no-build"])).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(o.profile, "dist-size");
        assert!(!o.build);
        assert!(parse(&a(&["--profile", "ci", "--opt-size"])).is_err());
        assert!(
            !parse(&a(&[]))
                .unwrap_or_else(|e| panic!("{e}"))
                .webview2_offline
        );
        assert!(
            parse(&a(&["--webview2-offline"]))
                .unwrap_or_else(|e| panic!("{e}"))
                .webview2_offline
        );
        let f = profile_flags("dist");
        assert!(
            f.contains(&"profile.dist.lto=\"thin\"".to_string()),
            "{f:?}"
        );
        assert!(f.contains(&"profile.dist.inherits=\"release\"".to_string()));
        assert!(
            profile_flags("dist-size").contains(&"profile.dist-size.opt-level=\"s\"".to_string())
        );
        assert!(
            profile_flags("release").is_empty(),
            "release is defined in Cargo.toml"
        );
    }

    #[test]
    fn layout_has_every_runtime_file_once() {
        let files = plan(Path::new(r"C:\t\dist"), Path::new(r"C:\v"));
        let rels: Vec<String> = files
            .iter()
            .map(|f| f.rel.display().to_string().replace('/', "\\"))
            .collect();
        for want in [
            "sr-app.exe",
            "sr-bridge.exe",
            "sr-launch.exe",
            "sr-extract.exe",
            "sr-daemon.exe",
            "sr-setup.exe",
            "pdfium.dll",
            "DirectML.dll",
            "vcruntime140.dll",
            "vcruntime140_1.dll",
            "msvcp140.dll",
            "msvcp140_1.dll",
            r"models\bge-small-en-v1.5\model_optimized.onnx",
        ] {
            assert_eq!(
                rels.iter().filter(|r| *r == want).count(),
                1,
                "{want} in {rels:?}"
            );
        }
        let mut sorted = rels.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), rels.len(), "duplicate staged path");
        // Models: the five pinned files (fetch-models).
        assert_eq!(rels.iter().filter(|r| r.starts_with("models\\")).count(), 5);
    }

    #[test]
    fn msi_option_and_names() {
        let a = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert!(!parse(&a(&[])).unwrap_or_else(|e| panic!("{e}")).msi);
        let o = parse(&a(&["--msi", "--webview2-offline"])).unwrap_or_else(|e| panic!("{e}"));
        assert!(o.msi && o.webview2_offline);
        assert_eq!(wix_id("f", Path::new("sr-setup.exe")), "f_sr_setup_exe");
        assert_eq!(wix_id("f", Path::new("sr-app.exe")), "f_sr_app_exe");
        let long = wix_id("f", Path::new(&"x".repeat(200)));
        assert!(long.len() <= 70, "{long}");
        let g = component_guid("root");
        assert_eq!(g, component_guid("root"), "stable across builds");
        assert_ne!(g, component_guid("licenses"));
        assert_eq!(g.len(), 36);
        assert_eq!(&g[14..15], "5");
        assert!(matches!(&g[19..20], "8" | "9" | "A" | "B"), "{g}");
    }

    #[test]
    fn wix_list_is_explicit_with_a_component_per_folder() {
        let files = plan(Path::new(r"C:\t\dist"), Path::new(r"C:\v"));
        let s = files_wxs_text(&files, Path::new(r"C:\stage")).unwrap_or_else(|e| panic!("{e}"));
        // The custom actions in selene-retrieval.wxs refer to these two file ids.
        assert!(
            s.contains(r#"<File Id="f_sr_setup_exe" Source="C:\stage\sr-setup.exe" />"#),
            "{s}"
        );
        assert!(s.contains(r#"<File Id="f_sr_app_exe""#));
        for d in [
            "c_root",
            "c_licenses",
            "c_models",
            "c_models_bge_small_en_v1_5",
        ] {
            assert_eq!(
                s.matches(&format!("Component Id=\"{d}\"")).count(),
                1,
                "{d}: {s}"
            );
        }
        // models has no files of its own but still needs a RemoveFolder (per-user, ICE64).
        assert!(s.contains(r#"<RemoveFolder Id="r_models" On="uninstall" />"#));
        assert!(s.contains(r#"<Directory Id="d_models" Name="models">"#));
        assert!(
            s.contains(r#"<Directory Id="d_models_bge_small_en_v1_5" Name="bge-small-en-v1.5" />"#)
        );
        assert_eq!(s.matches("<File ").count(), files.len());
        assert_eq!(s.matches("KeyPath=\"yes\"").count(), 4);
    }

    #[test]
    fn nsis_list_is_explicit_and_removes_dirs_deepest_first() {
        let files = plan(Path::new(r"C:\t\dist"), Path::new(r"C:\v"));
        let s = files_nsh(&files, Path::new(r"C:\stage"));
        assert!(s.contains(r#"File "C:\stage\sr-daemon.exe""#), "{s}");
        assert!(s.contains(r#"Delete "$INSTDIR\models\bge-small-en-v1.5\model_optimized.onnx""#));
        assert!(!s.contains("RMDir /r"), "never recursive");
        let deep = s.find(r#"RMDir "$INSTDIR\models\bge-small-en-v1.5""#);
        let top = s.find(r#"RMDir "$INSTDIR\models""#);
        assert!(deep.is_some() && top.is_some() && deep < top, "{s}");
        assert_eq!(s.matches("!macroend").count(), 2);
    }
}
