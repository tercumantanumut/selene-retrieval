//! `cargo xtask fetch-models [--all] [--dir <d>]`: downloads the five pinned files of the bundled
//! Minimum model (bge-small-en-v1.5: `model_optimized.onnx` + four tokenizer side files) for the
//! model-backed tests (M1-D O4). CI runs those tests with `SR_REQUIRE_MODELS=1`, so a skip fails.
//!
//! `--all` also fetches what the Standard / High model tests load (`sr-providers` `tier_models`
//! and `download`), about 1.3 GB: `arctic-m-v1.5/` gets the Standard fp16 and fp32 files and its
//! tokenizer side files, `arctic-l/` the High fp16 file and its tokenizer side files. These
//! folders sit next to the Minimum one (the parent of `--dir`).
//!
//! Pins come from the binary's own source of truth (`sr_common::tier::{MINIMUM, STANDARD, HIGH}`
//! for the ONNX files, `sr_providers::models::tokenizer_pins` for the side files): repo, revision,
//! size and sha256 are never duplicated here. Files already present with the right hash are kept
//! (CI cache). Each download goes to `<name>.part` and is renamed only after its sha256 matches.
//!
//! Default directory: `crates/providers/models/bge-small-en-v1.5` (gitignored), which is where the
//! sr-providers tests look when `SR_TEST_MODEL_DIR` is unset. Uses the inbox `curl.exe`.

use sr_common::tier::{HIGH, MINIMUM, ModelFile, STANDARD, Tier};
use sr_providers::models::{BUNDLED_MODEL_DIR, sha256_hex, tokenizer_pins};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Folder of the Standard model files, as `tests/tier_models.rs` and `tests/download.rs` name it.
const STANDARD_TEST_DIR: &str = "arctic-m-v1.5";
/// Folder of the High model files, as `tests/tier_models.rs` names it.
const HIGH_TEST_DIR: &str = "arctic-l";

struct Pinned {
    url: String,
    name: String,
    sha256: &'static str,
    bytes: Option<u64>,
}

fn hf_url(repo: &str, rev: &str, path: &str) -> String {
    format!("https://huggingface.co/{repo}/resolve/{rev}/{path}")
}

/// One ONNX file, saved under its base name (`ModelPaths::in_dir`).
fn onnx(f: &ModelFile) -> Pinned {
    Pinned {
        url: hf_url(f.hf_repo, f.hf_revision, f.path),
        name: f.path.rsplit('/').next().unwrap_or(f.path).to_string(),
        sha256: f.sha256,
        bytes: Some(f.bytes),
    }
}

/// The four tokenizer side files of a tier's model.
fn side_files(tier: Tier) -> Vec<Pinned> {
    let t = tokenizer_pins(tier);
    t.files()
        .into_iter()
        .map(|a| Pinned {
            url: hf_url(t.hf_repo, t.hf_revision, a.name),
            name: a.name.to_string(),
            sha256: a.sha256,
            bytes: None,
        })
        .collect()
}

fn pinned_files() -> Vec<Pinned> {
    let mut v = vec![onnx(&MINIMUM.gpu_file)];
    v.extend(side_files(Tier::Minimum));
    v
}

/// The `--all` extras: `(folder next to the Minimum one, files)`.
fn tier_test_files() -> Vec<(&'static str, Vec<Pinned>)> {
    let mut standard = vec![onnx(&STANDARD.gpu_file), onnx(&STANDARD.cpu_file)];
    standard.extend(side_files(Tier::Standard));
    let mut high = vec![onnx(&HIGH.gpu_file)];
    high.extend(side_files(Tier::High));
    vec![(STANDARD_TEST_DIR, standard), (HIGH_TEST_DIR, high)]
}

