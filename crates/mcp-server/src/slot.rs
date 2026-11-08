//! [`EngineSlot`]: the engine as seen by the pipe front ends while it may still be starting
//! (M2 tester O1).
//!
//! `sr-daemon` opens the pipe **before** the engine starts (model load, GPU detection), so a cold
//! spawn answers the handshake, MCP `initialize`, `tools/list`, status and event subscriptions
//! within about a second. Calls that need the engine (`context_search`, folder admin) wait for it
//! briefly ([`START_WAIT`]); if it is still starting after that, they get a plain, retryable
//! "still starting" answer. A failed start makes them `ServiceUnavailable`.
//!
//! The event bus exists before the engine (`EngineConfig::bus`), so subscribers attached during
//! the start see `HostState::Starting → Ready` and every later event in order.

use std::sync::Arc;
use std::time::Duration;

use sr_common::api::IndexMissingInfo;
use sr_common::error::{Error, ErrorCode};
use sr_common::events::{EngineStatus, HostState, Snapshot};
use sr_engine::{EngineHandle, EventBus};
use tokio::sync::watch;

/// How long a call that needs the engine waits for it to finish starting.
pub const START_WAIT: Duration = Duration::from_secs(25);

/// Plain summary while the engine starts (the same words as `EngineStatus::plain_summary`).
pub const STARTING_SUMMARY: &str = "Starting…";

#[derive(Clone)]
enum State {
    Starting,
    Ready(EngineHandle),
    Failed(ErrorCode),
    /// R34 (M5-E): the moved index is missing; the daemon waits in its recovery mode.
    IndexMissing(IndexMissingInfo),
}

/// Sets the outcome of the engine start (held by `sr-daemon`).
pub struct EngineSlotSetter {
    tx: watch::Sender<State>,
}

impl EngineSlotSetter {
    pub fn ready(&self, engine: EngineHandle) {
        let _ = self.tx.send(State::Ready(engine));
    }
    pub fn failed(&self, code: ErrorCode) {
        let _ = self.tx.send(State::Failed(code));
    }
    /// R34: the engine won't start because its moved index is missing (recovery mode).
    pub fn index_missing(&self, info: IndexMissingInfo) {
        let _ = self.tx.send(State::IndexMissing(info));
    }
}

/// The engine, once it has started; cheap to clone.
#[derive(Clone)]
pub struct EngineSlot {
    bus: Arc<EventBus>,
    rx: watch::Receiver<State>,
    /// `[integrations] mcp_add_folder_enabled` read from `settings.toml` before the engine
    /// starts, so `tools/list` needs no wait (the engine's own flag wins once it is ready).
    add_folder_hint: bool,
    /// `[integrations] mcp_sync_folder_enabled` from `settings.toml`, the same way.
    sync_folder_hint: bool,
    /// The provider tools per `settings.toml` while the engine starts (U17; the engine's own
    /// flags win once it is ready, and sessions get `tools/list_changed` if they differ).
    provider_hint: ToolFlags,
    /// M4-E (page 32): an index move that runs before the engine starts (progress, cancel).
    move_ctl: Arc<sr_engine::indexmove::MoveControl>,
    /// R34 (M5-E): the anchor data dir while in the recovery mode (`index_location.recover`).
    recovery_anchor: Option<std::path::PathBuf>,
    /// R34: fired after a recovery choice so the daemon drains and a fresh one starts.
    restart: Arc<tokio::sync::Notify>,
}

/// Which optional tools a session lists.
pub use sr_engine::llm::ToolFlags;

impl EngineSlot {
    /// A slot whose engine is still starting; publish on `bus` (pass it as `EngineConfig::bus`).
    pub fn starting(bus: Arc<EventBus>) -> (Self, EngineSlotSetter) {
        let (tx, rx) = watch::channel(State::Starting);
        (
            Self {
                bus,
                rx,
                add_folder_hint: false,
                sync_folder_hint: false,
                provider_hint: ToolFlags::default(),
                move_ctl: Arc::default(),
                recovery_anchor: None,
                restart: Arc::default(),
            },
            EngineSlotSetter { tx },
        )
    }

