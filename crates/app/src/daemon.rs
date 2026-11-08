//! Link to the background service (`sr-daemon`) through `sr-client`. The app is a client only:
//! it never hosts the engine (02 §2.1). Attaching launches the service when it isn't running.
//!
//! * Requests: one `AppClient` connection, re-attached once on `ServiceUnavailable`, each call
//!   bounded by [`REQUEST_TIMEOUT`].
//! * Status: a resuming [`Subscription`] (snapshot, then pushed events). After a service restart
//!   it re-attaches and starts again from the new run's snapshot (task 5). Nothing is polled; the
//!   only timer is the back-off between failed re-attach attempts.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use sr_client::{AppClient, AttachOptions, SubItem, Subscription};
use sr_common::error::{Error, ErrorCode};
use sr_common::protocol::ConnKind;
use tokio::sync::{Notify, broadcast, watch};

use crate::dto::{AppView, ServiceSignal};
use crate::state::ViewState;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// [Ask] / [Improve prompt] (`answer`, `enhance_prompt`): the service bounds them itself (overall
/// 150 s, Amendment 7.7), so the app waits a little longer than that. If even this runs out, the
/// app cancels the request (`requests.cancel`) so the provider isn't called and billed for an
/// answer nobody will see (M4 tester: the 30 s default cut long answers off and left them running).
pub const LLM_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// Time limit for one app-RPC call.
pub fn timeout_for(method: &str) -> Duration {
    match method {
        method::ANSWER | method::ENHANCE_PROMPT => LLM_REQUEST_TIMEOUT,
        _ => REQUEST_TIMEOUT,
    }
}
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// App-RPC method names (mirror of `sr_engine::rpc`; the app must not link the engine).
pub mod method {
    pub const DIAGNOSTICS: &str = "engine.diagnostics";
    pub const FOLDERS_LIST: &str = "folders.list";
    pub const FOLDERS_ADD: &str = "folders.add";
    pub const FOLDERS_REMOVE: &str = "folders.remove";
    pub const FOLDERS_SYNC: &str = "folders.sync";
    pub const FOLDERS_ESTIMATE: &str = "folders.estimate";
    pub const FOLDERS_PAUSE: &str = "folders.pause";
    pub const FOLDERS_RESUME: &str = "folders.resume";
    pub const APPROVALS_LIST: &str = "approvals.list";
    pub const APPROVALS_APPROVE: &str = "approvals.approve";
    pub const APPROVALS_DENY: &str = "approvals.deny";
    pub const NOTICES_DISMISS: &str = "notices.dismiss";
    pub const SEARCH_MODEL_STATUS: &str = "search_model.status";
    pub const SEARCH_MODEL_USE_RECOMMENDED: &str = "search_model.use_recommended";
    pub const SEARCH_MODEL_CANCEL: &str = "search_model.cancel";
    pub const SEARCH_MODEL_CHOOSE: &str = "search_model.choose";
    pub const SETTINGS_ADVANCED: &str = "settings.advanced";
    pub const SETTINGS_UPDATE_ADVANCED: &str = "settings.update_advanced";
    // M3 (pages 20–23). Requested from M3-Q / M3-P in docs/research/m3-u-progress.md; a service
    // without them answers "unknown method" (InvalidInput), shown as the plain error.
    pub const PROVIDERS_STATUS: &str = "providers.status";
    pub const PROVIDERS_SAVE: &str = "providers.save";
    pub const PROVIDERS_CHECK: &str = "providers.check";
    pub const PROVIDERS_SET_MODEL: &str = "providers.set_model";
    pub const PROVIDERS_REMOVE: &str = "providers.remove";
    pub const CONSENT_LIST: &str = "consent.list";
    pub const CONSENT_SET: &str = "consent.set";
    pub const SETTINGS_ANSWERS: &str = "settings.answers";
    pub const SETTINGS_UPDATE_ANSWERS: &str = "settings.update_answers";
    // M3 engine calls the M4 pages use (pages 26–28).
    pub const SEARCH: &str = "search";
    pub const ANSWER: &str = "answer";
    pub const ENHANCE_PROMPT: &str = "enhance_prompt";
    pub const FOLDERS_CANCEL: &str = "folders.cancel";
    // M4: names from the shared contract (docs/research/m4-rpc-contract.md); M5-C's
    // `cli_signin.{status,start}` (privileged, R31) come from there too (`cli_signin.rs`).
    pub use sr_common::api::methods::*;
}

/// Request/response access to the service; faked in unit tests.
pub trait Rpc: Send + Sync {
    fn call(
        &self,
        method: &str,
        params: Value,
    ) -> impl Future<Output = Result<Value, Error>> + Send;
}

pub struct DaemonLink {
    opts: AttachOptions,
    client: tokio::sync::Mutex<Option<Arc<AppClient>>>,
    /// "Retry" in the UI wakes the re-attach loop early.
    pub retry: Notify,
    info: Mutex<Option<(u32, u16)>>,
}

