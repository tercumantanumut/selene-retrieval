//! Binary-import guards (Amendments 3.1, 3.3, 4.1, 4.8), run in CI after a release build.
//!
//! * `check-crt`: `sr-bridge`, `sr-launch`, `sr-extract` and `sr-app` must import **no VC++
//!   runtime DLL** (`vcruntime*`, `msvcp*`, `concrt*`, plus `vccorlib*` and the debug
//!   `ucrtbased`). They link the hybrid CRT through `crates/common/build/static_crt.rs` and must
//!   start on a clean Windows machine without the VC++ redistributable. `sr-daemon` is allowed the
//!   dynamic CRT (prebuilt ORT is `/MD`-only; the four DLLs ship app-local) and is only listed.
//! * `check-delayload`: `sr-daemon` must import `DirectML.dll`, `d3d12.dll` and `dxgi.dll` **only**
//!   as delay-load imports, and `DirectML.dll` must be present there (proof that the daemon's own
//!   `build.rs` emitted `sr_providers::dml::DELAYLOAD_ARGS`). A load-time import would let a planted
//!   `DirectML.dll` map before `main()` calls `harden_dll_search()`.
//!
//! Usage:
//! ```text
//! cargo xtask check-crt       [--profile <p>] [--no-build] [--exe <path>]...
//! cargo xtask check-delayload [--profile <p>] [--no-build] [--exe <path>]
//! ```
//! `--profile` defaults to `release` (what ships). `--no-build` checks what is already in the
//! target directory. `--exe` checks the given files instead of the workspace exes (local triage).

use crate::pe::{Imports, imports_of_file};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Exes that must not import a VC++ runtime DLL, with the package that builds them.
const STATIC_CRT_EXES: &[(&str, &str)] = &[
    ("sr-bridge", "sr-bridge"),
    ("sr-launch", "sr-bridge"),
    ("sr-extract", "sr-extract"),
    ("sr-app", "sr-app"),
];
/// The engine process: dynamic CRT allowed, DirectML stack delay-loaded.
const DAEMON_EXE: &str = "sr-daemon";
/// Lower-case name prefixes of VC++ runtime DLLs (release and debug variants).
const VC_RUNTIME_PREFIXES: &[&str] = &["vcruntime", "msvcp", "concrt", "vccorlib", "ucrtbased"];
/// GPU DLLs that must be delay-loaded by the daemon (= `sr_providers::dml::DELAYLOAD_ARGS`).
const DELAY_ONLY: &[&str] = &["directml.dll", "d3d12.dll", "dxgi.dll"];
/// Must actually be present as a delay import (otherwise the delay-load wiring is missing).
const DELAY_REQUIRED: &[&str] = &["directml.dll"];

struct Opts {
    profile: String,
    build: bool,
    exes: Vec<PathBuf>,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        profile: "release".into(),
        build: true,
        exes: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--profile" => o.profile = it.next().ok_or("--profile needs a value")?.clone(),
            "--no-build" => o.build = false,
            "--exe" => o.exes.push(it.next().ok_or("--exe needs a path")?.into()),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(o)
}

fn workspace_root() -> PathBuf {
    // xtask/ is one level below the workspace root.
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    here.parent().unwrap_or(here).to_path_buf()
}

fn profile_dir(profile: &str) -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"));
    let target = if target.is_absolute() {
        target
    } else {
        workspace_root().join(target)
    };
    // The `dev` and `test` profiles write to target/debug; every other profile to target/<name>.
    let dir = match profile {
        "dev" | "test" => "debug",
        "bench" => "release",
        p => p,
    };
    target.join(dir)
}

fn cargo_build(profile: &str, args: &[&str]) -> Result<(), String> {
    if std::env::var_os("SR_DYNAMIC_CRT").is_some() {
        return Err(
            "SR_DYNAMIC_CRT is set: it disables the static VC++ runtime (local \
                    diagnostics only); unset it before running this guard"
                .into(),
        );
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(workspace_root())
        .args(["build", "--profile", profile])
        .args(args);
    println!("xtask: running {cmd:?}");
    let status = cmd
        .status()
        .map_err(|e| format!("could not run cargo build: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo build failed ({status})"))
    }
}

fn has_prefix(dll: &str, prefixes: &[&str]) -> bool {
    let l = dll.to_ascii_lowercase();
    prefixes.iter().any(|p| l.starts_with(p))
}

/// The CRT verdict for one static-CRT image: the offending DLLs, if any.
fn vc_runtime_imports(imp: &Imports) -> Vec<&str> {
    imp.load_time
        .iter()
        .chain(&imp.delay_load)
        .map(String::as_str)
        .filter(|d| has_prefix(d, VC_RUNTIME_PREFIXES))
        .collect()
}

/// The delay-load verdict for the daemon image: problems found (empty = pass).
fn delayload_problems(imp: &Imports) -> Vec<String> {
    let lt: Vec<String> = imp
        .load_time
        .iter()
        .map(|d| d.to_ascii_lowercase())
        .collect();
    let dl: Vec<String> = imp
        .delay_load
        .iter()
        .map(|d| d.to_ascii_lowercase())
        .collect();
    let mut problems: Vec<String> = DELAY_ONLY
        .iter()
        .filter(|d| lt.iter().any(|x| x == *d))
        .map(|d| format!("{d} is a load-time import (must be /DELAYLOAD)"))
        .collect();
    problems.extend(
        DELAY_REQUIRED
            .iter()
            .filter(|d| !dl.iter().any(|x| x == *d))
            .map(|d| {
                format!("{d} is not a delay-load import (daemon build.rs missing DELAYLOAD_ARGS?)")
            }),
    );
    problems
}

