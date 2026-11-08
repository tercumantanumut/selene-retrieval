//! Shared helpers for the model-backed integration tests.
//!
//! Model files are not committed (`crates/providers/models/` is gitignored). Point
//! `SR_TEST_MODEL_DIR` at a directory with the five bundled Minimum files, or copy them to
//! `crates/providers/models/bge-small-en-v1.5/`. Without them the tests print `SKIP` and pass,
//! unless `SR_REQUIRE_MODELS=1` (the M1-D gate sets it) turns that into a failure. Likewise
//! DirectML tests skip without a GPU / `DirectML.dll` unless `SR_REQUIRE_DML=1`.
#![allow(dead_code, clippy::expect_used)]

use std::path::{Path, PathBuf};

use sr_common::tier::{EpPlan, MINIMUM};
use sr_providers::ModelPaths;
use sr_providers::local::LocalOptions;

pub fn model_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("SR_TEST_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("models")
                .join("bge-small-en-v1.5")
        });
    if dir.join("model_optimized.onnx").is_file() {
        return Some(dir);
    }
    if std::env::var_os("SR_REQUIRE_MODELS").is_some() {
        panic!("SR_REQUIRE_MODELS set but no model in {}", dir.display());
    }
    eprintln!("SKIP: no bundled model in {}", dir.display());
    None
}

/// Directory holding a Microsoft-signed `DirectML.dll` for tests: `SR_TEST_DML_DIR`, else the
/// pyke ORT download cache (it ships DirectML 1.15.4 with the static build), else System32.
pub fn dml_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = std::env::var_os("SR_TEST_DML_DIR") {
        v.push(PathBuf::from(d));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let base = Path::new(&local).join(r"ort.pyke.io\dfbin\x86_64-pc-windows-msvc");
        if let Ok(rd) = std::fs::read_dir(base) {
            for e in rd.flatten() {
                if e.path().join("DirectML.dll").is_file() {
                    v.push(e.path());
                }
            }
        }
    }
    v.push(PathBuf::from(r"C:\Windows\System32"));
    v
}

pub fn dxgi_index() -> u32 {
    std::env::var("SR_TEST_DXGI_INDEX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

pub fn require_dml() -> bool {
    std::env::var_os("SR_REQUIRE_DML").is_some()
}

pub fn opts(dir: &Path, ep: EpPlan) -> LocalOptions {
    let mut o = LocalOptions::new(
        &MINIMUM,
        MINIMUM.gpu_file,
        ModelPaths::in_dir(dir, &MINIMUM.gpu_file),
        ep,
    );
    o.dml_search_dirs = dml_dirs();
    o
}

pub fn sentences() -> Vec<String> {
    serde_json::from_str(include_str!("../fixtures/bge_small_sentences.json")).expect("fixture")
}

/// sentence-transformers 6.1.0 / torch 2.14.1, BAAI/bge-small-en-v1.5, CLS, normalised, max_seq
/// 512 (spikes/native-stack/test11/reference.py).
pub fn reference() -> Vec<Vec<f32>> {
    serde_json::from_str(include_str!("../fixtures/bge_small_ref_st.json")).expect("fixture")
}

pub fn stats(a: &[Vec<f32>], b: &[Vec<f32>]) -> (f32, f32) {
    assert_eq!(a.len(), b.len());
    let cs: Vec<f32> = a
        .iter()
        .zip(b)
        .map(|(x, y)| {
            sr_providers::local::min_cosine(std::slice::from_ref(x), std::slice::from_ref(y))
        })
        .collect();
    let min = cs.iter().copied().fold(1.0, f32::min);
    let mean = cs.iter().sum::<f32>() / cs.len() as f32;
    (min, mean)
}