impl DaemonLink {
    pub fn new() -> Self {
        Self::with_options(AttachOptions::new(ConnKind::App, "app"))
    }

    pub fn with_options(opts: AttachOptions) -> Self {
        Self {
            opts,
            client: tokio::sync::Mutex::new(None),
            retry: Notify::new(),
            info: Mutex::new(None),
        }
    }

    /// `(pid, protocol)` of the service last attached to (diagnostics).
    pub fn info(&self) -> Option<(u32, u16)> {
        *self
            .info
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn client(&self) -> Result<Arc<AppClient>, Error> {
        let mut g = self.client.lock().await;
        if let Some(c) = g.as_ref() {
            return Ok(c.clone());
        }
        let c = Arc::new(AppClient::connect(&self.opts).await?);
        *self
            .info
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((c.daemon_pid, c.daemon_proto));
        *g = Some(c.clone());
        Ok(c)
    }

    /// Drop the request connection (after a restart); the next call re-attaches.
    pub async fn reset(&self) {
        *self.client.lock().await = None;
    }

    /// Updater path (02 §2.2): ask the service to finish its current batch, commit and exit
    /// before the installer replaces the files.
    pub async fn shutdown_drain(&self) -> Result<(), Error> {
        // Connect-only: never launch the service just to stop it.
        let existing = self.client.lock().await.clone();
        let c = match existing {
            Some(c) => c,
            None => {
                let mut o = self.opts.clone();
                o.launch = false;
                match AppClient::connect(&o).await {
                    Ok(c) => Arc::new(c),
                    Err(e) if e.code == ErrorCode::ServiceUnavailable => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
        };
        let r = tokio::time::timeout(REQUEST_TIMEOUT, c.shutdown(true))
            .await
            .map_err(|_| Error::new(ErrorCode::Timeout))?;
        self.reset().await;
        match r {
            // The service may close the pipe before answering: that is a successful drain.
            Err(e) if e.code == ErrorCode::ServiceUnavailable => Ok(()),
            other => other,
        }
    }
}

impl Default for DaemonLink {
    fn default() -> Self {
        Self::new()
    }
}

impl Rpc for DaemonLink {
    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let c = self.client().await?;
            match tokio::time::timeout(timeout_for(method), c.request(method, params.clone())).await
            {
                Err(_) => {
                    // Dropping the call doesn't stop the service's work: cancel it by id.
                    if let Some(id) = params.get("request_id").and_then(Value::as_str) {
                        let cancel =
                            c.request(method::REQUESTS_CANCEL, json!({ "request_id": id }));
                        let _ = tokio::time::timeout(Duration::from_secs(5), cancel).await;
                    }
                    return Err(Error::new(ErrorCode::Timeout));
                }
                Ok(Err(e)) if e.code == ErrorCode::ServiceUnavailable && attempt == 1 => {
                    self.reset().await;
                }
                Ok(r) => return r,
            }
        }
    }
}

/// Shared view state + the channel that pushes it to the UI/tray, and the M4 signal channel
/// (`ServiceSignal`: "re-read X" and one-off results, forwarded to the UI as events).
#[derive(Clone)]
pub struct Shared {
    pub state: Arc<Mutex<ViewState>>,
    pub tx: Arc<watch::Sender<AppView>>,
    pub signals: broadcast::Sender<ServiceSignal>,
}

impl Shared {
    pub fn new() -> (Self, watch::Receiver<AppView>) {
        let (tx, rx) = watch::channel(AppView::default());
        let (signals, _) = broadcast::channel(64);
        (
            Self {
                state: Arc::new(Mutex::new(ViewState::new())),
                tx: Arc::new(tx),
                signals,
            },
            rx,
        )
    }

    /// Forward one signal (dropped when no UI listens, which is fine: the UI re-reads on open).
    pub fn signal(&self, s: ServiceSignal) {
        let _ = self.signals.send(s);
    }

    /// Mutate the state and push the new view if it changed.
    pub fn update(&self, f: impl FnOnce(&mut ViewState) -> bool) {
        let view = {
            let mut s = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !f(&mut s) {
                return;
            }
            s.view()
        };
        self.tx.send_if_modified(|v| {
            if *v == view {
                false
            } else {
                *v = view;
                true
            }
        });
    }

    pub fn view(&self) -> AppView {
        self.tx.borrow().clone()
    }

    pub fn gpu(&self) -> Option<bool> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tier()
            .map(|t| t.gpu)
    }
}

/// Names/paths and skipped counts (not in the snapshot). Called on attach and when an event
/// mentions an unknown folder; never on a timer.
pub async fn refresh_folders<R: Rpc>(rpc: &R, shared: &Shared) {
    if let Ok(list) = rpc.call(method::FOLDERS_LIST, json!({})).await {
        shared.update(|s| s.apply_folder_list(&list));
    }
}

