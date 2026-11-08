//! [`CliLlm`]: the chat side of "Claude (sign in)" / "Codex (sign in)" (M5-C).
//!
//! * Every call goes through [`Egress`] like any remote provider: consent for the purpose
//!   (page 22), the audit row, the rate limiter, cancel and the time limit.
//! * One CLI run per call ([`super::prompt::render`] on stdin). `chat_stream` holds the run until
//!   its first text inside the egress call, so a refused sign-in or a usage limit is an error of
//!   the call itself; afterwards the stream ends with `Cancelled` on cancel and `Timeout` when
//!   nothing arrives for the time limit. Cancel, timeout, an error and dropping the stream all
//!   drop the run, which kills the whole process tree (Job Object) and removes its temp folder.
//! * A run that reports a tool call is stopped at once (`ProviderRequest`): with the flags used
//!   it can't happen, and if a future CLI version changed that, nothing more is read from it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use tokio_util::sync::CancellationToken;

use super::discover::CliPath;
use super::process::{self, Running, WorkDir};
use super::{CliKind, claude, codex, prompt, status};
use crate::llm::{Caps, ChatRequest, ChatResponse, ChatStream, LlmProvider, ModelInfo};
use crate::privacy::PrivacyGate;
use crate::registry::ProviderSpec;
use crate::remote::{Egress, Limiter, RemoteSettings};

/// The shortest time limit for one CLI run: the CLI starts, signs in and only then calls the
/// model, and Codex prints its answer only when it is complete. The service's own overall limit
/// for `answer` / `enhance_prompt` (150 s) and the app's (180 s) still apply above this.
pub const MIN_RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// A model id safe to pass on the command line (also through `cmd.exe` for npm shims).
fn valid_model(m: &str) -> bool {
    !m.is_empty()
        && m.len() <= 100
        && !m.starts_with('-')
        && m.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '/' | '[' | ']' | '@')
        })
}

/// Chat through the user's own CLI.
#[derive(Clone)]
pub struct CliLlm {
    kind: CliKind,
    exe: CliPath,
    spec: ProviderSpec,
    egress: Egress,
    timeout: Duration,
    /// Codex: the tool lock was read back once and holds (see `codex::tools_locked`).
    codex_locked: Arc<AtomicBool>,
    /// Codex: run with the user's `config.toml` (after a refused run without it; module docs of
    /// `codex`).
    codex_user_config: Arc<AtomicBool>,
}

impl std::fmt::Debug for CliLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliLlm")
            .field("kind", &self.kind)
            .field("exe", &self.exe.path)
            .finish_non_exhaustive()
    }
}

/// What one run produced so far.
enum Item {
    Text(String),
    End {
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    },
}

/// Normalises the two CLIs' event lines into text pieces and the end.
struct Reader {
    kind: CliKind,
    run: Running,
    /// Some text was already handed out (Claude: deltas; later whole messages are repeats).
    emitted: bool,
    saw_delta: bool,
    ended: bool,
    warn: Option<Error>,
}

fn tool_stop(kind: CliKind) -> Error {
    tracing::warn!(target: "sr_providers::cli", provider = kind.stem(), "the CLI reported a tool call; run stopped");
    Error::new(ErrorCode::ProviderRequest)
        .with_provider(kind.label())
        .with_detail(
            "the program tried to use a tool, which Selene Retrieval doesn't allow; stopped",
        )
}

impl Reader {
    async fn next(&mut self) -> Result<Option<Item>> {
        loop {
            if self.ended {
                return Ok(None);
            }
            let Some(line) = self.run.next_line().await? else {
                return self.at_eof().await;
            };
            match self.kind {
                CliKind::Claude => match claude::parse_line(&line) {
                    claude::Event::Delta(t) => {
                        self.saw_delta = true;
                        if !t.is_empty() {
                            self.emitted = true;
                            return Ok(Some(Item::Text(t)));
                        }
                    }
                    claude::Event::Message(t) => {
                        if !self.saw_delta && !t.is_empty() {
                            self.emitted = true;
                            return Ok(Some(Item::Text(t)));
                        }
                    }
                    claude::Event::ToolUse => return Err(tool_stop(self.kind)),
                    claude::Event::Fail(e) => return Err(e),
                    claude::Event::Done {
                        text,
                        input_tokens,
                        output_tokens,
                    } => {
                        self.ended = true;
                        if !self.emitted
                            && let Some(t) = text.filter(|t| !t.is_empty())
                        {
                            self.emitted = true;
                            // The end follows on the next call (`ended` is set).
                            return Ok(Some(Item::Text(t)));
                        }
                        return Ok(Some(Item::End {
                            input_tokens,
                            output_tokens,
                        }));
                    }
                    claude::Event::Other => {}
                },
                CliKind::Codex => match codex::parse_line(&line) {
                    codex::Event::Message(t) => {
                        if !t.is_empty() {
                            let t = if self.emitted { format!("\n\n{t}") } else { t };
                            self.emitted = true;
                            return Ok(Some(Item::Text(t)));
                        }
                    }
                    codex::Event::ToolUse => return Err(tool_stop(self.kind)),
                    codex::Event::Fail(e) => return Err(e),
                    codex::Event::Warn(e) => self.warn = Some(e),
                    codex::Event::Done {
                        input_tokens,
                        output_tokens,
                    } => {
                        self.ended = true;
                        if !self.emitted {
                            return Err(self.warn.take().unwrap_or_else(|| no_answer(self.kind)));
                        }
                        return Ok(Some(Item::End {
                            input_tokens,
                            output_tokens,
                        }));
                    }
                    codex::Event::Other => {}
                },
            }
        }
    }

