//! Delay-load the GPU DLLs that pyke's static ONNX Runtime imports on Windows (it is always built
//! with the DirectML EP: `DMLCreateDevice1`, `D3D12CreateDevice`, `CreateDXGIFactory2`).
//!
//! Without this, the loader maps `DirectML.dll` by *name* before `main`, so a planted copy in the
//! (user-writable, per-user) install dir is loaded before any hardening can run (M0-B test 4). With
//! it, nothing is mapped until `sr_providers::dml` has loaded the verified file by absolute path
//! (Amendment 3.3), and the delay-load helper then binds to that already-loaded module.
//!
//! `cargo:rustc-link-arg-*` only reaches this package's own test / example / bench executables.
//! **Every binary that links `sr-providers` with `local` (sr-daemon) must emit the same four lines
//! from its own build.rs** — see [`DELAYLOAD_ARGS`] in `src/dml.rs` and the M1-D report.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let local = std::env::var_os("CARGO_FEATURE_LOCAL").is_some();
    if windows && local {
        // Plain `rustc-link-arg` = every linked target of this package (lib unit-test exe,
        // integration tests, examples); `-tests`/`-benches` error out if no such target exists.
        for dll in ["DirectML.dll", "d3d12.dll", "dxgi.dll"] {
            println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-arg=delayimp.lib");
    }
}