/// Pending `add_folder` requests (after `ApprovalRequested` or a snapshot); never on a timer.
pub async fn refresh_approvals<R: Rpc>(rpc: &R, shared: &Shared) {
    if let Ok(list) = rpc.call(method::APPROVALS_LIST, json!({})).await {
        shared.update(|s| s.apply_approvals(&list));
    }
}

/// One-off per attach: the tier decision for services older than the snapshot's `tier` field
/// (`tier.detected` is pushed only at service start), and a search model change already running.
pub async fn refresh_tier<R: Rpc>(rpc: &R, shared: &Shared) {
    let known = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .tier()
        .is_some();
    if !known
        && let Ok(d) = rpc.call(method::DIAGNOSTICS, json!({})).await
        && let Some(t) = d.get("tier_decision").filter(|t| !t.is_null())
    {
        shared.update(|s| {
            s.apply_tier_decision(t);
            true
        });
    }
    if let Ok(st) = rpc.call(method::SEARCH_MODEL_STATUS, json!({})).await {
        shared.update(|s| s.apply_search_model_status(&st));
    }
}

/// The status loop: attach, apply snapshot + events, re-attach after restarts. Runs for the
/// app's lifetime.
pub async fn run_events(link: Arc<DaemonLink>, shared: Shared) {
    let mut backoff = BACKOFF_MIN;
    loop {
        shared.update(|s| {
            s.set_connecting();
            true
        });
        let mut sub = match Subscription::start(link.opts.clone(), None).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(code = ?e.code, "attach failed");
                shared.update(|s| {
                    s.set_unavailable(&e);
                    true
                });
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    () = link.retry.notified() => {}
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        backoff = BACKOFF_MIN;
        let l2 = link.clone();
        let s2 = shared.clone();
        tokio::spawn(async move { refresh_tier(&*l2, &s2).await });
        // Pending folder requests are listed on every attach (R-UX30): each snapshot below sets
        // `refresh_approvals`, so a request made while the app was closed shows at once.
        loop {
            match sub.next().await {
                Ok(SubItem::Snapshot(snap)) => {
                    let (mut refresh, mut approvals) = (false, false);
                    shared.update(|s| {
                        let fx = s.apply_snapshot(&snap.value);
                        refresh = fx.refresh_folders;
                        approvals = fx.refresh_approvals;
                        fx.changed
                    });
                    if refresh {
                        spawn_refresh(&link, &shared);
                    }
                    if approvals {
                        spawn_approvals(&link, &shared);
                    }
                }
                Ok(SubItem::Event(ev)) => {
                    let (mut refresh, mut approvals, mut signal) = (false, false, None);
                    shared.update(|s| {
                        let fx = s.apply_event(&ev.payload);
                        refresh = fx.refresh_folders;
                        approvals = fx.refresh_approvals;
                        signal = fx.signal;
                        fx.changed
                    });
                    if let Some(sig) = signal {
                        shared.signal(sig);
                    }
                    if refresh {
                        spawn_refresh(&link, &shared);
                    }
                    if approvals {
                        spawn_approvals(&link, &shared);
                    }
                }
                Ok(SubItem::Reconnected { daemon_pid }) => {
                    tracing::info!(daemon_pid, "re-attached to the background service");
                    link.reset().await;
                    shared.update(|s| {
                        s.reconnected();
                        true
                    });
                    let l2 = link.clone();
                    let s2 = shared.clone();
                    tokio::spawn(async move { refresh_tier(&*l2, &s2).await });
                }
                Err(e) => {
                    tracing::warn!(code = ?e.code, "lost the background service");
                    shared.update(|s| {
                        s.set_unavailable(&e);
                        true
                    });
                    break;
                }
            }
        }
    }
}

fn spawn_approvals(link: &Arc<DaemonLink>, shared: &Shared) {
    let l = link.clone();
    let s = shared.clone();
    tokio::spawn(async move { refresh_approvals(&*l, &s).await });
}

fn spawn_refresh(link: &Arc<DaemonLink>, shared: &Shared) {
    let l = link.clone();
    let s = shared.clone();
    tokio::spawn(async move { refresh_folders(&*l, &s).await });
}

#[cfg(test)]
pub mod fake {
    //! In-memory stand-in for the service (command tests).
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    pub struct FakeRpc {
        pub calls: Mutex<Vec<(String, Value)>>,
        pub replies: Mutex<VecDeque<Result<Value, Error>>>,
    }

    impl FakeRpc {
        pub fn reply(&self, r: Result<Value, Error>) {
            self.replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(r);
        }
        pub fn calls(&self) -> Vec<(String, Value)> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl Rpc for FakeRpc {
        async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((method.to_owned(), params));
            self.replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .unwrap_or(Ok(Value::Null))
        }
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    #[test]
    fn ask_and_enhance_outlive_the_service_limit() {
        assert!(timeout_for(method::ANSWER) > Duration::from_secs(150));
        assert!(timeout_for(method::ENHANCE_PROMPT) > Duration::from_secs(150));
        assert_eq!(timeout_for(method::FOLDERS_LIST), REQUEST_TIMEOUT);
    }
}
