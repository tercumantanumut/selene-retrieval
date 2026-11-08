//! LLM and reranker side of the provider interface (§2.5). The genai-backed remote providers are
//! in [`crate::remote`] (M3-P, feature `remote`); the cross-encoder reranker is engine work (M3).

use std::pin::Pin;

use futures::Stream;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sr_common::Result;
use sr_common::error::Purpose;
use tokio_util::sync::CancellationToken;

use crate::privacy::EgressCtx;

/// What an LLM provider supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Caps {
    pub streaming: bool,
    pub tools: bool,
    pub json_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub context_tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Egress purpose, checked by the PrivacyGate and written to `egress_audit`.
    pub purpose: Purpose,
    /// Collection / client for the audit row (M3-P).
    #[serde(default)]
    pub egress: EgressCtx,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub text: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

/// Streamed chat deltas. A stream is never retried after its first token.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;

/// Chat model provider (genai, M3: [`crate::remote::RemoteLlm`]). Object-safe.
pub trait LlmProvider: Send + Sync {
    /// Provider display name ("OpenAI"), used in error text.
    fn name(&self) -> &str;
    fn capabilities(&self) -> Caps;
    fn list_models<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<ModelInfo>>>;
    fn chat<'a>(
        &'a self,
        req: &'a ChatRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ChatResponse>>;
    /// Establishes the stream. `Resilient` may retry *this* call; never the stream's items.
    fn chat_stream<'a>(
        &'a self,
        req: &'a ChatRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ChatStream>>;
    /// Live key check for the wizard ("Your OpenAI key was rejected" on failure).
    fn validate_key<'a>(&'a self, cancel: &'a CancellationToken) -> BoxFuture<'a, Result<()>>;
}

/// Cross-encoder reranker (optional download, Advanced; High tier enables it in M3).
pub trait Reranker: Send + Sync {
    fn model_id(&self) -> &str;
    /// One score per doc, in input order.
    fn rerank<'a>(
        &'a self,
        query: &'a str,
        docs: &'a [&'a str],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<f32>>>;
}
