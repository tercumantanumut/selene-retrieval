//! Embedding side of the provider interface (docs/02-architecture.md §2.5).

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use sr_common::tier::{ModelFile, Tier, TierProfile};
use tokio_util::sync::CancellationToken;

/// Document or query side of an asymmetric embedding model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbedKind {
    Doc,
    Query,
}

/// `embedding_profiles` row: everything that makes two vectors comparable. Two providers whose
/// profiles have different fingerprints live in different embedding spaces and must never meet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingProfile {
    /// `"local"` or a remote provider id.
    pub provider: String,
    /// Model id, e.g. `BAAI/bge-small-en-v1.5`.
    pub model: String,
    /// Pinned HF revision of the variant file (empty for remote).
    pub model_revision: String,
    /// sha256 of the exact model file (sticky variant, §2.8). Empty for remote.
    pub variant_sha256: String,
    /// `fp32` | `fp16` | `int8` | `remote`.
    pub precision: String,
    pub dims: u32,
    /// `cls` or `mean`.
    pub pooling: String,
    pub query_prefix: String,
    pub doc_prefix: String,
    pub tier: Option<Tier>,
    pub is_remote: bool,
}

impl EmbeddingProfile {
    /// Profile of a local tier model run from `file` (one of `tier.gpu_file` / `tier.cpu_file`).
    pub fn local(tier: &TierProfile, file: &ModelFile) -> Self {
        Self {
            provider: "local".into(),
            model: tier.model_id.into(),
            model_revision: file.hf_revision.into(),
            variant_sha256: file.sha256.into(),
            precision: file.precision.as_str().into(),
            dims: tier.dims,
            pooling: tier.pooling.as_str().into(),
            query_prefix: tier.query_prefix.into(),
            doc_prefix: tier.doc_prefix.into(),
            tier: Some(tier.tier),
            is_remote: false,
        }
    }

    /// `embedding_profiles.fingerprint` =
    /// blake3(provider‖model‖revision‖variant_sha256‖dims‖pooling‖prefixes), length-prefixed.
    pub fn fingerprint(&self) -> String {
        let mut h = Vec::new();
        for f in [
            self.provider.as_str(),
            &self.model,
            &self.model_revision,
            &self.variant_sha256,
            &self.dims.to_string(),
            &self.pooling,
            &self.query_prefix,
            &self.doc_prefix,
        ] {
            h.extend_from_slice(&(f.len() as u64).to_le_bytes());
            h.extend_from_slice(f.as_bytes());
        }
        sr_common::ids::ContentHash::of_bytes(&h).to_hex()
    }

    /// Text actually fed to the model for `kind` (bge / arctic: query prefix, no doc prefix).
    pub fn prepare(&self, kind: EmbedKind, text: &str) -> String {
        let prefix = match kind {
            EmbedKind::Doc => &self.doc_prefix,
            EmbedKind::Query => &self.query_prefix,
        };
        if prefix.is_empty() {
            text.to_owned()
        } else {
            format!("{prefix}{text}")
        }
    }
}

/// What a vector index was built with (`vector_indexes.profile_id` → model, variant, dims). Queries
/// for that index must be embedded by a provider that matches it exactly (§2.4, Amendment 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexModel {
    pub model: String,
    pub variant_sha256: String,
    pub dims: u32,
}

impl From<&EmbeddingProfile> for IndexModel {
    fn from(p: &EmbeddingProfile) -> Self {
        Self {
            model: p.model.clone(),
            variant_sha256: p.variant_sha256.clone(),
            dims: p.dims,
        }
    }
}

/// `ModelIncompatible` (#37) with an internal detail (shown under Details only).
pub fn model_incompatible(detail: impl Into<String>) -> Error {
    Error::new(ErrorCode::ModelIncompatible).with_detail(detail)
}

/// Refuse to use `provider` for an index built by a different model, variant or dimension. The
/// vectors would be garbage rankings, never an error the user could act on, so this is checked
/// *before* embedding.
pub fn ensure_matches(provider: &EmbeddingProfile, index: &IndexModel) -> Result<()> {
    if provider.dims != index.dims {
        return Err(model_incompatible(format!(
            "dims mismatch: provider {} has {}, index has {}",
            provider.model, provider.dims, index.dims
        )));
    }
    if provider.model != index.model || provider.variant_sha256 != index.variant_sha256 {
        return Err(model_incompatible(format!(
            "model mismatch: provider {}@{} vs index {}@{}",
            provider.model,
            short(&provider.variant_sha256),
            index.model,
            short(&index.variant_sha256)
        )));
    }
    Ok(())
}

