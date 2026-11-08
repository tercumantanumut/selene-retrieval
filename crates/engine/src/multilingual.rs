//! The multilingual search model (M3-Q; decision 3 / Amendment 1: "multilingual-e5-small stays a
//! separate one-click profile, orthogonal to tiers"; design page 12 [M3] offer).
//!
//! * **Model:** `intfloat/multilingual-e5-small` (MIT, 384 dims, mean pooling, `query: ` /
//!   `passage: ` prefixes). GPU file: the fp16 export `Xenova/multilingual-e5-small`
//!   `onnx/model_fp16.onnx` (235 MB); CPU file: the official fp32 `intfloat/multilingual-e5-small`
//!   `onnx/model.onnx` (470 MB) — the same split as Standard/High (fp16 on the GPU, fp32 on the
//!   CPU until an int8 gate clears, Amendment 3.4). Both pinned by revision + sha256 (HF LFS oids,
//!   checked 2026-10-19); the tokenizer side files too.
//! * **Path:** the same as a tier change: consent click → resumable, verifying download into
//!   `<data>\models` → load → background re-embed into a shadow index → atomic swap
//!   (`crate::reembed`). Chunk ids don't change (the chunker stays the bundled WordPiece one).
//!   Search keeps using the old index until the swap; a failure keeps it.
//! * **Tokenizer pins (P-2, fixed by the M3 tester):** the provider's verifying loader picks
//!   tokenizer pins by model id (`sr_providers::models::tokenizer_pins_for`), so these e5 pins
//!   (identical to `sr_providers::models::E5_SMALL_TOKENIZER`, checked by a test) are used.

use sr_common::tier::{MINIMUM, ModelFile, Pooling, Precision, TierProfile};

/// Canonical model id.
pub const MULTILINGUAL_MODEL_ID: &str = "intfloat/multilingual-e5-small";

/// GPU variant: fp16 export (EP-portable like every fp16 tier file).
pub const E5_SMALL_FP16: ModelFile = ModelFile {
    hf_repo: "Xenova/multilingual-e5-small",
    hf_revision: "761b726dd34fb83930e26aab4e9ac3899aa1fa78",
    path: "onnx/model_fp16.onnx",
    bytes: 235_336_732,
    precision: Precision::Fp16,
    sha256: "0e0fe349c99ea21c6f3aa273af21f7fb753c1e1174ef1647032029c2be3251c3",
};

/// CPU variant: the official fp32 export.
pub const E5_SMALL_FP32: ModelFile = ModelFile {
    hf_repo: "intfloat/multilingual-e5-small",
    hf_revision: "614241f622f53c4eeff9890bdc4f31cfecc418b3",
    path: "onnx/model.onnx",
    bytes: 470_268_510,
    precision: Precision::Fp32,
    sha256: "ca456c06b3a9505ddfd9131408916dd79290368331e7d76bb621f1cba6bc8665",
};

/// The profile the local provider runs it with. `tier` is Minimum only for the resource class
/// (batch sizes, ANN parameters); the catalog records it as a `custom` profile.
pub static MULTILINGUAL: TierProfile = TierProfile {
    model_id: MULTILINGUAL_MODEL_ID,
    dims: 384,
    pooling: Pooling::Mean,
    query_prefix: "query: ",
    doc_prefix: "passage: ",
    max_seq_tokens: 512,
    gpu_file: E5_SMALL_FP16,
    cpu_file: E5_SMALL_FP32,
    bundled: false,
    ..MINIMUM
};

/// Tokenizer side files, at the official repo's pinned revision (`tokenizer.json` is an LFS
/// file: oid = sha256; the small files were downloaded and hashed).
pub const E5_TOKENIZER_REPO: &str = "intfloat/multilingual-e5-small";
pub const E5_TOKENIZER_REVISION: &str = "614241f622f53c4eeff9890bdc4f31cfecc418b3";
pub const E5_TOKENIZER_FILES: [(&str, &str); 4] = [
    (
        "tokenizer.json",
        "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
    ),
    (
        "config.json",
        "69137736cab8b8903a07fe8afaafdda25aac55415a12a55d1bffa9f581abf959",
    ),
    (
        "tokenizer_config.json",
        "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b",
    ),
    (
        "special_tokens_map.json",
        "d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7",
    ),
];

/// "Search in many languages ({size} download)" (page 12): the file a new index would use.
pub fn download_mb(gpu: bool) -> u64 {
    let f = if gpu { E5_SMALL_FP16 } else { E5_SMALL_FP32 };
    f.bytes.div_ceil(1_000_000)
}

/// Where the files go under `<data>\models` (same layout as the tier downloads).
pub fn paths(data_dir: &std::path::Path, f: &ModelFile) -> sr_providers::ModelPaths {
    let models = data_dir.join("models");
    let rev = &E5_TOKENIZER_REVISION[..12];
    sr_providers::ModelPaths {
        onnx: models.join(format!("{}.onnx", f.sha256)),
        tokenizer_dir: models.join(format!(
            "tok-{}@{rev}",
            E5_TOKENIZER_REPO.replace('/', "--")
        )),
    }
}

/// The pinned file with this sha256.
pub fn file_for(sha256: &str) -> Option<ModelFile> {
    [E5_SMALL_FP16, E5_SMALL_FP32]
        .into_iter()
        .find(|f| f.sha256 == sha256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_and_pins() {
        assert_eq!(MULTILINGUAL.dims, 384);
        assert_eq!(MULTILINGUAL.pooling, Pooling::Mean);
        assert_eq!(
            (MULTILINGUAL.query_prefix, MULTILINGUAL.doc_prefix),
            ("query: ", "passage: ")
        );
        assert_eq!(download_mb(true), 236);
        assert_eq!(download_mb(false), 471);
        for (_, sha) in E5_TOKENIZER_FILES {
            assert_eq!(sha.len(), 64);
        }
        // One set of pins: the provider's verifying loader picks these by model id (P-2).
        let pins = sr_providers::models::tokenizer_pins_for(&MULTILINGUAL);
        assert_eq!(pins, &sr_providers::models::E5_SMALL_TOKENIZER);
        assert_eq!(
            (pins.hf_repo, pins.hf_revision),
            (E5_TOKENIZER_REPO, E5_TOKENIZER_REVISION)
        );
        let ours: Vec<(&str, &str)> = pins.files().iter().map(|a| (a.name, a.sha256)).collect();
        assert_eq!(ours, E5_TOKENIZER_FILES.to_vec());
        #[cfg(feature = "local")]
        {
            let d = std::path::Path::new(r"C:\d");
            let store = sr_providers::download::ModelStore::in_data_dir(d);
            assert_eq!(
                store.paths(&MULTILINGUAL, &E5_SMALL_FP16).tokenizer_dir,
                paths(d, &E5_SMALL_FP16).tokenizer_dir
            );
        }
        assert_eq!(file_for(E5_SMALL_FP32.sha256), Some(E5_SMALL_FP32));
        assert!(file_for("nope").is_none());
        let p = paths(std::path::Path::new("C:\\d"), &E5_SMALL_FP16);
        assert!(
            p.tokenizer_dir
                .ends_with("tok-intfloat--multilingual-e5-small@614241f622f5")
        );
    }
}
