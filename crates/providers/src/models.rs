//! Model files: sha256 pins and the verifying loader.
//!
//! ONNX files are pinned in `sr_common::tier::files` (owned by sr-common, values from M0-E and
//! Amendment 3.4). This module adds the pins of the four tokenizer side files fastembed needs, and
//! loads an **already present** set of files (bundled Minimum, or a Standard/High download that M2's
//! downloader has finished): every file is read once into memory, its size and sha256 checked
//! against the pin, and only those verified bytes are handed to ONNX Runtime (no re-open between
//! check and use, so no TOCTOU window).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use sr_common::tier::{ModelFile, Tier, TierProfile};

/// A small, pinned, non-LFS file next to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuxFile {
    pub name: &'static str,
    pub sha256: &'static str,
}

/// Tokenizer files for one model id, at one HF repo + revision (fetched with the ONNX file in M2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenizerPins {
    pub model_id: &'static str,
    pub hf_repo: &'static str,
    pub hf_revision: &'static str,
    pub tokenizer_json: AuxFile,
    pub config_json: AuxFile,
    pub tokenizer_config_json: AuxFile,
    pub special_tokens_map_json: AuxFile,
}

impl TokenizerPins {
    pub const fn files(&self) -> [AuxFile; 4] {
        [
            self.tokenizer_json,
            self.config_json,
            self.tokenizer_config_json,
            self.special_tokens_map_json,
        ]
    }
}

const fn aux(name: &'static str, sha256: &'static str) -> AuxFile {
    AuxFile { name, sha256 }
}

/// All three tier models share the bert-base-uncased WordPiece vocab; `special_tokens_map.json`
/// is byte-identical across them. Hashes verified 2026-10-17 against the HF files at the pinned
/// revisions (downloaded with `curl …/resolve/<rev>/<file>` + `sha256sum`).
const SPECIAL_TOKENS_BERT: AuxFile = aux(
    "special_tokens_map.json",
    "5d5b662e421ea9fac075174bb0688ee0d9431699900b90662acd44b2a350503a",
);

