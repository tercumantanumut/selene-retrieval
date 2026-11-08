//! Remote chat (answers, enhance_prompt, iterate) through genai.
//!
//! * `chat`: one request, one [`Egress::call`](super::Egress::call) (consent for
//!   `req.purpose`, audit, timeout, cancel = abort).
//! * `chat_stream`: the request is sent and **held until the first event** inside the egress call,
//!   so an HTTP error (bad key, quota, 429) surfaces as an error of `chat_stream` itself and
//!   `Resilient` may retry it; once the first event has arrived the stream is never retried.
//!   The returned stream ends with `Cancelled` when `cancel` fires and with `Timeout` when no event
//!   arrives for the attempt timeout; in both cases, and when the caller drops it, the HTTP
//!   response is dropped and the connection closed at once (no orphan generation).

use futures::StreamExt;
use futures::future::BoxFuture;
use genai::chat::{
    ChatMessage as GMessage, ChatOptions, ChatRequest as GRequest, ChatStreamEvent, ReasoningEffort,
};
use sr_common::Result;
use sr_common::error::{Error, ErrorCode, Purpose};
use tokio_util::sync::CancellationToken;

use super::{ProviderHandle, SiteOwned, discovery, errors};
use crate::llm::{Caps, ChatRequest, ChatResponse, ChatStream, LlmProvider, ModelInfo, Role};

/// Chat side of a [`ProviderHandle`].
#[derive(Clone, Debug)]
pub struct RemoteLlm {
    h: ProviderHandle,
}

impl RemoteLlm {
    pub fn new(h: ProviderHandle) -> Self {
        Self { h }
    }
    pub fn handle(&self) -> &ProviderHandle {
        &self.h
    }

    /// `req.model`, or the provider's default (Selene registry) when empty.
    fn model_for(&self, req: &ChatRequest) -> Result<String> {
        let m = req.model.trim();
        if !m.is_empty() {
            return Ok(m.to_string());
        }
        self.h.spec().default_model.clone().ok_or_else(|| {
            Error::new(ErrorCode::ProviderRequest)
                .with_provider(self.h.display_name().to_string())
                .with_detail("no model chosen and the provider has no default")
        })
    }
}

fn to_genai(req: &ChatRequest) -> GRequest {
    let msgs = req
        .messages
        .iter()
        .map(|m| match m.role {
            Role::System => GMessage::system(m.content.clone()),
            Role::User => GMessage::user(m.content.clone()),
            Role::Assistant => GMessage::assistant(m.content.clone()),
        })
        .collect();
    GRequest::new(msgs)
}

/// genai options. On the OpenAI-compatible adapter an explicit `Budget(0)` reasoning effort is
/// set: genai then sends **no** `reasoning_effort` field (a budget is ignored there) and, more
/// importantly, does not read one out of the model name. Without it a live-listed model such as
/// `qwen/qwen3-max` would be sent as `qwen/qwen3` (genai strips `-low/-high/-max/…` suffixes).
fn options(req: &ChatRequest, openai_compat: bool) -> ChatOptions {
    let mut o = ChatOptions::default().with_capture_usage(true);
    if openai_compat {
        o = o.with_reasoning_effort(ReasoningEffort::Budget(0));
    }
    if let Some(t) = req.max_tokens {
        o = o.with_max_tokens(t);
    }
    if let Some(t) = req.temperature {
        o = o.with_temperature(f64::from(t));
    }
    o
}

/// Approximate request size for the audit row (message text bytes; exact JSON framing is
/// provider-specific and adds a few hundred bytes).
fn request_bytes(req: &ChatRequest) -> u64 {
    req.messages.iter().map(|m| m.content.len() as u64).sum()
}

fn to_u32(v: Option<i32>) -> Option<u32> {
    v.and_then(|x| u32::try_from(x).ok())
}