/// Every returned vector must have exactly `dims` finite components.
pub fn check_vectors(dims: u32, expected_len: usize, vecs: &[Vec<f32>]) -> Result<()> {
    if vecs.len() != expected_len {
        return Err(model_incompatible(format!(
            "provider returned {} vectors for {expected_len} texts",
            vecs.len()
        )));
    }
    if let Some(v) = vecs.iter().find(|v| v.len() != dims as usize) {
        return Err(model_incompatible(format!(
            "provider returned {} dims, profile says {dims}",
            v.len()
        )));
    }
    if vecs.iter().flatten().any(|x| !x.is_finite()) {
        return Err(model_incompatible("provider returned non-finite values"));
    }
    Ok(())
}

fn short(s: &str) -> &str {
    s.get(..12).unwrap_or(s)
}

/// Embedding provider: local ORT (bundled / downloaded tier models), later OpenAI-compatible,
/// Gemini and Ollama (M3). Object-safe so the engine can hold `Arc<dyn EmbeddingProvider>`.
///
/// Contract: vectors are L2-normalised, `len == dims()`, in input order. `embed_batch` is called
/// with at most [`max_batch`](Self::max_batch) texts. Cancellation is checked before work starts;
/// a model run that already started finishes (ORT runs are not interruptible through fastembed).
pub trait EmbeddingProvider: Send + Sync {
    fn profile(&self) -> &EmbeddingProfile;

    fn model_id(&self) -> &str {
        &self.profile().model
    }
    fn dims(&self) -> u32 {
        self.profile().dims
    }
    /// sha256 of the model file in use (sticky variant). Empty for remote providers.
    fn variant_sha256(&self) -> &str {
        &self.profile().variant_sha256
    }
    /// Largest batch per call (TierProfile batch for the active EP, VRAM-capped on the GPU).
    fn max_batch(&self) -> usize;

    fn embed_batch<'a>(
        &'a self,
        kind: EmbedKind,
        texts: &'a [&'a str],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>>>;

    /// Documents (no prefix for the tier models). Splits into `max_batch` slices.
    fn embed_documents<'a>(
        &'a self,
        texts: &'a [&'a str],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(texts.len());
            for part in texts.chunks(self.max_batch().max(1)) {
                if cancel.is_cancelled() {
                    return Err(Error::new(ErrorCode::Cancelled));
                }
                let v = self.embed_batch(EmbedKind::Doc, part, cancel).await?;
                check_vectors(self.dims(), part.len(), &v)?;
                out.extend(v);
            }
            Ok(out)
        })
    }

    /// One query (bge query prefix applied by the provider).
    fn embed_query<'a>(
        &'a self,
        text: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<f32>>> {
        Box::pin(async move {
            let one = [text];
            let mut v = self.embed_batch(EmbedKind::Query, &one, cancel).await?;
            check_vectors(self.dims(), 1, &v)?;
            v.pop().ok_or_else(|| model_incompatible("no query vector"))
        })
    }

    /// Query embedding for a specific index: refuses (`ModelIncompatible`) if this provider is not
    /// the one the index was built with, and re-checks the returned length.
    fn embed_query_for<'a>(
        &'a self,
        index: &'a IndexModel,
        text: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<f32>>> {
        Box::pin(async move {
            ensure_matches(self.profile(), index)?;
            let v = self.embed_query(text, cancel).await?;
            if v.len() != index.dims as usize {
                return Err(model_incompatible(format!(
                    "query vector has {} dims, index has {}",
                    v.len(),
                    index.dims
                )));
            }
            Ok(v)
        })
    }
}

impl<P: EmbeddingProvider + ?Sized> EmbeddingProvider for std::sync::Arc<P> {
    fn profile(&self) -> &EmbeddingProfile {
        (**self).profile()
    }
    fn max_batch(&self) -> usize {
        (**self).max_batch()
    }
    fn embed_batch<'a>(
        &'a self,
        kind: EmbedKind,
        texts: &'a [&'a str],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        (**self).embed_batch(kind, texts, cancel)
    }
}