    /// A slot for an engine that is already running (tests, in-process use).
    pub fn ready(engine: EngineHandle) -> Self {
        let bus = engine.bus_arc();
        let (_tx, rx) = watch::channel(State::Ready(engine));
        Self {
            bus,
            rx,
            add_folder_hint: false,
            sync_folder_hint: false,
            provider_hint: ToolFlags::default(),
            move_ctl: Arc::default(),
            recovery_anchor: None,
            restart: Arc::default(),
        }
    }

    /// R34: the recovery mode's anchor data dir (where `index_location.recover` acts).
    pub fn with_recovery(mut self, anchor: std::path::PathBuf) -> Self {
        self.recovery_anchor = Some(anchor);
        self
    }

    /// R34: the anchor, when in the recovery mode.
    pub fn recovery_anchor(&self) -> Option<&std::path::Path> {
        self.recovery_anchor.as_deref()
    }

    /// R34: ask the daemon to drain and exit (a fresh start follows on the next attach).
    pub fn request_restart(&self) {
        self.restart.notify_one();
    }

    /// R34: resolves once [`Self::request_restart`] was called.
    pub async fn restart_requested(&self) {
        self.restart.notified().await;
    }

    /// The index-move control the daemon runs the move with (page 32).
    pub fn with_move_control(mut self, ctl: Arc<sr_engine::indexmove::MoveControl>) -> Self {
        self.move_ctl = ctl;
        self
    }

    /// The index-move control (progress while the move runs; cancel).
    pub fn move_control(&self) -> &Arc<sr_engine::indexmove::MoveControl> {
        &self.move_ctl
    }

    pub fn with_add_folder_hint(mut self, enabled: bool) -> Self {
        self.add_folder_hint = enabled;
        self
    }

    pub fn with_sync_folder_hint(mut self, enabled: bool) -> Self {
        self.sync_folder_hint = enabled;
        self
    }

    /// Whether to list `context_answer` before the engine is ready (from `settings.toml`).
    pub fn with_provider_hint(mut self, answer: bool) -> Self {
        self.provider_hint = ToolFlags {
            answer,
            ..ToolFlags::default()
        };
        self
    }

    /// Whether the MCP `add_folder` tool is turned on (off by default).
    pub fn add_folder_enabled(&self) -> bool {
        self.get()
            .map_or(self.add_folder_hint, |e| e.mcp_add_folder_enabled())
    }

    /// The optional tools to list now: the engine's flags once it is ready, else the hints.
    pub fn tool_flags(&self) -> ToolFlags {
        match self.get() {
            Some(e) => e.tool_flags(),
            None => ToolFlags {
                add_folder: self.add_folder_hint,
                sync_folder: self.sync_folder_hint,
                ..self.provider_hint
            },
        }
    }

    pub fn bus(&self) -> &Arc<EventBus> {
        &self.bus
    }

    /// The engine if it has started.
    pub fn get(&self) -> Option<EngineHandle> {
        match &*self.rx.borrow() {
            State::Ready(e) => Some(e.clone()),
            _ => None,
        }
    }

    /// `true` once the start finished, either way.
    pub fn settled(&self) -> bool {
        !matches!(&*self.rx.borrow(), State::Starting)
    }

    /// R34: the missing index, while the daemon is in its recovery mode.
    pub fn index_missing(&self) -> Option<IndexMissingInfo> {
        match &*self.rx.borrow() {
            State::IndexMissing(i) => Some(i.clone()),
            _ => None,
        }
    }

    /// Wait up to `max` for the engine. Still starting → [`starting_error`]; start failed →
    /// `ServiceUnavailable`.
    pub async fn wait(&self, max: Duration) -> Result<EngineHandle, Error> {
        let mut rx = self.rx.clone();
        let r = tokio::time::timeout(max, rx.wait_for(|s| !matches!(s, State::Starting))).await;
        let state = match r {
            Ok(Ok(s)) => s.clone(),
            // Sender dropped while starting: the daemon is going away.
            Ok(Err(_)) => State::Failed(ErrorCode::ServiceUnavailable),
            Err(_) => return Err(starting_error()),
        };
        match state {
            State::Ready(e) => Ok(e),
            State::Failed(code) => Err(Error::new(ErrorCode::ServiceUnavailable)
                .with_detail(format!("engine did not start: {code}"))),
            // R34: "Reconnect the drive, or move the index back", never "add the folder".
            State::IndexMissing(info) => Err(sr_engine::recover::missing_error(&info)),
            State::Starting => Err(starting_error()),
        }
    }

