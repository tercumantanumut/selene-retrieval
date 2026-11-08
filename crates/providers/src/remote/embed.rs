//! Remote embeddings (`search_model = cloud:<provider>`, U16: "the text of your files will be
//! sent"). OpenAI-compatible `/embeddings` through genai: OpenRouter (registry default model),
//! Ollama and vLLM (model picked from the live list).
//!
//! * Purpose per call: `embed_doc` for indexing, `embed_query` for searches. Both need their own
//!   consent; every batch is audited with this embedder's [`EgressCtx`] (collection, client).
//! * Vectors come back in input order (sorted by the response `index`), L2-normalised, and are
//!   checked against the profile's dims (`ModelIncompatible` otherwise, §2.4).
//! * The profile is `precision = "remote"`, empty variant sha256, `is_remote = true`; its
//!   fingerprint separates it from every local space, so remote and local vectors never meet.

use futures::future::BoxFuture;
use sr_common::Result;
use sr_common::error::{Error, ErrorCode, Purpose};
use tokio_util::sync::CancellationToken;

use super::{ProviderHandle, errors};
use crate::embed::{EmbedKind, EmbeddingProfile, EmbeddingProvider, check_vectors};
use crate::privacy::EgressCtx;

/// Embedding side of a [`ProviderHandle`] for one model.
#[derive(Clone, Debug)]
pub struct RemoteEmbedder {
    h: ProviderHandle,
    profile: EmbeddingProfile,
    max_batch: usize,
    ctx: EgressCtx,
}

impl RemoteEmbedder {
    pub(crate) fn new(h: ProviderHandle, model: &str, dims: u32) -> Self {
        let max_batch = h.settings().embed_batch.max(1);
        let profile = EmbeddingProfile {
            provider: h.id().to_string(),
            model: model.to_string(),
            model_revision: String::new(),
            variant_sha256: String::new(),
            precision: "remote".into(),
            dims,
            pooling: "remote".into(),
            query_prefix: String::new(),
            doc_prefix: String::new(),
            tier: None,
            is_remote: true,
        };
        Self {
            h,
            profile,
            max_batch,
            ctx: EgressCtx::default(),
        }
    }

    /// Audit context (collection / client) for every call of this instance.
    pub fn with_ctx(mut self, ctx: EgressCtx) -> Self {
        self.ctx = ctx;
        self
    }

    /// Embeds one short fixed text to learn the model's dimensions (when the user picks a remote
    /// model). This **is** egress: it needs `embed_doc` consent and is audited.
    pub async fn probe_dims(
        h: &ProviderHandle,
        model: &str,
        cancel: &CancellationToken,
    ) -> Result<u32> {
        let (target, site) = h.target(model)?;
        let genai = &h.inner.genai;
        let res = h
            .inner
            .egress
            .call(
                Purpose::EmbedDoc,
                16,
                &site.host,
                &EgressCtx::default(),
                cancel,
                || async {
                    genai
                        .embed(target, "Selene Retrieval", None)
                        .await
                        .map_err(|e| errors::from_genai(&site.site(), e))
                },
            )
            .await?;
        let dims = res.first_vector().map_or(0, Vec::len);
        u32::try_from(dims).ok().filter(|d| *d > 0).ok_or_else(|| {
            Error::new(ErrorCode::ModelIncompatible)
                .with_provider(h.display_name().to_string())
                .with_detail(format!("{model} returned no vector"))
        })
    }

    async fn embed_impl(
        &self,
        kind: EmbedKind,
        texts: &[&str],
        cancel: &CancellationToken,
    ) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if cancel.is_cancelled() {
            return Err(Error::new(ErrorCode::Cancelled));
        }
        let purpose = match kind {
            EmbedKind::Doc => Purpose::EmbedDoc,
            EmbedKind::Query => Purpose::EmbedQuery,
        };
        let (target, site) = self.h.target(&self.profile.model)?;
        let inputs: Vec<String> = texts
            .iter()
            .map(|t| self.profile.prepare(kind, t))
            .collect();
        let bytes = inputs.iter().map(|s| s.len() as u64).sum();
        let genai = &self.h.inner.genai;
        let res = self
            .h
            .inner
            .egress
            .call(purpose, bytes, &site.host, &self.ctx, cancel, || async {
                genai
                    .embed_batch(target, inputs, None)
                    .await
                    .map_err(|e| errors::from_genai(&site.site(), e))
            })
            .await?;
        let mut embs = res.embeddings;
        embs.sort_by_key(|e| e.index);
        let out: Vec<Vec<f32>> = embs.into_iter().map(|e| normalize(e.vector)).collect();
        check_vectors(self.profile.dims, texts.len(), &out)?;
        Ok(out)
    }
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n.is_finite() && n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

impl EmbeddingProvider for RemoteEmbedder {
    fn profile(&self) -> &EmbeddingProfile {
        &self.profile
    }
    fn max_batch(&self) -> usize {
        self.max_batch
    }
    fn embed_batch<'a>(
        &'a self,
        kind: EmbedKind,
        texts: &'a [&'a str],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(self.embed_impl(kind, texts, cancel))
    }
}