/// Minimum (bundled): Qdrant/bge-small-en-v1.5-onnx-Q, same repo/revision as the ONNX file.
pub const BGE_SMALL_TOKENIZER: TokenizerPins = TokenizerPins {
    model_id: "BAAI/bge-small-en-v1.5",
    hf_repo: "Qdrant/bge-small-en-v1.5-onnx-Q",
    hf_revision: "aa8f8b060edb00e03bfdd08813a2949946c8ba55",
    tokenizer_json: aux(
        "tokenizer.json",
        "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    ),
    config_json: aux(
        "config.json",
        "13582bcf2effc85b7bf3d3f5532e686bc1c9ce86bb009d10f0ec33cbe92299dd",
    ),
    tokenizer_config_json: aux(
        "tokenizer_config.json",
        "0b29c7bfc889e53b36d9dd3e686dd4300f6525110eaa98c76a5dafceb2029f53",
    ),
    special_tokens_map_json: SPECIAL_TOKENS_BERT,
};

/// Standard: Snowflake/snowflake-arctic-embed-m-v1.5.
pub const ARCTIC_M15_TOKENIZER: TokenizerPins = TokenizerPins {
    model_id: "Snowflake/snowflake-arctic-embed-m-v1.5",
    hf_repo: "Snowflake/snowflake-arctic-embed-m-v1.5",
    hf_revision: "e58a8f756156a1293d763f17e3aae643474e9b8a",
    tokenizer_json: aux(
        "tokenizer.json",
        "91f1def9b9391fdabe028cd3f3fcc4efd34e5d1f08c3bf2de513ebb5911a1854",
    ),
    config_json: aux(
        "config.json",
        "d2dddc06af0aeeb7fd3289a3dee1ac76dce0c3df2467cadd26f5f731f8d7919b",
    ),
    tokenizer_config_json: aux(
        "tokenizer_config.json",
        "0e83e9d7206b3ade43f8f2aeef523cf5d5b4a25b67af21b273de21972c0f58b7",
    ),
    special_tokens_map_json: SPECIAL_TOKENS_BERT,
};

/// High: Snowflake/snowflake-arctic-embed-l.
pub const ARCTIC_L_TOKENIZER: TokenizerPins = TokenizerPins {
    model_id: "Snowflake/snowflake-arctic-embed-l",
    hf_repo: "Snowflake/snowflake-arctic-embed-l",
    hf_revision: "d8fb21ca8d905d2832ee8b96c894d3298964346b",
    tokenizer_json: aux(
        "tokenizer.json",
        "91f1def9b9391fdabe028cd3f3fcc4efd34e5d1f08c3bf2de513ebb5911a1854",
    ),
    config_json: aux(
        "config.json",
        "e2e01fe381e2a939eaed23dd37bdedeedfad37b034fd83c42916923f74b25a79",
    ),
    tokenizer_config_json: aux(
        "tokenizer_config.json",
        "0e83e9d7206b3ade43f8f2aeef523cf5d5b4a25b67af21b273de21972c0f58b7",
    ),
    special_tokens_map_json: SPECIAL_TOKENS_BERT,
};

/// Separate one-click profile (Amendment 1): intfloat/multilingual-e5-small, XLM-R sentencepiece
/// tokenizer at the official repo's pinned revision (M3-Q's profile `sr_engine::multilingual`;
/// M3-Q request P-2, fixed by the M3 tester). `tokenizer.json` is an LFS file (oid = sha256).
pub const E5_SMALL_TOKENIZER: TokenizerPins = TokenizerPins {
    model_id: "intfloat/multilingual-e5-small",
    hf_repo: "intfloat/multilingual-e5-small",
    hf_revision: "614241f622f53c4eeff9890bdc4f31cfecc418b3",
    tokenizer_json: aux(
        "tokenizer.json",
        "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
    ),
    config_json: aux(
        "config.json",
        "69137736cab8b8903a07fe8afaafdda25aac55415a12a55d1bffa9f581abf959",
    ),
    tokenizer_config_json: aux(
        "tokenizer_config.json",
        "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b",
    ),
    special_tokens_map_json: aux(
        "special_tokens_map.json",
        "d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7",
    ),
};

/// Every tokenizer pin set, looked up by model id.
pub const ALL_TOKENIZER_PINS: [&TokenizerPins; 4] = [
    &BGE_SMALL_TOKENIZER,
    &ARCTIC_M15_TOKENIZER,
    &ARCTIC_L_TOKENIZER,
    &E5_SMALL_TOKENIZER,
];

/// Tokenizer pins for a profile: by **model id** first (a profile outside the tier ladder, e.g.
/// the multilingual model, reuses a tier's resource class but has its own tokenizer), then by
/// tier for the tier models.
pub fn tokenizer_pins_for(tp: &TierProfile) -> &'static TokenizerPins {
    ALL_TOKENIZER_PINS
        .into_iter()
        .find(|p| p.model_id == tp.model_id)
        .unwrap_or_else(|| tokenizer_pins(tp.tier))
}

/// Tokenizer pins for a tier's model.
pub const fn tokenizer_pins(tier: Tier) -> &'static TokenizerPins {
    match tier {
        Tier::Minimum => &BGE_SMALL_TOKENIZER,
        Tier::Standard => &ARCTIC_M15_TOKENIZER,
        Tier::High => &ARCTIC_L_TOKENIZER,
    }
}

/// Directory name of the bundled Minimum model under `<app dir>\models\`.
pub const BUNDLED_MODEL_DIR: &str = "bge-small-en-v1.5";

/// Where one model's files are on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPaths {
    /// The ONNX file (absolute).
    pub onnx: PathBuf,
    /// Directory holding the four tokenizer files.
    pub tokenizer_dir: PathBuf,
}

impl ModelPaths {
    /// `dir\<basename of file.path>` + tokenizer files in `dir` (bundled layout and dev layout).
    pub fn in_dir(dir: &Path, file: &ModelFile) -> Self {
        let base = file.path.rsplit('/').next().unwrap_or(file.path);
        Self {
            onnx: dir.join(base),
            tokenizer_dir: dir.to_path_buf(),
        }
    }

