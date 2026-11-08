//! [`DaemonService`]: what sr-daemon plugs into the pipe server (M1-C `PipeService`): the engine's
//! event hub, status snapshots, one rmcp session per `kind=mcp` connection, app-RPC dispatch and
//! drain.
//!
//! It works on an [`EngineSlot`], so the pipe can open before the engine has started (M2 tester
//! O1): events and status come from the shared bus at once ("Starting…"), calls that need the
//! engine wait for it briefly.

use rmcp::ServiceExt;
use serde_json::Value;
use sr_common::error::{ClientKind, Error};
use sr_common::events::{EngineEvent, Snapshot};
use sr_engine::EngineHandle;
use sr_engine::pipe::{EventHub, PipeService, SessionInfo};
use tokio::net::windows::named_pipe::NamedPipeServer;

use crate::server::McpServer;
use crate::slot::{EngineSlot, START_WAIT};

/// The daemon's pipe service.
#[derive(Clone)]
pub struct DaemonService {
    slot: EngineSlot,
}

impl DaemonService {
    /// For an engine that is already running.
    pub fn new(engine: EngineHandle) -> Self {
        Self {
            slot: EngineSlot::ready(engine),
        }
    }

    /// For an engine that may still be starting (`sr-daemon`).
    pub fn with_slot(slot: EngineSlot) -> Self {
        Self { slot }
    }

    pub fn slot(&self) -> &EngineSlot {
        &self.slot
    }

    /// The engine if it has started.
    pub fn engine(&self) -> Option<EngineHandle> {
        self.slot.get()
    }
}

impl DaemonService {
    /// R34 recovery-mode calls (`None` = not one of them; the caller falls through and gets
    /// `IndexMissing` from the slot).
    async fn recovery_call(
        &self,
        method: &str,
        params: Value,
        info: sr_common::api::IndexMissingInfo,
    ) -> Option<Result<Value, Error>> {
        use sr_common::api::methods as m4;
        match method {
            m4::INDEX_LOCATION_RECOVER => Some(self.recover(params).await),
            m4::INDEX_LOCATION_STATUS => Some(json(&sr_common::api::IndexLocationStatus {
                path: info.path.clone(),
                is_default: false,
                size_bytes: 0,
                free_bytes: 0,
                moving: None,
                missing: Some(info),
            })),
            m4::POLICY_STATUS => {
                let p = sr_common::config::policy::current();
                Some(json(&sr_common::api::PolicyStatus {
                    managed: p.is_managed(),
                    fields: p.managed_fields(),
                    issues: p.issues.clone(),
                    sources: p.sources.clone(),
                }))
            }
            _ => None,
        }
    }

    async fn recover(&self, params: Value) -> Result<Value, Error> {
        use sr_common::error::ErrorCode;
        let p: sr_common::api::RecoverParams = serde_json::from_value(params)
            .map_err(|e| Error::new(ErrorCode::InvalidInput).with_detail(e.to_string()))?;
        let anchor = self
            .slot
            .recovery_anchor()
            .ok_or_else(|| Error::new(ErrorCode::Internal).with_detail("not in recovery mode"))?
            .to_path_buf();
        let to =
            tokio::task::spawn_blocking(move || sr_engine::recover::recover(&anchor, p.choice))
                .await
                .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))??;
        tracing::info!(target: "sr_daemon", choice = ?p.choice, "search index recovered; restarting");
        // Let the reply leave first; the app re-attaches to a fresh service.
        let slot = self.slot.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            slot.request_restart();
        });
        Ok(serde_json::json!({ "restarting": true, "index_path": to.display().to_string() }))
    }
}

/// Serialise a reply (the `Internal` mapping in one place).
fn json<T: serde::Serialize>(v: &T) -> Result<Value, Error> {
    serde_json::to_value(v)
        .map_err(|e| Error::new(sr_common::error::ErrorCode::Internal).with_source(e))
}

/// Client kind from the bridge's `--client` id (display only).
pub fn client_kind(id: &str) -> ClientKind {
    let id = id.to_ascii_lowercase();
    if id.contains("claude") {
        ClientKind::ClaudeDesktop
    } else if id.contains("cursor") {
        ClientKind::Cursor
    } else if id.contains("vscode") || id.contains("code") {
        ClientKind::VsCode
    } else {
        ClientKind::Other
    }
}

impl PipeService for DaemonService {
    type Event = EngineEvent;

    fn events(&self) -> &EventHub<EngineEvent> {
        self.slot.bus().hub()
    }

    fn snapshot(&self) -> Snapshot<Value> {
        let s = self.slot.status();
        Snapshot {
            stream: s.stream,
            seq: s.seq,
            value: serde_json::to_value(&s.value).unwrap_or(Value::Null),
        }
    }