fn show(path: &Path, imp: &Imports) {
    println!("  {}", path.display());
    println!("    load-time : {}", imp.load_time.join(", "));
    if !imp.delay_load.is_empty() {
        println!("    delay-load: {}", imp.delay_load.join(", "));
    }
}

fn exe_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

pub fn check_crt(args: &[String]) -> Result<(), String> {
    let o = parse_opts(args)?;
    let targets: Vec<PathBuf> = if o.exes.is_empty() {
        if o.build {
            // Built in one invocation without sr-daemon so feature unification cannot drag
            // daemon-only features (e.g. sr-common/parallel-hash) into the static exes.
            let mut a = Vec::new();
            for (bin, pkg) in STATIC_CRT_EXES {
                a.extend(["-p", pkg, "--bin", bin]);
            }
            cargo_build(&o.profile, &a)?;
        }
        let dir = profile_dir(&o.profile);
        STATIC_CRT_EXES
            .iter()
            .map(|(bin, _)| exe_path(&dir, bin))
            .collect()
    } else {
        o.exes.clone()
    };

    println!("check-crt: no VC++ runtime import allowed in:");
    let mut failures = Vec::new();
    for path in &targets {
        match imports_of_file(path) {
            Ok(imp) => {
                show(path, &imp);
                let bad = vc_runtime_imports(&imp);
                if !bad.is_empty() {
                    failures.push(format!("{}: imports {}", path.display(), bad.join(", ")));
                }
            }
            Err(e) => failures.push(e),
        }
    }
    if o.exes.is_empty() {
        let daemon = exe_path(&profile_dir(&o.profile), DAEMON_EXE);
        match imports_of_file(&daemon) {
            Ok(imp) => {
                println!("allowed the dynamic CRT (Amendment 3.1), listed for reference:");
                show(&daemon, &imp);
            }
            Err(_) => println!("({DAEMON_EXE} not built in this profile; skipped, it is exempt)"),
        }
    }
    if failures.is_empty() {
        println!(
            "check-crt: OK ({} exes, no VC++ runtime import)",
            targets.len()
        );
        Ok(())
    } else {
        Err(format!("check-crt failed:\n  {}", failures.join("\n  ")))
    }
}

pub fn check_delayload(args: &[String]) -> Result<(), String> {
    let o = parse_opts(args)?;
    let path = match o.exes.as_slice() {
        [] => {
            if o.build {
                cargo_build(&o.profile, &["--bin", DAEMON_EXE])?;
            }
            exe_path(&profile_dir(&o.profile), DAEMON_EXE)
        }
        [one] => one.clone(),
        _ => return Err("check-delayload takes at most one --exe".into()),
    };
    let imp = imports_of_file(&path)?;
    println!("check-delayload: DirectML/d3d12/dxgi must be delay-load only in:");
    show(&path, &imp);
    let problems = delayload_problems(&imp);
    if problems.is_empty() {
        println!("check-delayload: OK");
        Ok(())
    } else {
        Err(format!(
            "check-delayload failed for {}:\n  {}",
            path.display(),
            problems.join("\n  ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imp(lt: &[&str], dl: &[&str]) -> Imports {
        Imports {
            load_time: lt.iter().map(|s| (*s).to_string()).collect(),
            delay_load: dl.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn crt_rule_flags_every_vc_runtime_variant() {
        let clean = imp(
            &[
                "KERNEL32.dll",
                "ntdll.dll",
                "api-ms-win-crt-runtime-l1-1-0.dll",
                "ADVAPI32.dll",
            ],
            &[],
        );
        assert!(vc_runtime_imports(&clean).is_empty());
        for bad in [
            "VCRUNTIME140.dll",
            "vcruntime140_1.dll",
            "VCRUNTIME140D.dll",
            "MSVCP140.dll",
            "msvcp140_atomic_wait.dll",
            "CONCRT140.dll",
            "ucrtbased.dll",
        ] {
            assert_eq!(
                vc_runtime_imports(&imp(&["KERNEL32.dll", bad], &[])),
                vec![bad]
            );
            assert_eq!(
                vc_runtime_imports(&imp(&[], &[bad])),
                vec![bad],
                "delay-load too"
            );
        }
        // The inbox UCRT (ucrtbase.dll, api-ms-win-crt-*) is allowed: hybrid CRT (4.1).
        assert!(vc_runtime_imports(&imp(&["ucrtbase.dll"], &[])).is_empty());
    }

    #[test]
    fn delayload_rule() {
        let good = imp(
            &["KERNEL32.dll", "VCRUNTIME140.dll"],
            &["DirectML.dll", "d3d12.dll", "dxgi.dll"],
        );
        assert!(delayload_problems(&good).is_empty());
        let eager = imp(&["KERNEL32.dll", "DXGI.dll"], &["DirectML.dll"]);
        assert_eq!(delayload_problems(&eager).len(), 1);
        let missing = imp(&["KERNEL32.dll"], &[]);
        assert_eq!(
            delayload_problems(&missing).len(),
            1,
            "DirectML must be delay-imported"
        );
        let both = imp(&["DirectML.dll"], &[]);
        assert_eq!(delayload_problems(&both).len(), 2);
    }

    #[test]
    fn opts() {
        let a: Vec<String> = ["--profile", "ci", "--no-build"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = parse_opts(&a).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(o.profile, "ci");
        assert!(!o.build);
        assert!(parse_opts(&["--bogus".to_string()]).is_err());
        assert!(profile_dir("dev").ends_with("debug"));
        assert!(profile_dir("release").ends_with("release"));
    }
}
