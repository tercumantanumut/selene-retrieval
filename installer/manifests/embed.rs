// Shared build-script helper (owner: M5-I): embeds the application manifest into the shipped
// windowless exes. Included with `include!` by the build.rs of sr-bridge (sr-bridge, sr-launch),
// sr-extract, sr-mcp-server (sr-daemon) and installer/sr-setup. sr-app gets `SR_APP_MANIFEST`
// through tauri-build instead (`WindowsAttributes::app_manifest`), because tauri-build already
// compiles a manifest resource and two manifests would collide.
//
// What it does: writes `console.manifest` (compiled in with include_str!, so editing it rebuilds
// the build script) to OUT_DIR and passes link.exe `/MANIFEST:EMBED /MANIFESTINPUT:<file>
// /MANIFESTUAC:NO` for the named bins only (`rustc-link-arg-bin`), so test and bench executables
// are unaffected. `/MANIFESTUAC:NO` because the manifest declares `asInvoker` itself.
// Skipped for non-MSVC targets. `cargo xtask check-manifest` verifies the result on the exact
// files that ship.
#[allow(dead_code)]
const SR_CONSOLE_MANIFEST: &str = include_str!("console.manifest");
#[allow(dead_code)]
const SR_APP_MANIFEST: &str = include_str!("sr-app.manifest");

#[allow(dead_code)]
fn embed_console_manifest(bins: &[&str]) {
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|v| v == "msvc");
    if !msvc {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    let file = out.join("sr-console.manifest");
    if std::fs::read_to_string(&file).ok().as_deref() != Some(SR_CONSOLE_MANIFEST) {
        std::fs::write(&file, SR_CONSOLE_MANIFEST)
            .unwrap_or_else(|e| panic!("can't write {}: {e}", file.display()));
    }
    for bin in bins {
        println!("cargo:rustc-link-arg-bin={bin}=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin={bin}=/MANIFESTINPUT:{}",
            file.display()
        );
        println!("cargo:rustc-link-arg-bin={bin}=/MANIFESTUAC:NO");
    }
}