    async fn serve_mcp(&self, stream: NamedPipeServer, session: SessionInfo) {
        self.slot.bus().publish(EngineEvent::ClientConnected {
            client: client_kind(&session.handshake.client_id),
        });
        // M4-E (page 29): the app is recorded (first time, last used, open sessions) once the
        // engine is up; the session's scope is looked up per tool call.
        let canonical = sr_common::api::normalize_client_id(&session.handshake.client_id);
        let tracker = {
            let (slot, raw) = (self.slot.clone(), session.handshake.client_id.clone());
            tokio::spawn(async move {
                match slot.wait(std::time::Duration::from_secs(120)).await {
                    Ok(e) => Some((e.clone(), e.client_session_opened(&raw).await)),
                    Err(_) => None,
                }
            })
        };
        let server = McpServer::with_slot(self.slot.clone()).with_client(&canonical);
        match server.serve(stream).await {
            Ok(running) => {
                let _ = running.waiting().await;
            }
            Err(e) => {
                tracing::warn!(target: "sr_mcp::session", session = session.id, err = %e, "MCP session ended during initialize");
            }
        }
        // Don't hold the session open while the engine is still starting: the tracker is then
        // still waiting for it (nothing recorded yet) and is stopped. Once the engine runs, the
        // tracker has recorded the session (or is about to) and is awaited, so it is closed.
        if self.slot.get().is_none() {
            tracker.abort();
        }
        if let Ok(Some((engine, id))) = tracker.await {
            engine.client_session_closed(&id);
        }
    }

    async fn call(
        &self,
        method: String,
        params: Value,
        session: SessionInfo,
    ) -> Result<Value, Error> {
        // R31 (M4-E): consent, approvals, keys, scopes, HTTP and the index move only for our own
        // app (or the admin CLI) next to this daemon.
        if sr_common::api::is_privileged(&method) && !session.trusted_app {
            let peer = session
                .client_pid
                .and_then(sr_client::win::process_image_path)
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "unknown".into());
            tracing::warn!(target: "sr_mcp::rpc", method = %method, peer = %peer, "privileged call refused: caller is not Selene Retrieval's app");
            return Err(Error::new(sr_common::error::ErrorCode::PermissionDenied)
                .with_detail("this action is only available in the Selene Retrieval app"));
        }
        // R34 (M5-E): the moved index is missing; the recovery mode answers its own calls.
        if let Some(info) = self.slot.index_missing()
            && let Some(r) = self.recovery_call(&method, params.clone(), info).await
        {
            return r;
        }
        // Status answers at once while the engine starts (the app shows "Starting…" or the
        // index move's progress).
        if method == sr_engine::rpc::STATUS && self.slot.get().is_none() {
            return serde_json::to_value(self.slot.status())
                .map_err(|e| Error::new(sr_common::error::ErrorCode::Internal).with_source(e));
        }
        // Page 32: while a move runs before the engine starts, its status and cancel answer at
        // once (the engine isn't there yet).
        if self.slot.get().is_none()
            && let Some(p) = self.slot.move_control().progress()
        {
            use sr_common::api::methods as m4;
            if method == m4::INDEX_LOCATION_CANCEL {
                return Ok(serde_json::json!({ "cancelled": self.slot.move_control().cancel() }));
            }
            if method == m4::INDEX_LOCATION_STATUS {
                return Ok(serde_json::json!({
                    "path": p.from,
                    "is_default": false,
                    "size_bytes": p.total_bytes,
                    "free_bytes": 0,
                    "moving": p,
                }));
            }
        }
        let engine = self.slot.wait(START_WAIT).await?;
        engine.rpc_session(&method, params, session.id).await
    }

    fn app_session_closed(&self, session: &SessionInfo) {
        // Pages 27-28: closing the app stops its answer / enhance calls (nothing more is billed).
        if let Some(e) = self.slot.get() {
            let n = e.cancel_session_requests(session.id);
            if n > 0 {
                tracing::info!(target: "sr_mcp::rpc", requests = n, "app session closed; its requests were stopped");
            }
        }
    }

    async fn drain(&self) {
        // A drain requested during the start waits for it (bounded), then drains the engine.
        if let Ok(engine) = self.slot.wait(std::time::Duration::from_secs(120)).await {
            engine.drain().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sr_common::protocol::{ConnKind, Handshake};
    use sr_engine::EventBus;
    use sr_engine::pipe::{PipeService, SessionInfo};

    use super::*;

    fn session(trusted: bool) -> SessionInfo {
        SessionInfo {
            id: 1,
            client_pid: None,
            handshake: Handshake::new(ConnKind::App, "app"),
            trusted_app: trusted,
        }
    }

    /// R31: an app session that isn't our own sr-app is refused for every privileged method,
    /// before anything else happens; other methods are not refused for that reason.
    #[tokio::test]
    async fn privileged_methods_need_our_app() {
        let (slot, _set) = EngineSlot::starting(Arc::new(EventBus::default()));
        let svc = DaemonService::with_slot(slot);
        for m in sr_common::api::PRIVILEGED {
            let e = svc
                .call((*m).to_string(), serde_json::json!({}), session(false))
                .await
                .unwrap_err();
            assert_eq!(e.code, sr_common::error::ErrorCode::PermissionDenied, "{m}");
        }
        // Status still answers for anyone (the engine is starting here).
        let v = svc
            .call(
                sr_engine::rpc::STATUS.into(),
                serde_json::json!({}),
                session(false),
            )
            .await
            .unwrap();
        assert_eq!(v["value"]["host"], "starting");
    }
}
