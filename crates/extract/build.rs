//! Links the VC++ runtime statically for this exe (Amendment 3.16); see the helper for details.
//! Shared helper owned by M1-A (one CRT mechanism for sr-bridge, sr-extract and sr-app).

include!("../common/build/static_crt.rs");
// M5-I: application manifest (longPathAware, supportedOS, asInvoker) for the shipped bin.
include!("../../installer/manifests/embed.rs");

fn main() {
    static_vc_runtime();
    embed_console_manifest(&["sr-extract"]);
    // M1-B: parsers run on the worker's main thread (no per-request thread hop), so the main
    // thread gets the 64 MiB stack that hostile PDFs may need (pdfium recurses on structure).
    if std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|v| v == "msvc") {
        println!("cargo:rustc-link-arg-bin=sr-extract=/STACK:67108864");
    }
}
