//! Delay-load the GPU DLLs (Amendment 4.8, m1-d O1). sr-engine's own test and bench executables
//! link sr-providers/local (static ORT with the DirectML EP); without `/DELAYLOAD` the loader would
//! map `DirectML.dll` by name before `main`. `rustc-link-arg` from a dependency's build script
//! never reaches this package, so every package with ORT-linked executables emits the arguments
//! itself, from the one definition in `sr_providers::dml::DELAYLOAD_ARGS`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let local = std::env::var_os("CARGO_FEATURE_LOCAL").is_some();
    if windows && local {
        for a in sr_providers::dml::DELAYLOAD_ARGS {
            println!("cargo:rustc-link-arg={a}");
        }
        // Test executables that never reference ORT get "LNK4199: /DELAYLOAD ignored"; harmless.
        println!("cargo:rustc-link-arg=/IGNORE:4199");
    }
}
