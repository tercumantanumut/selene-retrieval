//! Deterministic fake provider for tests in this crate and downstream (sr-core, sr-engine) that
//! must not load ONNX Runtime. Vectors are a pure function of the (prefixed) text, so equal texts
//! give equal vectors and different profiles give different spaces.

pub mod fake_http;

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::embed::{EmbedKind, EmbeddingProfile, EmbeddingProvider};

/// Fake `EmbeddingProvider`. Configure with the builder-style fields, then wrap in `Arc`.
pub struct HashEmbedder {
    pub profile: EmbeddingProfile,
    pub max_batch: usize,
    /// If set, every call waits for one permit (tests hold calls in flight with it).
    pub gate: Option<Arc<Semaphore>>,
    /// If set, every call fails with this code.
    pub fail: Option<ErrorCode>,
    /// Return vectors with this many dims instead of `profile.dims` (dims-mismatch tests).
    pub return_dims: Option<u32>,
    /// Every call's texts, in call order.
    pub calls: Mutex<Vec<Vec<String>>>,
}

impl HashEmbedder {
    pub fn new(model: &str, dims: u32) -> Self {
        Self {
            profile: EmbeddingProfile {
                provider: "test".into(),
                model: model.into(),
                model_revision: "test".into(),
                variant_sha256: crate::models::sha256_hex(model.as_bytes()),
                precision: "fp32".into(),
                dims,
                pooling: "cls".into(),
                query_prefix: "query: ".into(),
                doc_prefix: String::new(),
                tier: None,
                is_remote: false,
            },
            max_batch: 64,
            gate: None,
            fail: None,
            return_dims: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Deterministic unit vector for `text` in a `dims`-dim space seeded by `model`.
    pub fn vector(model: &str, text: &str, dims: u32) -> Vec<f32> {
        let mut h = fnv_seed(model.as_bytes(), text.as_bytes());
        let mut v: Vec<f32> = (0..dims)
            .map(|_| {
                h = h
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((h >> 33) as f32 / (1u64 << 31) as f32) - 0.5
            })
            .collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        v.iter_mut().for_each(|x| *x /= n);
        v
    }

    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

fn fnv_seed(a: &[u8], b: &[u8]) -> u64 {
    // FNV-1a over both inputs; good enough to seed the PRNG.
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in a.iter().chain([0xffu8].iter()).chain(b) {
        h ^= u64::from(x);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl EmbeddingProvider for HashEmbedder {
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
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(Error::new(ErrorCode::Cancelled));
            }
            if let Ok(mut c) = self.calls.lock() {
                c.push(texts.iter().map(|s| (*s).to_owned()).collect());
            }
            if let Some(g) = &self.gate {
                tokio::select! {
                    () = cancel.cancelled() => return Err(Error::new(ErrorCode::Cancelled)),
                    p = g.acquire() => { if let Ok(p) = p { p.forget(); } }
                }
            }
            if let Some(code) = self.fail {
                return Err(Error::new(code));
            }
            let dims = self.return_dims.unwrap_or(self.profile.dims);
            Ok(texts
                .iter()
                .map(|t| Self::vector(&self.profile.model, &self.profile.prepare(kind, t), dims))
                .collect())
        })
    }
}
