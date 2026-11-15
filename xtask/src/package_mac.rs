//! `cargo xtask package-mac`: the macOS (Apple silicon) app bundle and disk image (Amendment 18,
//! phase 4).
//!
//! ```text
//! cargo xtask package-mac [--profile <p>] [--no-build] [--no-dmg] [--vendor <dir>]
//! ```
//! 1. Pinned inputs: pdfium for macOS ([`fetch::PDFIUM_MAC_ARM64`]) and the bundled Minimum model,
//!    sha256-checked like the Windows package.
//! 2. Build with the same `dist` profile and the same split as `package` (the static exes plus
//!    sr-app with its embedded UI in one invocation, sr-daemon on its own).
//! 3. Assemble `Selene Retrieval.app`:
//!    ```text
//!    Contents/Info.plist
//!    Contents/MacOS/      sr-app (main) sr-daemon sr-bridge sr-launch sr-extract
//!    Contents/Frameworks/ libpdfium.dylib            (opened by sr-extract by absolute path)
//!    Contents/Resources/  icon.icns  models/bge-small-en-v1.5/…  licenses/…
//!    ```
//!    Code lives only in `MacOS` and `Frameworks`, data only in `Resources`, so the signature
//!    seals every file (`sr_common::install` finds the parts at run time).
//! 4. Sign: releases are not code-signed yet (R-UX54), so every Mach-O is signed ad hoc
//!    (`codesign --sign -`), inside out, then the bundle; `codesign --verify --strict --deep`
//!    must pass. With `SR_MAC_SIGN_IDENTITY` set (a Developer ID), that identity and the
//!    hardened runtime are used instead (notarization is a later step).
//! 5. Guard: `otool -L` on every Mach-O; only system libraries (`/usr/lib`, `/System`) may be
//!    linked (nothing from Homebrew or a build machine path).
//! 6. `SeleneRetrieval-<v>-arm64.dmg` (UDZO, with an Applications link to drag onto) and
//!    `SHA256SUMS-mac.txt` in `target/package/mac`.

use crate::fetch;
use crate::package::{cargo, target_dir};
use std::path::{Path, PathBuf};
use std::process::Command;

const APP_NAME: &str = "Selene Retrieval";
const MAIN_EXE: &str = "sr-app";
/// Helper programs next to the main one, with their package (built together with sr-app).
const HELPERS: &[(&str, &str)] = &[
    ("sr-bridge", "sr-bridge"),
    ("sr-launch", "sr-bridge"),
    ("sr-extract", "sr-extract"),
];
const DAEMON: (&str, &str) = ("sr-daemon", "sr-mcp-server");
const MODEL_DIR: &str = "bge-small-en-v1.5";
const MIN_MACOS: &str = "13.0";