    /// stdout ended without an end event: the exit code and stderr decide.
    async fn at_eof(&mut self) -> Result<Option<Item>> {
        self.ended = true;
        let (code, stderr) = self.run.finish().await;
        if code == Some(0) && self.emitted {
            return Ok(Some(Item::End {
                input_tokens: None,
                output_tokens: None,
            }));
        }
        if let Some(w) = self.warn.take() {
            return Err(w);
        }
        if !stderr.trim().is_empty() {
            return Err(super::classify(self.kind, &stderr));
        }
        Err(no_answer(self.kind))
    }
}

fn no_answer(kind: CliKind) -> Error {
    Error::new(ErrorCode::ProviderUnavailable)
        .with_provider(kind.label())
        .with_detail("the program ended without an answer")
}

impl CliLlm {
    /// `spec` is the provider row (default model); `exe` the CLI found by
    /// [`super::discover::find`]. No process is started here.
    pub fn new(
        kind: CliKind,
        exe: CliPath,
        spec: ProviderSpec,
        gate: Arc<dyn PrivacyGate>,
        settings: &RemoteSettings,
    ) -> Self {
        let timeout = settings.attempt_timeout.max(MIN_RUN_TIMEOUT);
        let egress = Egress::new(
            spec.id.clone(),
            spec.display_name.clone(),
            gate,
            Arc::new(Limiter::new(settings.rate_limit_rpm)),
            timeout,
        );
        Self {
            kind,
            exe,
            spec,
            egress,
            timeout,
            codex_locked: Arc::new(AtomicBool::new(false)),
            codex_user_config: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn kind(&self) -> CliKind {
        self.kind
    }

    pub fn exe(&self) -> &CliPath {
        &self.exe
    }

    fn model_for(&self, req: &ChatRequest) -> Result<String> {
        let m = req.model.trim();
        let m = if m.is_empty() {
            self.spec.default_model.clone().unwrap_or_default()
        } else {
            m.to_string()
        };
        if !valid_model(&m) {
            return Err(Error::new(ErrorCode::ProviderRequest)
                .with_provider(self.kind.label())
                .with_detail("no usable model name"));
        }
        Ok(m)
    }

    /// Codex: read the feature lock back once (fail closed).
    async fn ensure_locked(&self) -> Result<()> {
        if self.kind != CliKind::Codex || self.codex_locked.load(Ordering::Acquire) {
            return Ok(());
        }
        let out = process::run(
            &self.exe,
            &codex::features_args(),
            None,
            status::STATUS_TIMEOUT,
            self.kind.label(),
        )
        .await?;
        if out.success() && codex::tools_locked(&out.stdout) {
            self.codex_locked.store(true, Ordering::Release);
            return Ok(());
        }
        tracing::warn!(target: "sr_providers::cli", "Codex tool lock not confirmed; not used");
        Err(Error::new(ErrorCode::ProviderRequest)
            .with_provider(self.kind.label())
            .with_detail(
                "this Codex version can't be limited to writing text; update Codex and try again",
            ))
    }

    fn args(&self, model: &str, user_config: bool) -> Result<(Vec<String>, Option<WorkDir>)> {
        match self.kind {
            CliKind::Claude => Ok((claude::chat_args(model), None)),
            CliKind::Codex => {
                // `-C` must name the folder; the run's own cwd is another empty folder.
                let d = WorkDir::new()?;
                let a = codex::exec_args(model, &d.path().to_string_lossy(), user_config);
                Ok((a, Some(d)))
            }
        }
    }

    /// Starts a run and reads up to its first text (or its end / failure).
    async fn open(
        &self,
        model: &str,
        text: &str,
    ) -> Result<(Reader, Option<Item>, Option<WorkDir>)> {
        let mut user_config = self.codex_user_config.load(Ordering::Acquire);
        loop {
            let (args, dir) = self.args(model, user_config)?;
            let run = Running::spawn(&self.exe, &args, text.to_string(), self.kind.label())?;
            let mut r = Reader {
                kind: self.kind,
                run,
                emitted: false,
                saw_delta: false,
                ended: false,
                warn: None,
            };
            match r.next().await {
                Ok(first) => return Ok((r, first, dir)),
                Err(e)
                    if self.kind == CliKind::Codex
                        && !user_config
                        && e.code == ErrorCode::ProviderAuth =>
                {
                    drop(r);
                    // Signed in after all? Then the login is where only the user's config points.
                    let st = status::check(self.kind, Some(&self.exe)).await;
                    if !st.authenticated {
                        return Err(e);
                    }
                    tracing::info!(target: "sr_providers::cli", "Codex: retrying with the user's config (overrides kept)");
                    self.codex_user_config.store(true, Ordering::Release);
                    user_config = true;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn chat_impl(
        &self,
        req: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse> {
        let model = self.model_for(req)?;
        self.ensure_locked().await?;
        let text = prompt::render(&req.messages);
        self.egress
            .call(
                req.purpose,
                text.len() as u64,
                self.kind.audit_host(),
                &req.egress,
                cancel,
                || async {
                    let (mut r, first, _dir) = self.open(&model, &text).await?;
                    let mut out = String::new();
                    let mut usage = (None, None);
                    let mut item = first;
                    loop {
                        match item {
                            Some(Item::Text(t)) => out.push_str(&t),
                            Some(Item::End {
                                input_tokens,
                                output_tokens,
                            }) => usage = (input_tokens, output_tokens),
                            None => break,
                        }
                        item = r.next().await?;
                    }
                    Ok(ChatResponse {
                        text: out,
                        input_tokens: usage.0,
                        output_tokens: usage.1,
                    })
                },
            )
            .await
    }

    async fn stream_impl(
        &self,
        req: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<ChatStream> {
        let model = self.model_for(req)?;
        self.ensure_locked().await?;
        let text = prompt::render(&req.messages);
        let (reader, first, dir) = self
            .egress
            .call(
                req.purpose,
                text.len() as u64,
                self.kind.audit_host(),
                &req.egress,
                cancel,
                || self.open(&model, &text),
            )
            .await?;
        Ok(guard(
            reader,
            first,
            dir,
            cancel.clone(),
            self.timeout,
            self.kind,
        ))
    }
}

struct StreamState {
    reader: Option<Reader>,
    first: Option<Item>,
    _dir: Option<WorkDir>,
    cancel: CancellationToken,
    idle: Duration,
    kind: CliKind,
}

/// Text pieces only; cancel, the idle limit and errors end the stream and kill the run.
fn guard(
    reader: Reader,
    first: Option<Item>,
    dir: Option<WorkDir>,
    cancel: CancellationToken,
    idle: Duration,
    kind: CliKind,
) -> ChatStream {
    let st = StreamState {
        reader: Some(reader),
        first,
        _dir: dir,
        cancel,
        idle,
        kind,
    };
    Box::pin(futures::stream::unfold(st, |mut st| async move {
        loop {
            let item = match st.first.take() {
                Some(i) => Ok(Some(i)),
                None => {
                    let r = st.reader.as_mut()?;
                    let cancel = st.cancel.clone();
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
                        n = tokio::time::timeout(st.idle, r.next()) => match n {
                            Ok(x) => x,
                            Err(_) => Err(Error::new(ErrorCode::Timeout)
                                .with_provider(st.kind.label())
                                .with_detail("the answer stopped arriving")),
                        },
                    }
                }
            };
            match item {
                Ok(Some(Item::Text(t))) => return Some((Ok(t), st)),
                Ok(Some(Item::End { .. })) => continue,
                Ok(None) => return None,
                Err(e) => {
                    // Kill the run now, not at the next poll.
                    st.reader = None;
                    return Some((Err(e), st));
                }
            }
        }
    }))
}

impl LlmProvider for CliLlm {
    fn name(&self) -> &str {
        &self.spec.display_name
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
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<ModelInfo>>> {
        Box::pin(async move { Ok(crate::remote::discovery::registry_models(&self.spec)) })
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
    /// "Check": the CLI's own status command; nothing is sent to the provider.
    fn validate_key<'a>(&'a self, _cancel: &'a CancellationToken) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let st = status::check(self.kind, Some(&self.exe)).await;
            if st.authenticated {
                Ok(())
            } else if !st.cli_found {
                Err(self.kind.not_installed())
            } else if let Some(code) = st.error {
                Err(Error::new(code).with_provider(self.kind.label()))
            } else {
                Err(self.kind.not_signed_in("the CLI reports no sign-in"))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_are_plain() {
        assert!(valid_model("claude-sonnet-4-6"));
        assert!(valid_model("gpt-5.4-low"));
        assert!(valid_model("opus[1m]"));
        assert!(!valid_model(""));
        assert!(!valid_model("--dangerously-skip-permissions"));
        assert!(!valid_model("a b"));
        assert!(!valid_model("x\"&calc"));
        assert!(!valid_model("%PATH%"));
    }

    /// Amendment 11: every model the two sign-in rows offer can be passed to the CLI.
    #[test]
    fn every_listed_model_is_usable() {
        for id in ["claudecode", "codex"] {
            let row = crate::registry::manifest().get(id);
            let models = row.map(|p| p.models.clone()).unwrap_or_default();
            assert!(models.len() > 2, "{id}: model list");
            for m in &models {
                assert!(valid_model(&m.id), "{id}: {}", m.id);
            }
            let d = row
                .and_then(|p| p.default_model.clone())
                .unwrap_or_default();
            assert!(valid_model(&d), "{id}: default {d}");
        }
    }
}
