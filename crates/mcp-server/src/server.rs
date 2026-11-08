//! The MCP server for one session (rmcp `ServerHandler` over the engine).
//!
//! * Tools: see [`crate::tools`]. Engine failures → `isError: true` with the one plain-message
//!   renderer (title + body + agent phrasing of the action) and `structuredContent`
//!   `{code, retryable, retry_after}`; bad arguments → JSON-RPC `-32602` (§2.5).
//! * Resources: `sr://status` (status snapshot + plain summary) and `sr://folders`; a session that
//!   subscribes gets `notifications/resources/updated` when status changes (push, P1.5).
//! * Progress: `context_search`, `sync_folder` and `context_answer` send
//!   `notifications/progress` when the request carries a progress token.
//! * Tool list (U17, R-UX29, Amendment 16): `context_search` and `list_folders` always;
//!   `sync_folder`, `add_folder` and `context_answer` only when usable. Every session watches the
//!   engine's events and sends `notifications/tools/list_changed` when that set changes (a
//!   setting, a provider added or removed, the engine becoming ready), so no client restart is
//!   needed. A call to a tool that is not listed gets the unknown-tool answer (`-32602`, the
//!   same as any unknown name, Amendment 16) and never runs.
//! * Cancellation: `notifications/cancelled` cancels the request's token; `context_answer` drops
//!   the provider request at once (nothing more is billed).
//! * Logging: tool name, duration and outcome only. Query, file and chunk text are never logged.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ProgressNotificationParam,
    ProgressToken, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResourceUpdatedNotificationParam, ServerCapabilities, ServerConfig,
    SubscribeRequestParams, Tool, ToolAnnotations, UnsubscribeRequestParams,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, Peer, RoleServer, ServerHandler};
use serde_json::{Value, json};
use sr_common::error::Error;
use sr_common::events::{EngineEvent, FolderStatus};
use sr_common::ids::FolderId;
use sr_engine::EngineHandle;
use sr_engine::clients::{NO_ACCESS_TEXT, Scope};

use crate::slot::{EngineSlot, START_WAIT, ToolFlags};
use crate::tools::{self, NoParams, SearchParams, SyncParams};

pub const RESOURCE_STATUS: &str = "sr://status";
pub const RESOURCE_FOLDERS: &str = "sr://folders";

/// Longest `sync_folder` wait when the caller asked for progress.
pub const SYNC_WAIT_MAX: Duration = Duration::from_secs(10 * 60);

/// Tool result for a failed call: `isError: true`, plain text with the agent phrasing of the one
/// next action, and `structuredContent` `{code, retryable, retry_after}` (docs/02 §2.5).
pub fn tool_error(e: &Error) -> CallToolResult {
    let mut r = CallToolResult::error(vec![ContentBlock::text(e.user_message().agent_text())]);
    r.structured_content = Some(e.mcp_structured());
    r
}

fn ok_structured(value: Value, text: String) -> CallToolResult {
    let mut r = CallToolResult::structured(value);
    r.content = vec![ContentBlock::text(text)];
    r
}

/// Set by the HTTP listener on a request whose bearer token checked out (M4-E): the HTTP tool's
/// client id. rmcp hands the request's `http::request::Parts` to every tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpClientId(pub String);

/// Tool result for an app whose scope names no folder (contract §4).
fn no_access() -> CallToolResult {
    ok_structured(
        json!({ "no_access": true, "text": NO_ACCESS_TEXT }),
        NO_ACCESS_TEXT.to_string(),
    )
}

/// Tool result while the engine is still starting (M2 tester O1): plain, retryable, and the
/// structured `Busy` code with a 5 s hint (no new error code).
fn still_starting(e: &Error) -> CallToolResult {
    if e.code != sr_common::error::ErrorCode::Busy {
        return tool_error(e);
    }
    let mut r = CallToolResult::error(vec![ContentBlock::text(
        "Selene Retrieval is still starting (loading its search model). Try again in a few seconds.",
    )]);
    r.structured_content = Some(e.mcp_structured());
    r
}

/// The answer to a tool this session doesn't list (JSON-RPC `-32602`): the same for a name that
/// never existed, a removed one and a switched-off `sync_folder` / `add_folder` (Amendment 16).
fn unknown_tool(name: &str) -> McpError {
    McpError::invalid_params(format!("unknown tool {name}"), None)
}