struct Opts {
    profile: String,
    build: bool,
    dmg: bool,
    vendor: PathBuf,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        profile: "dist".into(),
        build: true,
        dmg: true,
        vendor: fetch::default_vendor_dir(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--profile" => o.profile = it.next().ok_or("--profile needs a value")?.clone(),
            "--no-build" => o.build = false,
            "--no-dmg" => o.dmg = false,
            "--vendor" => o.vendor = it.next().ok_or("--vendor needs a path")?.into(),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(o)
}

/// A system tool by absolute path (never `PATH`).
fn tool(path: &str) -> Command {
    Command::new(path)
}

fn run(mut cmd: Command, what: &str) -> Result<(), String> {
    let st = cmd
        .status()
        .map_err(|e| format!("{what}: could not run: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("{what} failed ({st})"))
    }
}

fn output(mut cmd: Command, what: &str) -> Result<String, String> {
    let o = cmd
        .output()
        .map_err(|e| format!("{what}: could not run: {e}"))?;
    if !o.status.success() {
        return Err(format!(
            "{what} failed ({}): {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn copy(src: &Path, dst: &Path) -> Result<u64, String> {
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    std::fs::copy(src, dst).map_err(|e| format!("copy {} → {}: {e}", src.display(), dst.display()))
}

/// The bundle's Info.plist. Values are plain ASCII (checked), so no XML escaping is needed.
fn info_plist(version: &str, identifier: &str) -> String {
    let keys: &[(&str, &str)] = &[
        ("CFBundleDevelopmentRegion", "en"),
        ("CFBundleDisplayName", APP_NAME),
        ("CFBundleExecutable", MAIN_EXE),
        ("CFBundleIconFile", "icon.icns"),
        ("CFBundleIdentifier", identifier),
        ("CFBundleInfoDictionaryVersion", "6.0"),
        ("CFBundleName", APP_NAME),
        ("CFBundlePackageType", "APPL"),
        ("CFBundleShortVersionString", version),
        ("CFBundleVersion", version),
        (
            "LSApplicationCategoryType",
            "public.app-category.productivity",
        ),
        ("LSMinimumSystemVersion", MIN_MACOS),
    ];
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    for (k, v) in keys {
        s.push_str(&format!("  <key>{k}</key>\n  <string>{v}</string>\n"));
    }
    s.push_str("  <key>NSHighResolutionCapable</key>\n  <true/>\n");
    s.push_str("  <key>NSSupportsAutomaticGraphicsSwitching</key>\n  <true/>\n");
    s.push_str("</dict>\n</plist>\n");
    s
}

fn plain(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ' '))
}

/// `identifier` from tauri.conf.json.
fn bundle_id(root: &Path) -> Result<String, String> {
    let conf = root.join("crates").join("app").join("tauri.conf.json");
    let text = std::fs::read_to_string(&conf).map_err(|e| format!("{}: {e}", conf.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", conf.display()))?;
    let id = v
        .get("identifier")
        .and_then(|i| i.as_str())
        .ok_or("tauri.conf.json has no identifier")?;
    if plain(id) && !id.contains(' ') {
        Ok(id.to_string())
    } else {
        Err(format!("unexpected bundle identifier {id:?}"))
    }
}

/// `icon.icns` from the app's 1024 px PNG (sips + iconutil, both part of macOS).
fn make_icns(png: &Path, out: &Path, work: &Path) -> Result<(), String> {
    let set = work.join("icon.iconset");
    let _ = std::fs::remove_dir_all(&set);
    std::fs::create_dir_all(&set).map_err(|e| format!("{}: {e}", set.display()))?;
    for size in [16u32, 32, 128, 256, 512] {
        for (scale, suffix) in [(1u32, ""), (2, "@2x")] {
            let px = size * scale;
            let dst = set.join(format!("icon_{size}x{size}{suffix}.png"));
            let mut c = tool("/usr/bin/sips");
            c.args(["-z", &px.to_string(), &px.to_string()])
                .arg(png)
                .arg("--out")
                .arg(&dst)
                .stdout(std::process::Stdio::null());
            run(c, "sips")?;
        }
    }
    let mut c = tool("/usr/bin/iconutil");
    c.args(["-c", "icns"]).arg(&set).arg("-o").arg(out);
    run(c, "iconutil")
}

fn sign(path: &Path, identifier: Option<&str>) -> Result<(), String> {
    let identity = std::env::var("SR_MAC_SIGN_IDENTITY").ok();
    let mut c = tool("/usr/bin/codesign");
    c.args(["--force", "--timestamp=none"]);
    match &identity {
        Some(id) => {
            c.args(["--options", "runtime", "--sign", id]);
        }
        None => {
            c.args(["--sign", "-"]);
        }
    }
    if let Some(i) = identifier {
        c.args(["--identifier", i]);
    }
    c.arg(path);
    run(c, &format!("codesign {}", path.display()))
}

/// Libraries a Mach-O links (`otool -L`), without its own install name.
fn linked_libraries(bin: &Path) -> Result<Vec<String>, String> {
    let mut c = tool("/usr/bin/otool");
    c.arg("-L").arg(bin);
    let text = output(c, "otool -L")?;
    Ok(text
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect())
}

/// Only system libraries may be linked; a dylib's own install name (first entry) is skipped.
fn check_links(bin: &Path) -> Result<(), String> {
    let is_dylib = bin.extension().is_some_and(|e| e == "dylib");
    let libs = linked_libraries(bin)?;
    let bad: Vec<&String> = libs
        .iter()
        .skip(usize::from(is_dylib))
        .filter(|l| !(l.starts_with("/usr/lib/") || l.starts_with("/System/Library/")))
        .collect();
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} links non-system libraries: {}",
            bin.display(),
            bad.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

fn sha_line(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    Ok(format!(
        "{}  {name}",
        sr_providers::models::sha256_hex(&bytes)
    ))
}

fn build(profile: &str) -> Result<(), String> {
    let s = |x: &str| x.to_string();
    let mut a = Vec::new();
    for (bin, pkg) in HELPERS.iter().chain([&(MAIN_EXE, MAIN_EXE)]) {
        a.extend([s("-p"), s(pkg), s("--bin"), s(bin)]);
    }
    // What `tauri build` sets: embed ui/dist instead of loading the dev server.
    a.extend([s("--features"), s("sr-app/custom-protocol")]);
    cargo(profile, &a)?;
    cargo(profile, &[s("-p"), s(DAEMON.1), s("--bin"), s(DAEMON.0)])
}

pub fn main(args: &[String]) -> Result<(), String> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return Err("package-mac runs on macOS on Apple silicon".into());
    }
    let o = parse(args)?;
    let version = env!("CARGO_PKG_VERSION");
    let root = fetch::workspace_root();
    let identifier = bundle_id(&root)?;
    if !plain(version) {
        return Err(format!("unexpected version {version:?}"));
    }
    if !root.join("ui").join("dist").join("index.html").is_file() {
        return Err("ui/dist is missing: run `npm ci && npm run build` in ui/ first".into());
    }

    // 1. Pinned inputs.
    let pdfium = fetch::pdfium_for_host()?;
    fetch::fetch(pdfium, &o.vendor)?;
    let models = o.vendor.join("models").join(MODEL_DIR);
    crate::models::fetch_models(&["--dir".into(), models.display().to_string()])?;

    // 2. Build.
    if o.build {
        build(&o.profile)?;
    }
    let out = crate::package::profile_dir(&o.profile);

    // 3. Assemble.
    let pkg = target_dir().join("package").join("mac");
    let app = pkg.join(format!("{APP_NAME}.app"));
    let _ = std::fs::remove_dir_all(&app);
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    let frameworks = contents.join("Frameworks");
    let resources = contents.join("Resources");
    let mut bytes = 0u64;
    let mut code: Vec<(PathBuf, String)> = Vec::new();
    for (bin, _) in HELPERS.iter().chain([&DAEMON]) {
        let dst = macos.join(bin);
        bytes += copy(&out.join(bin), &dst)?;
        code.push((dst, format!("{identifier}.{bin}")));
    }
    let main_exe = macos.join(MAIN_EXE);
    bytes += copy(&out.join(MAIN_EXE), &main_exe)?;
    for (_, name, _) in pdfium.files {
        let src = o.vendor.join(pdfium.dir).join(name);
        if name.ends_with(".dylib") {
            let dst = frameworks.join(name);
            bytes += copy(&src, &dst)?;
            code.insert(0, (dst, format!("{identifier}.pdfium")));
        } else {
            bytes += copy(&src, &resources.join("licenses").join(name))?;
        }
    }
    for f in crate::package::model_files() {
        bytes += copy(
            &models.join(&f),
            &resources.join("models").join(MODEL_DIR).join(&f),
        )?;
    }
    bytes += copy(
        &root.join("LICENSE"),
        &resources
            .join("licenses")
            .join("LICENSE-selene-retrieval.txt"),
    )?;
    make_icns(
        &root
            .join("crates")
            .join("app")
            .join("icons")
            .join("icon.png"),
        &resources.join("icon.icns"),
        &pkg,
    )?;
    std::fs::write(
        contents.join("Info.plist"),
        info_plist(version, &identifier),
    )
    .map_err(|e| format!("Info.plist: {e}"))?;
    std::fs::write(contents.join("PkgInfo"), "APPL????").map_err(|e| format!("PkgInfo: {e}"))?;

    // 5. Guard (before signing, on the exact files that ship).
    for (p, _) in &code {
        check_links(p)?;
    }
    check_links(&main_exe)?;

    // 4. Sign inside out, then the bundle (which signs the main exe), then verify.
    for (p, id) in &code {
        sign(p, Some(id))?;
    }
    sign(&app, None)?;
    let mut v = tool("/usr/bin/codesign");
    v.args(["--verify", "--strict", "--deep", "--verbose=2"])
        .arg(&app);
    run(v, "codesign --verify")?;
    let signed = std::env::var_os("SR_MAC_SIGN_IDENTITY").is_some();
    println!(
        "package-mac: {} ({:.1} MiB, {})",
        app.display(),
        bytes as f64 / (1024.0 * 1024.0),
        if signed {
            "signed"
        } else {
            "ad-hoc signed, not notarized"
        }
    );

    // 6. Disk image.
    if !o.dmg {
        return Ok(());
    }
    let dmg_root = pkg.join("dmg");
    let _ = std::fs::remove_dir_all(&dmg_root);
    std::fs::create_dir_all(&dmg_root).map_err(|e| format!("{}: {e}", dmg_root.display()))?;
    let mut ditto = tool("/usr/bin/ditto");
    ditto
        .arg(&app)
        .arg(dmg_root.join(format!("{APP_NAME}.app")));
    run(ditto, "ditto")?;
    std::os::unix::fs::symlink("/Applications", dmg_root.join("Applications"))
        .map_err(|e| format!("Applications link: {e}"))?;
    let dmg = pkg.join(format!("SeleneRetrieval-{version}-arm64.dmg"));
    let _ = std::fs::remove_file(&dmg);
    let mut h = tool("/usr/bin/hdiutil");
    h.args([
        "create",
        "-volname",
        APP_NAME,
        "-fs",
        "HFS+",
        "-format",
        "UDZO",
        "-srcfolder",
    ])
    .arg(&dmg_root)
    .arg(&dmg);
    run(h, "hdiutil create")?;
    let _ = std::fs::remove_dir_all(&dmg_root);
    std::fs::write(pkg.join("SHA256SUMS-mac.txt"), sha_line(&dmg)? + "\n")
        .map_err(|e| format!("SHA256SUMS-mac.txt: {e}"))?;
    let size = std::fs::metadata(&dmg).map(|m| m.len()).unwrap_or(0);
    println!(
        "package-mac: {} ({:.1} MB)",
        dmg.display(),
        size as f64 / 1_000_000.0
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_plist_names_the_main_exe_and_identifier() {
        let p = info_plist("0.1.0", "ai.selene.retrieval");
        assert!(p.contains("<key>CFBundleExecutable</key>\n  <string>sr-app</string>"));
        assert!(p.contains("<string>ai.selene.retrieval</string>"));
        assert!(p.contains("<key>LSMinimumSystemVersion</key>"));
        assert!(p.ends_with("</plist>\n"));
    }

    #[test]
    fn parse_flags() {
        let o = parse(&["--no-build".into(), "--no-dmg".into()]).unwrap();
        assert!(!o.build && !o.dmg);
        assert_eq!(o.profile, "dist");
        assert!(parse(&["--msi".into()]).is_err());
    }

    #[test]
    fn plain_values_only() {
        assert!(plain("Selene Retrieval"));
        assert!(plain("0.1.0"));
        assert!(!plain("a<b"));
        assert!(!plain("a&b"));
    }
}
