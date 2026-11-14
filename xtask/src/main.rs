//! xtask: dev-only tasks. M0 implements the two CI guards; M1 adds the golden-set eval runner;
//! specta bindings and packaging arrive with their milestones.
//!
//! * `cargo run -p xtask -- check-bridge-deps`: fails if `sr-bridge` links a heavy native stack
//!   (docs/05-repo-skeleton.md: LanceDB, ort, fastembed, pdfium must never reach the bridge).
//! * `cargo run -p xtask -- schema [out.json]`: writes the JSON Schemas of `Settings`,
//!   `FolderSettings` and `CollectionSettings` (drive the generated Basic/Advanced settings UI).
//! * `cargo run -p xtask -- eval --golden <g.jsonl> --engine <adapter> ...`: retrieval metrics
//!   (recall@{5,10,50}, nDCG@10, MRR@10, latency p50/p95), variant comparison and gates. See
//!   [`eval`] and `eval/README.md`.
//! * `cargo xtask check-crt` / `cargo xtask check-delayload`: binary-import guards (no VC++
//!   runtime DLL in the static-CRT exes; DirectML stack delay-load only in `sr-daemon`). See
//!   [`crt`].
//!
//! * `cargo xtask check-manifest [--profile <p>] [--exe <path>]...`: every shipped exe embeds the
//!   application manifest (asInvoker, supportedOS Win10/11, longPathAware, PerMonitorV2; M5-I),
//!   and every shipped `main()` opens with its DLL-search hardening call (S-08, source check).
//!   See [`manifest`].
//!
//! * `cargo xtask fetch-models [--all] [--dir <d>]`: downloads and sha256-checks the five pinned
//!   Minimum model files for the model-backed tests (CI sets `SR_REQUIRE_MODELS=1`); `--all` adds
//!   the Standard and High files a few tests load (about 1.3 GB). See [`models`].
//!
//! * `cargo xtask fetch-pdfium | fetch-runtime | fetch-nsis | fetch-wix [--dir <d>]`: sha256-pinned pdfium
//!   7881, DirectML 1.15.4 + the four VC++ 14.44 runtime DLLs, NSIS 3.13 (M2-I). See [`fetch`].
//! * `cargo xtask package [--opt-size] [--no-build]`: size-optimised build, staging, signing
//!   hooks and the per-user NSIS installer. See [`package`]. `cargo xtask sign <file>...`: signs
//!   when credentials are configured, otherwise reports UNSIGNED. See [`sign`].
//! * `cargo xtask package-mac [--no-build] [--no-dmg]`: the macOS app bundle (ad-hoc signed) and
//!   disk image. See [`package_mac`].
//!
//! * `cargo xtask sync-providers [--check]`: regenerates `crates/providers/data/selene-providers.json`
//!   from Selene's provider registry (read-only) plus our policy table; `--check` fails on drift
//!   (CI). See [`sync_providers`].
//!
//! `cargo xtask <cmd>` is an alias for `cargo run -p xtask -- <cmd>` (.cargo/config.toml).

mod crt;
mod eval;
mod fetch;
mod manifest;
mod models;
mod package;
mod package_mac;
mod pe;
mod sign;
mod sync_providers;

use std::process::{Command, ExitCode};

/// Crates that must never appear in `sr-bridge`'s dependency graph.
const BRIDGE_FORBIDDEN: &[&str] = &[
    "lancedb",
    "lance",
    "ort",
    "ort-sys",
    "fastembed",
    "pdfium-render",
    "tantivy",
    "arrow-array",
    "rusqlite",
    "sr-engine",
    "sr-core",
    "sr-catalog",
    "sr-providers",
    // blake3 rayon/mmap are daemon-only (sr-common feature `parallel-hash`, wave 2).
    "rayon-core",
    "memmap2",
];

fn check_bridge_deps() -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args([
            "tree",
            "-p",
            "sr-bridge",
            "-e",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}",
            "--target",
            "all",
        ])
        .output()
        .map_err(|e| format!("could not run cargo tree: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo tree failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let found: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|name| BRIDGE_FORBIDDEN.contains(name))
        .collect();
    if found.is_empty() {
        println!(
            "sr-bridge dependency graph is clean ({} lines checked)",
            text.lines().count()
        );
        Ok(())
    } else {
        Err(format!(
            "sr-bridge must not depend on: {}",
            found.join(", ")
        ))
    }
}

fn write_schema(out: Option<&str>) -> Result<(), String> {
    use sr_common::config::{CollectionSettings, FolderSettings, Settings};
    let schema = serde_json::json!({
        "settings": Settings::json_schema(),
        "folder_settings": schemars::schema_for!(FolderSettings),
        "collection_settings": schemars::schema_for!(CollectionSettings),
    });
    let body = serde_json::to_string_pretty(&schema).map_err(|e| e.to_string())?;
    match out {
        Some(path) => std::fs::write(path, body + "\n").map_err(|e| format!("{path}: {e}")),
        None => {
            println!("{body}");
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("check-bridge-deps") => check_bridge_deps(),
        Some("check-crt") => crt::check_crt(&args[1..]),
        Some("check-delayload") => crt::check_delayload(&args[1..]),
        Some("check-manifest") => manifest::main(&args[1..]),
        Some("fetch-models") => models::fetch_models(&args[1..]),
        Some("fetch-pdfium") => fetch::fetch_pdfium(&args[1..]),
        Some("fetch-runtime") => fetch::fetch_runtime(&args[1..]),
        Some("fetch-nsis") => fetch::fetch_nsis(&args[1..]),
        Some("fetch-webview2") => fetch::fetch_webview2_cmd(&args[1..]),
        Some("fetch-wix") => fetch::fetch_wix(&args[1..]),
        Some("package") => package::main(&args[1..]),
        Some("package-mac") => package_mac::main(&args[1..]),
        Some("sign") => sign::main(&args[1..]),
        Some("schema") => write_schema(args.get(1).map(String::as_str)),
        Some("eval") => eval::main(&args[1..]),
        Some("sync-providers") => sync_providers::main(&args[1..]),
        _ => Err(format!(
            "usage: xtask <check-bridge-deps | check-crt [opts] | check-delayload [opts] | check-manifest [opts] \
             | fetch-models [--all] [--dir <d>] | fetch-pdfium | fetch-runtime | fetch-nsis | fetch-wix | fetch-webview2 [--offline [--latest]] [--dir <d>] \
             | package [--msi] [--webview2-offline] [--opt-size] [--no-build] [--stage-only] [--profile <p>] [--vendor <d>] | package-mac [--no-build] [--no-dmg] | sign <file>... \
             | schema [out.json] | sync-providers [--check] [--selene <dir>] [--out <file>] | eval ...>\n  check-* opts: [--profile <p>] [--no-build] \
             [--exe <path>]...\n{}",
            eval::USAGE
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}
