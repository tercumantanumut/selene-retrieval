//! Links the VC++ runtime statically for this exe (Amendment 3.16; hybrid CRT from
//! `crates/common/build/static_crt.rs`), then runs tauri-build (Windows resources, manifest,
//! capabilities/ACL). tauri-build's own static-VC-runtime step is turned off so exactly one
//! mechanism decides the CRT (`cargo xtask check-crt` verifies the result).
//!
//! M3: embeds the generated AI provider list (`crates/providers/data/selene-providers.json`, by
//! `xtask sync-providers`) as `$OUT_DIR/providers.json`, so the UI never hard-codes provider or
//! model names (src/providers.rs). Until that file exists an empty list is embedded.

include!("../common/build/static_crt.rs");
// M5-I: `SR_APP_MANIFEST` (the app's application manifest, passed to tauri-build below).
include!("../../installer/manifests/embed.rs");

use std::path::{Path, PathBuf};

/// Manifest names, in order (M3-P writes `selene-providers.json`; `providers.json` is accepted).
const MANIFESTS: &[&str] = &["selene-providers.json", "providers.json"];

fn embed_provider_list() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let data = manifest_dir.join("../providers/data");
    // Watch the folder (a new manifest appearing re-runs this script) and each candidate file.
    if data.is_dir() {
        println!("cargo:rerun-if-changed={}", data.display());
    } else {
        println!(
            "cargo:rerun-if-changed={}",
            manifest_dir.join("../providers").display()
        );
    }
    let found = MANIFESTS.iter().map(|n| data.join(n)).find(|p| p.is_file());
    let doc = match &found {
        Some(p) => {
            println!("cargo:rerun-if-changed={}", p.display());
            std::fs::read_to_string(p)
                .unwrap_or_else(|e| panic!("can't read the provider list {}: {e}", p.display()))
        }
        None => {
            println!(
                "cargo:warning=sr-app: no provider list in crates/providers/data yet (run `cargo xtask sync-providers`); the app lists no AI providers"
            );
            "[]".to_owned()
        }
    };
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    write(&out.join("providers.json"), &doc);
}

fn write(p: &Path, doc: &str) {
    if std::fs::read_to_string(p).ok().as_deref() != Some(doc) {
        std::fs::write(p, doc).unwrap_or_else(|e| panic!("can't write {}: {e}", p.display()));
    }
}

fn main() {
    static_vc_runtime();
    embed_provider_list();
    // M5-I: our manifest (Common-Controls v6 as before, plus PerMonitorV2, longPathAware,
    // supportedOS, asInvoker) instead of tauri-build's default; installer/manifests/.
    let attrs = tauri_build::Attributes::new().windows_attributes(
        tauri_build::WindowsAttributes::new()
            .static_vc_runtime(false)
            .app_manifest(SR_APP_MANIFEST),
    );
    if let Err(e) = tauri_build::try_build(attrs) {
        panic!("tauri-build failed: {e:#}");
    }
}