    /// The bundled Minimum model: `<app_dir>\models\bge-small-en-v1.5\model_optimized.onnx`.
    pub fn bundled(app_dir: &Path) -> Self {
        Self::in_dir(
            &app_dir.join("models").join(BUNDLED_MODEL_DIR),
            &sr_common::tier::MINIMUM.gpu_file,
        )
    }
}

/// Model bytes that passed their pins. Only constructible through [`load_verified`].
#[cfg_attr(not(feature = "local"), allow(dead_code))] // bytes are consumed by `local`
pub struct VerifiedModel {
    pub tier: &'static TierProfile,
    pub file: ModelFile,
    pub paths: ModelPaths,
    pub(crate) onnx: Vec<u8>,
    pub(crate) tokenizer_json: Vec<u8>,
    pub(crate) config_json: Vec<u8>,
    pub(crate) tokenizer_config_json: Vec<u8>,
    pub(crate) special_tokens_map_json: Vec<u8>,
}

impl std::fmt::Debug for VerifiedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedModel")
            .field("model", &self.tier.model_id)
            .field("file", &self.file.path)
            .field("sha256", &self.file.sha256)
            .field("bytes", &self.onnx.len())
            .finish()
    }
}

/// Lowercase hex sha256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in d.iter() {
        s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        s.push(char::from_digit(u32::from(b & 0xf), 16).unwrap_or('0'));
    }
    s
}

/// The file must be one of the tier's pinned variants (refuses e.g. an int8 file the profile
/// does not list: 3.4 keeps int8 off until the eval harness clears it).
pub fn ensure_file_belongs(tier: &TierProfile, file: &ModelFile) -> Result<()> {
    if *file == tier.gpu_file || *file == tier.cpu_file {
        Ok(())
    } else {
        Err(
            Error::new(ErrorCode::ModelIncompatible).with_detail(format!(
                "{} is not a pinned variant of {}",
                file.path, tier.model_id
            )),
        )
    }
}

/// Read and verify `file` (+ tokenizer files) from `paths`. Errors:
/// * missing / unreadable file → the io error's code (`NotFound`, `PermissionDenied`, …), with
///   `model` set; the caller (bundled → `Internal`, broken install; download → re-fetch) decides.
/// * size or sha256 mismatch → `ModelIntegrity` ("didn't pass its safety check"). The loader never
///   deletes: deleting a bad download is the downloader's job (M2), and the bundled file lives in
///   the install dir.
pub fn load_verified(
    tier: &'static TierProfile,
    file: &ModelFile,
    paths: &ModelPaths,
) -> Result<VerifiedModel> {
    ensure_file_belongs(tier, file)?;
    let pins = tokenizer_pins_for(tier);
    let onnx = read_pinned(tier, &paths.onnx, Some(file.bytes), file.sha256)?;
    let rd = |a: AuxFile| read_pinned(tier, &paths.tokenizer_dir.join(a.name), None, a.sha256);
    Ok(VerifiedModel {
        tier,
        file: *file,
        paths: paths.clone(),
        onnx,
        tokenizer_json: rd(pins.tokenizer_json)?,
        config_json: rd(pins.config_json)?,
        tokenizer_config_json: rd(pins.tokenizer_config_json)?,
        special_tokens_map_json: rd(pins.special_tokens_map_json)?,
    })
}

