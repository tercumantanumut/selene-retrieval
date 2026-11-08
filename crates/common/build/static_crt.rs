// Shared build-script helper: per-package static VC++ runtime (Amendment 3.16).
//
// Included with `include!` by the build.rs of every exe that must run on a clean Windows
// machine without the VC++ redistributable: sr-bridge, sr-extract, sr-app (and sr-launch when it
// exists). `sr-daemon` deliberately has no such build.rs: it links ORT, whose prebuilt objects are
// /MD-only (Amendment 3.1), so it keeps the dynamic CRT and ships the four VC++ DLLs app-local.
//
// Why a build script and not `+crt-static` in .cargo/config.toml: cargo merges
// `target.<triple>.rustflags` for the whole build, so a global flag would also hit sr-daemon (and
// the M0 spikes). `cargo:rustc-link-arg` from a build script applies only to this package's
// linker invocations.
//
// What it links ("hybrid CRT", the technique used by `static_vcruntime` and tauri-build):
//   * vcruntime + C++ STL statically (libvcruntime.lib, libcmt.lib, libcpmt.lib), so no
//     VCRUNTIME140*.dll / MSVCP140*.dll import;
//   * the Universal CRT dynamically from the OS (ucrt.lib -> api-ms-win-crt-* apisets, inbox on
//     Windows 10+). Unlike a fully static libucrt, this also links C objects that cc compiled
//     with /MD (their `__imp_` UCRT references resolve), which is what broke M0-B's
//     `+crt-static` attempt.
// Release CRT only: Rust links the release CRT in debug builds too.
//
// Skipped when the target is not MSVC or `+crt-static` is already set via RUSTFLAGS.
#[allow(dead_code)]
fn static_vc_runtime() {
    println!("cargo:rerun-if-env-changed=SR_DYNAMIC_CRT");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|v| v == "msvc");
    let already_static = std::env::var("CARGO_CFG_TARGET_FEATURE")
        .is_ok_and(|f| f.split(',').any(|x| x == "crt-static"));
    // Escape hatch for diagnosing link problems locally; never set in CI or release builds.
    let opt_out = std::env::var_os("SR_DYNAMIC_CRT").is_some();
    if !msvc || already_static || opt_out {
        return;
    }
    for lib in [
        "msvcrt.lib",
        "msvcrtd.lib",
        "vcruntime.lib",
        "vcruntimed.lib",
        "libvcruntimed.lib",
        "libcmtd.lib",
        "msvcprt.lib",
        "msvcprtd.lib",
        "libcpmtd.lib",
        "libucrt.lib",
        "libucrtd.lib",
        "ucrtd.lib",
    ] {
        println!("cargo:rustc-link-arg=/NODEFAULTLIB:{lib}");
    }
    for lib in ["libcmt.lib", "libvcruntime.lib", "libcpmt.lib", "ucrt.lib"] {
        println!("cargo:rustc-link-arg=/DEFAULTLIB:{lib}");
    }
}
