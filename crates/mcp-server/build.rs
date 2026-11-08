//! sr-daemon's own build script (Amendment 4.8, m1-d O1, mandatory): the daemon links static ORT
//! with the DirectML EP, so `DirectML.dll`, `d3d12.dll` and `dxgi.dll` must be **delay-loaded**;
//! otherwise the loader maps a planted `DirectML.dll` from the install dir before `main` runs.
//! `main` then calls `sr_providers::dml::harden_dll_search()` as its first statement, and the
//! DirectML loader binds only a Microsoft-signed copy by absolute path.
//!
//! `rustc-link-arg` applies to every executable of this package (sr-daemon and the integration
//! tests, which link the engine too). The C runtime stays dynamic for the daemon (Amendment 3.1:
//! prebuilt ORT is `/MD`-only; the VC++ DLLs ship app-local), so the shared static-CRT helper is
//! deliberately **not** included here.

// M5-I: application manifest (longPathAware, supportedOS, asInvoker) for sr-daemon only; the
// static-CRT helper is still not included (see above).
include!("../../installer/manifests/embed.rs");

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    embed_console_manifest(&["sr-daemon"]);
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    if windows {
        for a in sr_providers::dml::DELAYLOAD_ARGS {
            println!("cargo:rustc-link-arg={a}");
        }
        // Test executables that never reference ORT get "LNK4199: /DELAYLOAD ignored"; harmless.
        println!("cargo:rustc-link-arg=/IGNORE:4199");
    }
}