fn read_pinned(
    tier: &TierProfile,
    path: &Path,
    want_len: Option<u64>,
    want_sha: &str,
) -> Result<Vec<u8>> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let model_err = |code: ErrorCode, detail: String| {
        let mut e = Error::new(code).with_detail(detail);
        e.ctx.model = Some(tier.model_id.to_owned());
        e
    };
    if let Some(want) = want_len {
        let len = std::fs::metadata(path)
            .map_err(|e| {
                let mut err = Error::from(e);
                err.ctx.model = Some(tier.model_id.to_owned());
                err.with_detail(format!("model file {name}"))
            })?
            .len();
        if len != want {
            return Err(model_err(
                ErrorCode::ModelIntegrity,
                format!("{name}: {len} bytes, pinned {want}"),
            ));
        }
    }
    let bytes = std::fs::read(path).map_err(|e| {
        let mut err = Error::from(e);
        err.ctx.model = Some(tier.model_id.to_owned());
        err.with_detail(format!("model file {name}"))
    })?;
    let got = sha256_hex(&bytes);
    if got != want_sha {
        return Err(model_err(
            ErrorCode::ModelIntegrity,
            format!("{name}: sha256 {got}, pinned {want_sha}"),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sr_common::tier::{HIGH, MINIMUM, STANDARD, files};

    #[test]
    fn sha256_hex_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn pins_are_well_formed_and_match_tiers() {
        for t in [&MINIMUM, &STANDARD, &HIGH] {
            let p = tokenizer_pins(t.tier);
            assert_eq!(p.model_id, t.model_id);
            for f in p.files() {
                assert_eq!(f.sha256.len(), 64, "{}", f.name);
                assert!(
                    f.sha256
                        .bytes()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
                );
            }
            // tokenizer files come from the same revision as the GPU file except for Minimum,
            // whose single bundled file is the Qdrant one.
            assert_eq!(p.hf_revision, t.gpu_file.hf_revision);
        }
    }

    #[test]
    fn pins_follow_the_model_id_before_the_tier() {
        // A profile with Minimum's resource class but another model (the multilingual one).
        let e5 = TierProfile {
            model_id: "intfloat/multilingual-e5-small",
            ..MINIMUM
        };
        assert_eq!(tokenizer_pins_for(&e5), &E5_SMALL_TOKENIZER);
        assert_eq!(tokenizer_pins_for(&MINIMUM), &BGE_SMALL_TOKENIZER);
        assert_eq!(tokenizer_pins_for(&STANDARD), &ARCTIC_M15_TOKENIZER);
        assert_eq!(tokenizer_pins_for(&HIGH), &ARCTIC_L_TOKENIZER);
    }

    #[test]
    fn bundled_paths() {
        let p = ModelPaths::bundled(Path::new(r"C:\App"));
        assert_eq!(
            p.onnx,
            Path::new(r"C:\App\models\bge-small-en-v1.5\model_optimized.onnx")
        );
        let s = ModelPaths::in_dir(Path::new(r"C:\m"), &files::ARCTIC_M15_FP16);
        assert_eq!(s.onnx, Path::new(r"C:\m\model_fp16.onnx"));
    }

    #[test]
    fn foreign_variant_refused() {
        let e = ensure_file_belongs(&MINIMUM, &files::BGE_SMALL_INT8).unwrap_err();
        assert_eq!(e.code, ErrorCode::ModelIncompatible);
        ensure_file_belongs(&MINIMUM, &MINIMUM.gpu_file).unwrap();
        ensure_file_belongs(&STANDARD, &files::ARCTIC_M15_FP32).unwrap();
    }

    #[test]
    fn tampered_and_missing_files() {
        let dir = std::env::temp_dir().join(format!("sr-m1d-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = ModelPaths::in_dir(&dir, &MINIMUM.gpu_file);
        // missing
        let e = load_verified(&MINIMUM, &MINIMUM.gpu_file, &paths).unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        assert_eq!(e.ctx.model.as_deref(), Some(MINIMUM.model_id));
        // wrong size
        std::fs::write(&paths.onnx, b"not a model").unwrap();
        let e = load_verified(&MINIMUM, &MINIMUM.gpu_file, &paths).unwrap_err();
        assert_eq!(e.code, ErrorCode::ModelIntegrity);
        // right size, wrong bytes (sparse-ish file of zeros)
        let f = std::fs::File::create(&paths.onnx).unwrap();
        f.set_len(MINIMUM.gpu_file.bytes).unwrap();
        drop(f);
        let e = load_verified(&MINIMUM, &MINIMUM.gpu_file, &paths).unwrap_err();
        assert_eq!(e.code, ErrorCode::ModelIntegrity);
        assert!(e.ctx.detail.as_deref().unwrap_or("").contains("sha256"));
        assert!(paths.onnx.exists(), "loader never deletes");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