    /// Status snapshot: the engine's own, or a "Starting…" one taken on the shared bus.
    pub fn status(&self) -> Snapshot<EngineStatus> {
        match self.get() {
            Some(e) => e.status(),
            None => {
                let missing = self.index_missing();
                self.bus
                    .snapshot(|| starting_status(&self.move_ctl, missing.clone()))
            }
        }
    }

    /// [`Self::status`] from async code.
    pub async fn status_async(&self) -> Snapshot<EngineStatus> {
        match self.get() {
            Some(e) => e.status_async().await,
            None => self.status(),
        }
    }
}

fn starting_status(
    ctl: &sr_engine::indexmove::MoveControl,
    missing: Option<IndexMissingInfo>,
) -> EngineStatus {
    let mut s = EngineStatus {
        host: HostState::Starting,
        summary: STARTING_SUMMARY.into(),
        index_move: ctl.progress(),
        index_missing: missing,
        // The policy is read before the engine starts (the daemon installs it), so locked
        // fields show even in the recovery mode.
        managed: sr_common::config::policy::current().managed_fields(),
        ..EngineStatus::default()
    };
    // "Moving the search index (n%)" while a move runs (page 32), else "Starting…".
    s.summary = s.plain_summary();
    s
}

/// "Still starting": the existing `Busy` code (retryable, WaitAndRetry) with a 5 s hint; the
/// detail says why. No new error code (the table stays at 37).
pub fn starting_error() -> Error {
    Error::new(ErrorCode::Busy)
        .with_retry_after(5)
        .with_detail("still starting (loading the search model)")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Amendment 16: before the engine is ready, `sync_folder` is listed per the settings hint.
    #[test]
    fn sync_folder_hint_before_the_start() {
        let bus = Arc::new(EventBus::default());
        let (slot, _set) = EngineSlot::starting(bus.clone());
        assert!(!slot.tool_flags().sync_folder);
        let (slot, _set) = EngineSlot::starting(bus);
        let f = slot.with_sync_folder_hint(true).tool_flags();
        assert!(f.sync_folder && !f.add_folder && !f.answer, "{f:?}");
    }

    #[tokio::test]
    async fn starting_then_failed() {
        let (slot, set) = EngineSlot::starting(Arc::new(EventBus::default()));
        assert!(slot.get().is_none() && !slot.settled());
        let s = slot.status();
        assert_eq!(s.value.host, HostState::Starting);
        assert_eq!(s.value.summary, STARTING_SUMMARY);
        let e = slot.wait(Duration::from_millis(20)).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Busy);
        assert!(e.retryable);
        let s2 = slot.clone();
        let waiter = tokio::spawn(async move { s2.wait(Duration::from_secs(5)).await });
        set.failed(ErrorCode::Internal);
        let e = waiter.await.unwrap().unwrap_err();
        assert_eq!(e.code, ErrorCode::ServiceUnavailable);
        assert!(slot.settled());
    }

    /// R34 (M5-E): the recovery mode says what happened and what to do; calls get IndexMissing.
    #[tokio::test]
    async fn index_missing_mode() {
        let (slot, set) = EngineSlot::starting(Arc::new(EventBus::default()));
        let info = IndexMissingInfo {
            path: r"D:\SeleneRetrieval Index".into(),
            drive: Some("D:".into()),
            old_copy: None,
            folders_to_restore: 2,
        };
        set.index_missing(info.clone());
        assert!(slot.settled());
        assert_eq!(slot.index_missing(), Some(info.clone()));
        let s = slot.status();
        assert_eq!(s.value.summary, "Search index not found");
        assert_eq!(s.value.index_missing, Some(info));
        let e = slot.wait(Duration::from_millis(20)).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::IndexMissing);
        let t = e.user_message().agent_text();
        assert!(
            t.contains("Reconnect the drive") && !t.contains("add the folder"),
            "{t}"
        );
    }
}