fn file_ok(path: &Path, p: &Pinned) -> Result<bool, String> {
    match std::fs::read(path) {
        Ok(b) => {
            if p.bytes.is_some_and(|n| n != b.len() as u64) {
                return Ok(false);
            }
            Ok(sha256_hex(&b) == p.sha256)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Fetches `files` into `dir` (verify, then rename); files already there with the right hash stay.
fn fetch_into(dir: &Path, files: &[Pinned]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for p in files {
        let dest = dir.join(&p.name);
        if file_ok(&dest, p)? {
            println!("fetch-models: {} ok (cached)", p.name);
            continue;
        }
        let part = dir.join(format!("{}.part", p.name));
        println!("fetch-models: {} <- {}", p.name, p.url);
        let status = Command::new("curl")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--retry",
                "3",
            ])
            .args(["--proto", "=https", "--tlsv1.2", "--output"])
            .arg(&part)
            .arg(&p.url)
            .status()
            .map_err(|e| format!("could not run curl: {e}"))?;
        if !status.success() {
            let _ = std::fs::remove_file(&part);
            return Err(format!("download of {} failed ({status})", p.name));
        }
        if !file_ok(&part, p)? {
            let _ = std::fs::remove_file(&part);
            return Err(format!("{}: sha256/size mismatch against the pin", p.name));
        }
        std::fs::rename(&part, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    println!(
        "fetch-models: {} pinned files verified in {}",
        files.len(),
        dir.display()
    );
    Ok(())
}

pub fn fetch_models(args: &[String]) -> Result<(), String> {
    let mut dir: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("crates")
        .join("providers")
        .join("models")
        .join(BUNDLED_MODEL_DIR);
    let mut all = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => dir = it.next().ok_or("--dir needs a path")?.into(),
            "--all" => all = true,
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    fetch_into(&dir, &pinned_files())?;
    if all {
        let root = dir
            .parent()
            .ok_or_else(|| format!("{} has no parent folder", dir.display()))?;
        for (name, files) in tier_test_files() {
            fetch_into(&root.join(name), &files)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_pinned(p: &Pinned) {
        assert_eq!(p.sha256.len(), 64, "{}", p.name);
        assert!(
            p.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "{}",
            p.name
        );
        assert!(p.url.starts_with("https://huggingface.co/"), "{}", p.url);
        assert!(
            !p.url.contains("/main/"),
            "must be pinned to a revision: {}",
            p.url
        );
    }

    #[test]
    fn five_pinned_files_with_hashes() {
        let f = pinned_files();
        assert_eq!(f.len(), 5);
        assert_eq!(f[0].name, "model_optimized.onnx");
        f.iter().for_each(assert_pinned);
    }

    /// `--all`: exactly the files `tests/tier_models.rs` and `tests/download.rs` load, from the
    /// tier pins.
    #[test]
    fn all_adds_the_standard_and_high_test_files() {
        let sets = tier_test_files();
        let names: Vec<(&str, Vec<&str>)> = sets
            .iter()
            .map(|(d, f)| (*d, f.iter().map(|p| p.name.as_str()).collect()))
            .collect();
        let side = [
            "tokenizer.json",
            "config.json",
            "tokenizer_config.json",
            "special_tokens_map.json",
        ];
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].0, "arctic-m-v1.5");
        assert_eq!(&names[0].1[..2], ["model_fp16.onnx", "model.onnx"]);
        assert_eq!(names[1].0, "arctic-l");
        assert_eq!(names[1].1[0], "model_fp16.onnx");
        for (_, files) in &names {
            let mut rest: Vec<&str> = files
                .iter()
                .copied()
                .filter(|n| !n.ends_with(".onnx"))
                .collect();
            rest.sort_unstable();
            let mut want = side.to_vec();
            want.sort_unstable();
            assert_eq!(rest, want);
        }
        for (_, files) in &sets {
            files.iter().for_each(assert_pinned);
        }
        let (std_files, high_files) = (&sets[0].1, &sets[1].1);
        assert_eq!(std_files[0].sha256, STANDARD.gpu_file.sha256);
        assert_eq!(std_files[1].sha256, STANDARD.cpu_file.sha256);
        assert_eq!(high_files[0].sha256, HIGH.gpu_file.sha256);
        assert!(std_files[0].url.contains(STANDARD.gpu_file.hf_revision));
        assert!(high_files[0].url.contains(HIGH.gpu_file.hf_revision));
        assert!(
            std_files[2]
                .url
                .contains(tokenizer_pins(Tier::Standard).hf_revision)
        );
        assert!(
            high_files[1]
                .url
                .contains(tokenizer_pins(Tier::High).hf_revision)
        );
    }
}