/// One MCP session.
#[derive(Clone)]
pub struct McpServer {
    slot: EngineSlot,
    /// Handshake `client_id` of the connection (approval requests name the asking app).
    client_id: String,
    subscribed_status: Arc<AtomicBool>,
    watcher_started: Arc<AtomicBool>,
    tools_watcher_started: Arc<AtomicBool>,
}

impl McpServer {
    pub fn new(engine: EngineHandle) -> Self {
        Self::with_slot(EngineSlot::ready(engine))
    }

    /// A session that may start before the engine is ready: `initialize`, `tools/list` and status
    /// answer at once; tools that need the engine wait for it ([`START_WAIT`]).
    pub fn with_slot(slot: EngineSlot) -> Self {
        Self {
            slot,
            client_id: "mcp".into(),
            subscribed_status: Arc::new(AtomicBool::new(false)),
            watcher_started: Arc::new(AtomicBool::new(false)),
            tools_watcher_started: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The connection's client id (canonical, e.g. "claude"; see
    /// `sr_common::api::normalize_client_id`).
    pub fn with_client(mut self, client_id: &str) -> Self {
        self.client_id = client_id.to_string();
        self
    }

    /// Who is calling: the HTTP tool whose token the listener checked, else this pipe session's
    /// app (M4-E).
    pub fn caller(&self, ext: &rmcp::model::Extensions) -> String {
        ext.get::<http::request::Parts>()
            .and_then(|p| p.extensions.get::<HttpClientId>())
            .map_or_else(|| self.client_id.clone(), |c| c.0.clone())
    }

    /// The caller's scope (and its "last used" time). A lookup failure grants nothing.
    async fn scope_of(&self, engine: &EngineHandle, caller: &str) -> Scope {
        engine.client_seen(caller).await;
        match engine.client_scope(caller).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "sr_mcp::scope", code = %e.code, "scope lookup failed; no folders shown");
                Scope::Folders(std::collections::HashSet::new())
            }
        }
    }

    /// The caller's scope without waiting for the engine (status / resources while starting:
    /// there are no folders yet).
    async fn scope_now(&self, caller: &str) -> Scope {
        match self.slot.get() {
            Some(e) => self.scope_of(&e, caller).await,
            None => Scope::All,
        }
    }

    /// The engine if it has started.
    pub fn engine(&self) -> Option<EngineHandle> {
        self.slot.get()
    }

    /// The engine, or the "still starting" tool result (boxed: `CallToolResult` is ~200 bytes).
    async fn eng(&self) -> Result<EngineHandle, Box<CallToolResult>> {
        self.slot
            .wait(START_WAIT)
            .await
            .map_err(|e| Box::new(still_starting(&e)))
    }

    fn tool_list(flags: ToolFlags) -> Vec<Tool> {
        let mut defs = tools::definitions();
        if flags.sync_folder {
            defs.push(tools::sync_folder_definition());
        }
        if flags.add_folder {
            defs.push(tools::add_folder_definition());
        }
        if flags.answer {
            defs.push(tools::answer_definition());
        }
        defs.into_iter()
            .map(|t| {
                Tool::new(t.name, t.description, Arc::new(t.schema))
                    .with_title(t.title)
                    .with_annotations(
                        ToolAnnotations::new()
                            .read_only(t.read_only)
                            .destructive(false)
                            .open_world(false),
                    )
            })
            .collect()
    }

    async fn progress(
        peer: &Peer<RoleServer>,
        token: &Option<ProgressToken>,
        done: f64,
        total: Option<f64>,
        msg: &str,
    ) {
        if let Some(t) = token {
            let mut p = ProgressNotificationParam::new(t.clone(), done);
            p.total = total;
            p.message = Some(msg.to_string());
            let _ = peer.notify_progress(p).await;
        }
    }

    async fn context_search(
        &self,
        p: SearchParams,
        token: Option<ProgressToken>,
        peer: &Peer<RoleServer>,
        caller: &str,
    ) -> Result<CallToolResult, McpError> {
        let debug = p.debug;
        let mut args = p
            .validate()
            .map_err(|m| McpError::invalid_params(m, None))?;
        Self::progress(peer, &token, 0.0, Some(1.0), "Searching").await;
        let query = args.query.clone();
        let engine = match self.eng().await {
            Ok(e) => e,
            Err(r) => return Ok(*r),
        };
        let scope = self.scope_of(&engine, caller).await;
        if scope.is_empty() {
            return Ok(no_access());
        }
        args.folder_ids = match scope.restrict(args.folder_ids) {
            Ok(v) => v,
            Err(e) => return Ok(tool_error(&e)),
        };
        let r = engine.search(args).await;
        Self::progress(peer, &token, 1.0, Some(1.0), "Done").await;
        Ok(match r {
            Ok(out) => ok_structured(
                tools::search_structured(&out, debug),
                tools::search_text(&out, &query),
            ),
            Err(e) => tool_error(&e),
        })
    }

    /// `sync_folder` (off by default, Amendment 16). `call_tool` only dispatches it while it is
    /// listed; the engine's own switch is checked once more after the start (the list before it
    /// came from `settings.toml`), so a switched-off check never runs.
    async fn sync_folder(
        &self,
        p: SyncParams,
        token: Option<ProgressToken>,
        peer: &Peer<RoleServer>,
        ct: &tokio_util::sync::CancellationToken,
        caller: &str,
    ) -> Result<CallToolResult, McpError> {
        let engine = match self.eng().await {
            Ok(e) => e,
            Err(r) => return Ok(*r),
        };
        if !engine.mcp_sync_folder_enabled() {
            return Err(unknown_tool(tools::SYNC_FOLDER));
        }
        Ok(self
            .sync_folder_on(&engine, p, token, peer, ct, caller)
            .await)
    }

    async fn sync_folder_on(
        &self,
        engine: &EngineHandle,
        p: SyncParams,
        token: Option<ProgressToken>,
        peer: &Peer<RoleServer>,
        ct: &tokio_util::sync::CancellationToken,
        caller: &str,
    ) -> CallToolResult {
        let scope = self.scope_of(engine, caller).await;
        if scope.is_empty() {
            return no_access();
        }
        // Subscribe before queueing so no progress event is missed.
        let mut rx = engine.bus().subscribe();
        let queued = match (&scope, p.folder_id) {
            (_, Some(id)) if !scope.allows(id) => {
                Err(Error::new(sr_common::error::ErrorCode::NotFound)
                    .with_detail("unknown folder id"))
            }
            (Scope::Folders(set), None) => {
                let mut all: Vec<FolderId> = set.iter().copied().collect();
                all.sort();
                let mut v = Vec::new();
                let mut err = None;
                for id in all {
                    match engine.sync_folder(Some(id)).await {
                        Ok(q) => v.extend(q),
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                }
                match err {
                    Some(e) => Err(e),
                    None => Ok(v),
                }
            }
            (_, f) => engine.sync_folder(f).await,
        };
        let ids = match queued {
            Ok(v) => v,
            Err(e) => return tool_error(&e),
        };
        if token.is_none() || ids.is_empty() {
            let text = if ids.is_empty() {
                "No folders have been added to Selene Retrieval yet.".to_string()
            } else {
                format!(
                    "Checking {} folder(s) for changes. Search results update as the files are indexed.",
                    ids.len()
                )
            };
            return ok_structured(json!({ "queued": ids, "waited": false }), text);
        }
        // With a progress token: stream progress until every folder settles (or the cap).
        let deadline = Instant::now() + SYNC_WAIT_MAX;
        let mut settled: std::collections::HashSet<_> = std::collections::HashSet::new();
        let mut started = false;
        let mut step = 0.0;
        loop {
            if settled.len() == ids.len() {
                break;
            }
            let ev = tokio::select! {
                () = ct.cancelled() => break,
                () = tokio::time::sleep_until(deadline.into()) => break,
                ev = rx.recv() => ev,
            };
            let Ok(ev) = ev else { continue };
            match ev.payload {
                EngineEvent::ScanProgress {
                    folder_id,
                    done,
                    total_est,
                } if ids.contains(&folder_id) => {
                    started = true;
                    step += 1.0;
                    let _ = (done, total_est);
                    Self::progress(peer, &token, step, None, "Looking for changes").await;
                }
                EngineEvent::IndexProgress {
                    folder_id,
                    files_done,
                    files_total,
                    ..
                } if ids.contains(&folder_id) => {
                    started = true;
                    step += 1.0;
                    Self::progress(
                        peer,
                        &token,
                        step,
                        None,
                        &format!("Indexed {files_done} of {files_total} files"),
                    )
                    .await;
                }
                EngineEvent::FolderStatusChanged {
                    folder_id, status, ..
                } if ids.contains(&folder_id) => {
                    if matches!(status, FolderStatus::Scanning | FolderStatus::Indexing) {
                        started = true;
                    }
                    if started
                        && matches!(
                            status,
                            FolderStatus::Synced
                                | FolderStatus::SyncedWithErrors
                                | FolderStatus::Partial
                                | FolderStatus::Error
                                | FolderStatus::Unavailable
                        )
                    {
                        settled.insert(folder_id);
                    }
                }
                _ => {}
            }
        }
        let mut status = engine.status_async().await;
        scoped_status(&mut status.value, &scope);
        ok_structured(
            json!({ "queued": ids, "waited": true, "summary": status.value.summary }),
            EngineHandle::status_text(&status.value),
        )
    }

    /// `add_folder` (off by default; `call_tool` only dispatches it while it is listed, the
    /// engine's own switch is checked once more after the start, and the engine refuses it again
    /// if the switch went off meanwhile): never adds anything itself. An impossible folder is
    /// refused at once; otherwise a pending approval is recorded and the agent is told to wait
    /// for the user (Q5: approval only in our own UI, 10-minute expiry).
    async fn add_folder(&self, path: &str, caller: &str) -> Result<CallToolResult, McpError> {
        let engine = match self.eng().await {
            Ok(e) => e,
            Err(r) => return Ok(*r),
        };
        if !engine.mcp_add_folder_enabled() {
            return Err(unknown_tool(tools::ADD_FOLDER));
        }
        engine.client_seen(caller).await;
        let kind = sr_common::api::client_kind_of(caller);
        Ok(match engine.request_add_folder(caller, kind, path).await {
            Ok(pending) => {
                let m = pending.user_message();
                ok_structured(
                    json!({
                        "status": "pending_approval",
                        "approval_id": pending.ctx.approval_id,
                        "text": m.agent_text(),
                    }),
                    // `agent_text` already ends with "Waiting for the user to approve this in
                    // Selene Retrieval." (M3 tester: the prefix repeated it).
                    m.agent_text(),
                )
            }
            Err(e) => tool_error(&e),
        })
    }

    /// `context_answer`: progress phases go out as `notifications/progress` (when the call has a
    /// token); `ct` (the client's cancellation) stops the provider request. Like every opt-in
    /// tool it answers as unknown while it isn't listed (checked again after the start).
    async fn context_answer(
        &self,
        p: tools::AnswerParams,
        token: Option<ProgressToken>,
        peer: &Peer<RoleServer>,
        ct: &tokio_util::sync::CancellationToken,
        caller: &str,
    ) -> Result<CallToolResult, McpError> {
        let (mode, debug) = (p.sources, p.debug);
        let mut args = p
            .validate(caller)
            .map_err(|m| McpError::invalid_params(m, None))?;
        let engine = match self.eng().await {
            Ok(e) => e,
            Err(r) => return Ok(*r),
        };
        if !self.slot.tool_flags().answer {
            return Err(unknown_tool(tools::CONTEXT_ANSWER));
        }
        let scope = self.scope_of(&engine, caller).await;
        if scope.is_empty() {
            return Ok(no_access());
        }
        args.folder_ids = match scope.restrict(args.folder_ids) {
            Ok(v) => v,
            Err(e) => return Ok(tool_error(&e)),
        };
        let question = args.question.clone();
        Self::progress(peer, &token, 0.0, None, "Searching your files…").await;
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<sr_engine::answers::AnswerPhase>();
        let report: Arc<dyn Fn(sr_engine::answers::AnswerPhase) + Send + Sync> =
            Arc::new(move |ph| {
                let _ = tx.send(ph);
            });
        let fwd = {
            let (peer, token) = (peer.clone(), token.clone());
            tokio::spawn(async move {
                let mut step = 0.0;
                while let Some(ph) = rx.recv().await {
                    step += 1.0;
                    let msg = match ph {
                        sr_engine::answers::AnswerPhase::ReadingFiles { .. } => {
                            "Reading your files…".to_string()
                        }
                        sr_engine::answers::AnswerPhase::Writing => {
                            "Writing the answer…".to_string()
                        }
                        sr_engine::answers::AnswerPhase::SearchingAgain { round, max } => {
                            format!("Searching again (round {round} of {max})…")
                        }
                    };
                    Self::progress(&peer, &token, step, None, &msg).await;
                }
            })
        };
        let r = engine.answer(args, report, ct).await;
        let _ = tokio::time::timeout(Duration::from_millis(200), fwd).await;
        Ok(match r {
            Ok(a) => ok_structured(
                tools::answer_structured(&a, mode, debug),
                tools::answer_text(&a, &question, mode),
            ),
            Err(e) => tool_error(&e),
        })
    }

    /// Re-announce the tool list whenever the usable set changes (U17, R-UX29).
    fn start_tools_watcher(&self, peer: Peer<RoleServer>) {
        if self.tools_watcher_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut rx = self.slot.bus().subscribe();
        let slot = self.slot.clone();
        tokio::spawn(async move {
            let mut last = slot.tool_flags();
            loop {
                let relevant = match rx.recv().await {
                    Ok(ev) => matches!(
                        ev.payload,
                        EngineEvent::SettingsChanged
                            | EngineEvent::HostState { .. }
                            | EngineEvent::ProviderHealth { .. }
                    ),
                    // Missed events: re-check.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                    Err(_) => break,
                };
                if !relevant {
                    continue;
                }
                // Settings writes come in bursts; let them settle first.
                tokio::time::sleep(Duration::from_millis(150)).await;
                let now = slot.tool_flags();
                if now != last {
                    last = now;
                    tracing::info!(target: "sr_mcp::session", answer = now.answer, add_folder = now.add_folder, sync_folder = now.sync_folder, "tool list changed");
                    if peer.notify_tool_list_changed().await.is_err() {
                        break;
                    }
                }
            }
        });
    }

    async fn list_folders_waiting(
        &self,
        caller: &str,
    ) -> Result<Vec<sr_engine::FolderInfo>, Error> {
        let engine = self.slot.wait(START_WAIT).await?;
        let scope = self.scope_of(&engine, caller).await;
        Ok(engine
            .list_folders()
            .await?
            .into_iter()
            .filter(|f| scope.allows(f.folder_id))
            .collect())
    }

    fn status_json(
        &self,
        s: &sr_common::events::Snapshot<sr_common::events::EngineStatus>,
    ) -> Value {
        json!({
            "summary": s.value.summary,
            "host": s.value.host,
            "queued_jobs": s.value.queued_jobs,
            "folders": s.value.folders,
            "catch_up": s.value.catch_up,
            "waiting_for_fullscreen": s.value.waiting_for_fullscreen,
            "notices": s.value.notices.iter().map(|n| json!({
                "code": n.code,
                "title": n.message.title,
                "body": n.message.body,
                "action": n.message.action.label(),
                "text": n.message.agent_text(),
            })).collect::<Vec<_>>(),
            "seq": s.seq,
        })
    }

    /// Watches the bus and sends `resources/updated` for `sr://status` while subscribed.
    fn start_status_watcher(&self, peer: Peer<RoleServer>) {
        if self.watcher_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut rx = self.slot.bus().subscribe();
        let flag = self.subscribed_status.clone();
        tokio::spawn(async move {
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                let relevant = matches!(
                    ev.payload,
                    EngineEvent::FolderStatusChanged { .. }
                        | EngineEvent::PurgeCompleted { .. }
                        | EngineEvent::HostState { .. }
                        | EngineEvent::IndexProgress { .. }
                        | EngineEvent::IndexSwapped { .. }
                        | EngineEvent::CatchUpProgress { .. }
                        | EngineEvent::Notice { .. }
                        | EngineEvent::GpuFullscreenWait { .. }
                        | EngineEvent::ConsentNeeded { .. }
                );
                if !relevant || !flag.load(Ordering::SeqCst) {
                    continue;
                }
                // Coalesce bursts (progress events) into one notification per second.
                tokio::time::sleep(Duration::from_secs(1)).await;
                while rx.try_recv().is_ok() {}
                if peer
                    .notify_resource_updated(ResourceUpdatedNotificationParam::new(RESOURCE_STATUS))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .enable_resources_subscribe()
                .build(),
        )
        .with_server_info(Implementation::new(
            "selene-retrieval",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(
            "Selene Retrieval searches the folders the user added in the Selene Retrieval app. \
             Use `context_search` to find passages in them by meaning and by exact words. \
             When `context_answer` is available, it writes a cited answer from the user's files. \
             Folders are added only by the user in the app.",
        )
    }

    async fn on_initialized(&self, context: rmcp::service::NotificationContext<RoleServer>) {
        self.start_tools_watcher(context.peer.clone());
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Also started on `notifications/initialized`; a client that skips it still gets updates.
        self.start_tools_watcher(_context.peer.clone());
        Ok(ListToolsResult::with_all_items(Self::tool_list(
            self.slot.tool_flags(),
        )))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        Self::tool_list(self.slot.tool_flags())
            .into_iter()
            .find(|t| t.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let t0 = Instant::now();
        let name = request.name.to_string();
        let token = context.meta.get_progress_token();
        let args = request.arguments;
        let bad = |m: String| McpError::invalid_params(m, None);
        // M4-E: every tool sees only the caller's scope (contract §4).
        let caller = self.caller(&context.extensions);
        // Amendment 16: the opt-in tools answer like unknown ones while they aren't listed (the
        // same flags as `tools/list`, so the list and the call never disagree).
        let flags = self.slot.tool_flags();
        let listed = match name.as_str() {
            tools::SYNC_FOLDER => flags.sync_folder,
            tools::ADD_FOLDER => flags.add_folder,
            tools::CONTEXT_ANSWER => flags.answer,
            _ => true,
        };
        let res: Result<CallToolResult, McpError> = match name.as_str() {
            tools::CONTEXT_SEARCH => {
                let p: SearchParams = tools::parse(args).map_err(bad)?;
                self.context_search(p, token, &context.peer, &caller).await
            }
            tools::LIST_FOLDERS => {
                let _: NoParams = tools::parse(args).map_err(bad)?;
                Ok(match self.list_folders_waiting(&caller).await {
                    Ok(f) => ok_structured(json!({ "folders": f }), tools::folders_text(&f)),
                    Err(e) => still_starting(&e),
                })
            }
            tools::SYNC_FOLDER if listed => {
                let p: SyncParams = tools::parse(args).map_err(bad)?;
                self.sync_folder(p, token, &context.peer, &context.ct, &caller)
                    .await
            }
            tools::ADD_FOLDER if listed => {
                let p: tools::AddFolderParams = tools::parse(args).map_err(bad)?;
                self.add_folder(&p.path, &caller).await
            }
            tools::CONTEXT_ANSWER if listed => {
                let p: tools::AnswerParams = tools::parse(args).map_err(bad)?;
                self.context_answer(p, token, &context.peer, &context.ct, &caller)
                    .await
            }
            other => Err(unknown_tool(other)),
        };
        let outcome = match &res {
            Ok(r) if r.is_error == Some(true) => "error",
            Ok(_) => "ok",
            Err(_) => "invalid",
        };
        tracing::info!(target: "sr_mcp::tool", tool = %name, ms = t0.elapsed().as_millis() as u64, outcome);
        res.map(CallToolResponse::from)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new(RESOURCE_STATUS, "status")
                .with_title("Selene Retrieval status")
                .with_description("Whether search is up to date, per folder, with a plain summary.")
                .with_mime_type("application/json"),
            Resource::new(RESOURCE_FOLDERS, "folders")
                .with_title("Folders")
                .with_description("The folders the user added, with ids and status.")
                .with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let caller = self.caller(&context.extensions);
        let text = match request.uri.as_str() {
            RESOURCE_STATUS => {
                let mut s = self.slot.status_async().await;
                let scope = self.scope_now(&caller).await;
                scoped_status(&mut s.value, &scope);
                self.status_json(&s).to_string()
            }
            RESOURCE_FOLDERS => match self.list_folders_waiting(&caller).await {
                Ok(f) => json!({ "folders": f }).to_string(),
                Err(e) => {
                    return Err(McpError::internal_error(
                        e.user_message().agent_text(),
                        Some(e.mcp_structured()),
                    ));
                }
            },
            other => {
                return Err(McpError::resource_not_found(
                    format!("unknown resource {other}"),
                    None,
                ));
            }
        };
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, request.uri.clone()).with_mime_type("application/json"),
        ])
        .into())
    }

    #[allow(deprecated)]
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        if request.uri == RESOURCE_STATUS {
            self.subscribed_status.store(true, Ordering::SeqCst);
            self.start_status_watcher(context.peer.clone());
            Ok(())
        } else if request.uri == RESOURCE_FOLDERS {
            Ok(())
        } else {
            Err(McpError::resource_not_found(
                format!("unknown resource {}", request.uri),
                None,
            ))
        }
    }

    #[allow(deprecated)]
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        if request.uri == RESOURCE_STATUS {
            self.subscribed_status.store(false, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// A status snapshot limited to the caller's scope: folder lines outside it are dropped and the
/// plain summary is recomputed over the rest (M4-E, contract §4).
pub fn scoped_status(s: &mut sr_common::events::EngineStatus, scope: &Scope) {
    if matches!(scope, Scope::All) {
        return;
    }
    s.folders.retain(|f| scope.allows(f.folder_id));
    s.summary = s.plain_summary();
}

#[cfg(test)]
mod scope_tests {
    use super::*;
    use sr_common::events::{EngineStatus, FolderStatusSummary, HostState};

    #[test]
    fn status_is_limited_to_the_scope() {
        let (a, b) = (FolderId::new(), FolderId::new());
        let line = |id, done, total| FolderStatusSummary {
            folder_id: id,
            display_name: "x".into(),
            status: FolderStatus::Indexing,
            files_indexed: done,
            files_total: total,
            ..FolderStatusSummary::default()
        };
        let mut s = EngineStatus {
            host: HostState::Ready,
            folders: vec![line(a, 1, 1), line(b, 1, 9)],
            ..EngineStatus::default()
        };
        s.summary = s.plain_summary();
        assert_eq!(s.summary, "Indexing 2 of 10 files");
        let mut all = s.clone();
        scoped_status(&mut all, &Scope::All);
        assert_eq!(all, s);
        scoped_status(&mut s, &Scope::Folders([a].into_iter().collect()));
        assert_eq!(s.folders.len(), 1);
        assert_eq!(
            s.summary, "Up to date",
            "the other folder's progress isn't shown"
        );
        let mut none = s.clone();
        scoped_status(&mut none, &Scope::Folders(std::collections::HashSet::new()));
        assert!(none.folders.is_empty());
    }

    #[test]
    fn no_access_is_plain_text() {
        let r = no_access();
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["structuredContent"]["no_access"], true);
        assert!(
            v["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("Connected apps")
        );
    }
}

#[cfg(test)]
mod tool_list_tests {
    use super::*;

    fn names(flags: ToolFlags) -> Vec<String> {
        McpServer::tool_list(flags)
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    /// Amendment 16: two tools by default; each opt-in tool only with its flag.
    #[test]
    fn default_list_and_opt_in_tools() {
        assert_eq!(
            names(ToolFlags::default()),
            ["context_search", "list_folders"]
        );
        let answer = ToolFlags {
            answer: true,
            ..ToolFlags::default()
        };
        assert_eq!(
            names(answer),
            ["context_search", "list_folders", "context_answer"]
        );
        let sync = ToolFlags {
            sync_folder: true,
            ..ToolFlags::default()
        };
        assert_eq!(
            names(sync),
            ["context_search", "list_folders", "sync_folder"]
        );
        let all = ToolFlags {
            add_folder: true,
            sync_folder: true,
            answer: true,
        };
        assert_eq!(
            names(all),
            [
                "context_search",
                "list_folders",
                "sync_folder",
                "add_folder",
                "context_answer"
            ]
        );
    }

    #[test]
    fn instructions_name_only_the_current_tools() {
        let (slot, _set) = EngineSlot::starting(Arc::new(sr_engine::EventBus::default()));
        let info = McpServer::with_slot(slot).get_info();
        let text = info.instructions.unwrap_or_default();
        assert!(text.contains("`context_search`") && text.contains("`context_answer`"));
        for gone in [
            "`search`",
            "`answer`",
            "read_file",
            "grep",
            "get_status",
            "enhance_prompt",
        ] {
            assert!(!text.contains(gone), "{gone}: {text}");
        }
    }
}