impl RemoteLlm {
    async fn chat_impl(
        &self,
        req: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse> {
        let model = self.model_for(req)?;
        let (target, site) = self.h.target(&model)?;
        let g = to_genai(req);
        let opts = options(req, self.h.is_openai_compat());
        let genai = &self.h.inner.genai;
        let res = self
            .h
            .inner
            .egress
            .call(
                req.purpose,
                request_bytes(req),
                &site.host,
                &req.egress,
                cancel,
                || async {
                    genai
                        .exec_chat(target, g, Some(&opts))
                        .await
                        .map_err(|e| errors::from_genai(&site.site(), e))
                },
            )
            .await?;
        Ok(ChatResponse {
            text: res.texts().concat(),
            input_tokens: to_u32(res.usage.prompt_tokens),
            output_tokens: to_u32(res.usage.completion_tokens),
        })
    }

    async fn stream_impl(
        &self,
        req: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<ChatStream> {
        let model = self.model_for(req)?;
        let (target, site) = self.h.target(&model)?;
        let g = to_genai(req);
        let opts = options(req, self.h.is_openai_compat());
        let genai = &self.h.inner.genai;
        let (first, rest) = self
            .h
            .inner
            .egress
            .call(
                req.purpose,
                request_bytes(req),
                &site.host,
                &req.egress,
                cancel,
                || async {
                    let resp = genai
                        .exec_chat_stream(target, g, Some(&opts))
                        .await
                        .map_err(|e| errors::from_genai(&site.site(), e))?;
                    let mut s = resp.stream;
                    loop {
                        match s.next().await {
                            Some(Ok(ChatStreamEvent::Start)) => continue,
                            Some(Ok(ChatStreamEvent::Chunk(c))) => {
                                return Ok((Some(c.content), Some(s)));
                            }
                            Some(Ok(ChatStreamEvent::End(_))) | None => return Ok((None, None)),
                            // Reasoning / tool / ping events: the request was accepted.
                            Some(Ok(_)) => return Ok((None, Some(s))),
                            Some(Err(e)) => return Err(errors::from_genai(&site.site(), e)),
                        }
                    }
                },
            )
            .await?;
        Ok(guard_stream(
            first,
            rest,
            cancel.clone(),
            self.h.inner.egress.attempt_timeout(),
            site,
        ))
    }
}

struct StreamState<S> {
    first: Option<String>,
    rest: Option<S>,
    cancel: CancellationToken,
    idle: std::time::Duration,
    site: SiteOwned,
}

enum Step {
    Text(String),
    Skip,
    End,
    Fail(Error),
}

/// Text deltas only; cancellation, idle timeout and errors end the stream (and drop the HTTP
/// response).
fn guard_stream<S>(
    first: Option<String>,
    rest: Option<S>,
    cancel: CancellationToken,
    idle: std::time::Duration,
    site: SiteOwned,
) -> ChatStream
where
    S: futures::Stream<Item = genai::Result<ChatStreamEvent>> + Send + Unpin + 'static,
{
    let st = StreamState {
        first,
        rest,
        cancel,
        idle,
        site,
    };
    Box::pin(futures::stream::unfold(st, |mut st| async move {
        loop {
            if let Some(t) = st.first.take()
                && !t.is_empty()
            {
                return Some((Ok(t), st));
            }
            let step = {
                let inner = st.rest.as_mut()?;
                let cancel = st.cancel.clone();
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => Step::Fail(Error::new(ErrorCode::Cancelled)),
                    r = tokio::time::timeout(st.idle, inner.next()) => match r {
                        Err(_) => Step::Fail(
                            Error::new(ErrorCode::Timeout)
                                .with_provider(st.site.provider.clone())
                                .with_detail("the answer stopped arriving"),
                        ),
                        Ok(None) | Ok(Some(Ok(ChatStreamEvent::End(_)))) => Step::End,
                        Ok(Some(Ok(ChatStreamEvent::Chunk(c)))) => Step::Text(c.content),
                        Ok(Some(Ok(_))) => Step::Skip,
                        Ok(Some(Err(e))) => Step::Fail(errors::from_genai(&st.site.site(), e)),
                    },
                }
            };
            match step {
                Step::Text(t) if !t.is_empty() => return Some((Ok(t), st)),
                Step::Text(_) | Step::Skip => continue,
                // Dropping `st` drops the response.
                Step::End => return None,
                Step::Fail(e) => {
                    // Dropping the inner stream closes the connection now, not at the next poll.
                    st.rest = None;
                    return Some((Err(e), st));
                }
            }
        }
    }))
}

impl LlmProvider for RemoteLlm {
    fn name(&self) -> &str {
        self.h.display_name()
    }
    fn capabilities(&self) -> Caps {
        Caps {
            streaming: true,
            tools: false,
            json_mode: false,
        }
    }
    fn list_models<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<ModelInfo>>> {
        Box::pin(discovery::list_models(&self.h, cancel))
    }
    fn chat<'a>(
        &'a self,
        req: &'a ChatRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ChatResponse>> {
        Box::pin(self.chat_impl(req, cancel))
    }
    fn chat_stream<'a>(
        &'a self,
        req: &'a ChatRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ChatStream>> {
        Box::pin(self.stream_impl(req, cancel))
    }
    fn validate_key<'a>(&'a self, cancel: &'a CancellationToken) -> BoxFuture<'a, Result<()>> {
        Box::pin(discovery::validate_key(&self.h, cancel))
    }
}

/// A one-token chat used as the key check for providers without a free authenticated endpoint.
pub(crate) async fn probe(h: &ProviderHandle, cancel: &CancellationToken) -> Result<()> {
    let model = h
        .spec()
        .utility_model
        .clone()
        .or_else(|| h.spec().default_model.clone())
        .unwrap_or_default();
    let req = ChatRequest {
        model,
        messages: vec![crate::llm::ChatMessage {
            role: Role::User,
            content: "ping".into(),
        }],
        max_tokens: Some(1),
        temperature: None,
        purpose: Purpose::ValidateKey,
        egress: Default::default(),
    };
    RemoteLlm::new(h.clone())
        .chat_impl(&req, cancel)
        .await
        .map(|_| ())
}
