//! Daemon-side pipe server (docs/02-architecture.md §2.2, architect review D1, Amendment 3.10).
//! Owner: M1-C.
//!
//! * Pipe `\\.\pipe\sr-<sidhash>-<datadirhash>` ([`sr_client::naming::pipe_name`], so client and
//!   server can never disagree), DACL = current user SID only + mandatory label
//!   `S:(ML;;NWNR;;;ME)`, `PIPE_REJECT_REMOTE_CLIENTS`, `FILE_FLAG_FIRST_PIPE_INSTANCE` on the
//!   first instance ([`sr_client::win::PipeSecurity`]).
//! * [`HostLock`]: `LockFileEx(<data_dir>\host.lock)` held for the daemon's life (one host per
//!   user + data dir, also across RDP and console sessions).
//! * Handshake `{proto, build, kind, client_id}` with the skew rules of
//!   [`sr_common::protocol::decide`]: N-1..N accepted; older → `Unavailable{upgrade}`; newer with
//!   `drain_restart` → `Unavailable{restarting}` and the server drains.
//! * `kind=mcp`: the raw stream goes to [`PipeService::serve_mcp`] (rmcp `transport-async-rw`).
//! * `kind=app`: length-prefixed JSON-RPC. `host.ping`, `host.shutdown`, `events.subscribe` and
//!   `events.unsubscribe` are served here; every other method goes to [`PipeService::call`].
//! * Events: [`EventHub`] sequences under one lock and broadcasts in seq order. A subscriber gets
//!   the subscribe response, then the replayed events, then live events: strictly increasing,
//!   gap-free. A subscriber that lags past the broadcast buffer is caught up from the replay ring,
//!   or sent `events.resnapshot` when the ring has moved on.
//!
//! This module is generic over [`PipeService`] and uses only tokio, serde, `sr-common` and
//! `sr-client`, so it does not need the rest of the engine (the M1-C stub daemon compiles this
//! very file to test it).

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sr_client::naming;
use sr_client::win::{self, PipeSecurity};
use sr_common::error::{Error, ErrorCode};
use sr_common::events::{Cursor, EventLog, Replay, Seq, Snapshot, now_ms};
use sr_common::ids::StreamId;
use sr_common::protocol::rpc::{
    self, Notification, PingResult, Request, Response, RpcError, SubscribeParams, SubscribeResult,
};
use sr_common::protocol::{
    ConnKind, HANDSHAKE_TIMEOUT_MS, Handshake, HandshakeReply, MAX_HANDSHAKE_BYTES, SkewDecision,
    UnavailableReason, decide, min_accepted,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, WriteHalf};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::sync::{Mutex, broadcast, watch};

/// Live-event buffer per subscriber before it counts as lagging.
pub const BROADCAST_CAPACITY: usize = 1024;
/// S-11: engine calls one app session may have running at once; the next request waits
/// (reading from the pipe pauses), so a same-user client can't spawn without bound.
pub const MAX_IN_FLIGHT_PER_SESSION: usize = 32;
/// S-11: largest request frame from an app session that isn't our own app (ours keep the
/// protocol's 16 MiB limit for imports and big settings).
pub const MAX_UNTRUSTED_FRAME: usize = 1024 * 1024;

/// `host.restart` (M4-E): drain and exit; served by the pipe server like `host.shutdown`.
pub const HOST_RESTART: &str = "host.restart";

// ------------------------------------------------------------------------------------------------
// Event hub

/// The one ordered event source: sequencing + replay ring ([`EventLog`]) + live broadcast.
/// `publish` sequences and broadcasts under one lock, so live order equals seq order.
pub struct EventHub<E> {
    log: EventLog<E>,
    tx: broadcast::Sender<Seq<E>>,
    order: std::sync::Mutex<()>,
}

impl<E: Clone + Send + 'static> EventHub<E> {
    pub fn new(ring_capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            log: EventLog::new(ring_capacity),
            tx,
            order: std::sync::Mutex::new(()),
        }
    }

    /// Sequence, append to the ring and broadcast.
    pub fn publish(&self, payload: E) -> Seq<E> {
        let _g = self
            .order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ev = self.log.publish(payload);
        // No receivers is fine.
        let _ = self.tx.send(ev.clone());
        ev
    }

    /// Snapshot whose `seq` exactly matches the state captured by `f`.
    pub fn snapshot<S>(&self, f: impl FnOnce() -> S) -> Snapshot<S> {
        self.log.snapshot(f)
    }

    pub fn replay_since(&self, cursor: Cursor) -> Replay<E> {
        self.log.replay_since(cursor)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Seq<E>> {
        self.tx.subscribe()
    }

    pub fn cursor(&self) -> Cursor {
        self.log.cursor()
    }

    pub fn stream(&self) -> StreamId {
        self.log.stream()
    }
}

// ------------------------------------------------------------------------------------------------
// Service trait

/// Who is on the other end of a session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Per-daemon-run session number.
    pub id: u64,
    /// From the kernel (`GetNamedPipeClientProcessId`), not from the handshake.
    pub client_pid: Option<u32>,
    pub handshake: Handshake,
    /// R31 (M4-E): a `kind=app` peer whose process image is our own `sr-app.exe` or
    /// `sr-daemon.exe` next to this daemon ([`trusted_peer`]). Privileged methods need it.
    pub trusted_app: bool,
}

/// R31 (M4-E): whether the process `pid` is Selene Retrieval's own app (`sr-app.exe`) or the
/// hidden admin CLI (`sr-daemon.exe`) installed next to this daemon. The pid comes from the
/// kernel, the image path from `QueryFullProcessImageNameW`; both paths are canonicalised before
/// the case-insensitive comparison. Debug builds honour `SR_TRUST_ANY_APP` (tests).
pub fn trusted_peer(pid: Option<u32>) -> bool {
    if naming::dev_env("SR_TRUST_ANY_APP").is_some() {
        return true;
    }
    let Some(image) = pid.and_then(win::process_image_path) else {
        return false;
    };
    let Ok(me) = std::env::current_exe() else {
        return false;
    };
    is_trusted_image(&image, &me)
}

/// The rule behind [`trusted_peer`]: same directory as the daemon, and the file is `sr-app.exe`
/// or `sr-daemon.exe`.
pub fn is_trusted_image(peer: &Path, daemon: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (peer, daemon) = (canon(peer), canon(daemon));
    let (Some(pd), Some(dd)) = (peer.parent(), daemon.parent()) else {
        return false;
    };
    let name = peer
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase());
    naming::data_dir_key(pd) == naming::data_dir_key(dd)
        && matches!(name.as_deref(), Some("sr-app.exe" | "sr-daemon.exe"))
}

/// S-05 (R31): may this app session stop or restart the service (`host.shutdown`,
/// `host.restart`)? Our own app / admin CLI ([`SessionInfo::trusted_app`]) always. The installer
/// (`sr-setup.exe`, which runs from a temporary folder) may ask for a **drained** shutdown: any
/// same-user process could end the daemon with `TerminateProcess` anyway, and a drain commits
/// the current batch first, so refusing it would only make updates rougher. Pure, for tests.
pub fn host_call_allowed(method: &str, trusted_app: bool, peer_image: Option<&Path>) -> bool {
    if trusted_app {
        return true;
    }
    method == rpc::SHUTDOWN
        && peer_image
            .and_then(Path::file_name)
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("sr-setup.exe"))
}

/// S-05: a handshake `drain_restart` (version skew) stops the running daemon, so only our own
/// binaries next to it may ask: `sr-app.exe`, `sr-bridge.exe` (a newer bridge after an update)
/// or `sr-daemon.exe`. Anyone else gets the plain "service is outdated" reply instead.
pub fn drain_restart_allowed(pid: Option<u32>) -> bool {
    if naming::dev_env("SR_TRUST_ANY_APP").is_some() {
        return true;
    }
    let (Some(image), Ok(me)) = (
        pid.and_then(win::process_image_path),
        std::env::current_exe(),
    ) else {
        return false;
    };
    is_install_image(
        &image,
        &me,
        &["sr-app.exe", "sr-bridge.exe", "sr-daemon.exe"],
    )
}

/// `peer` lives in the daemon's directory and is one of `names` (case-insensitive). Pure.
pub fn is_install_image(peer: &Path, daemon: &Path, names: &[&str]) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (peer, daemon) = (canon(peer), canon(daemon));
    let (Some(pd), Some(dd)) = (peer.parent(), daemon.parent()) else {
        return false;
    };
    let name = peer
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase());
    naming::data_dir_key(pd) == naming::data_dir_key(dd)
        && name.is_some_and(|n| names.contains(&n.as_str()))
}

/// What the engine plugs into the pipe server.
pub trait PipeService: Send + Sync + 'static {
    type Event: Serialize + Clone + Send + Sync + 'static;

    /// The engine's event hub.
    fn events(&self) -> &EventHub<Self::Event>;

    /// Status snapshot taken with [`EventHub::snapshot`] (so its `seq` matches).
    fn snapshot(&self) -> Snapshot<Value>;

    /// Serve one MCP session over the pipe (rmcp `serve(stream)`); return when it ends.
    fn serve_mcp(
        &self,
        stream: NamedPipeServer,
        session: SessionInfo,
    ) -> impl Future<Output = ()> + Send;

    /// Every app-RPC method not under `host.*` / `events.*`.
    fn call(
        &self,
        method: String,
        params: Value,
        session: SessionInfo,
    ) -> impl Future<Output = Result<Value, Error>> + Send;

    /// Drain: finish the current batch and commit. Called once, after the server has stopped
    /// accepting; [`ServerHandle::stopped`] resolves when it returns.
    fn drain(&self) -> impl Future<Output = ()> + Send;

    /// A `kind=app` session ended (M4-E: its in-flight calls are cancelled).
    fn app_session_closed(&self, _session: &SessionInfo) {}
}

// ------------------------------------------------------------------------------------------------
// Host lock

/// `LockFileEx(<data_dir>\host.lock)`, held for the daemon's life.
#[derive(Debug)]
pub struct HostLock {
    _file: std::fs::File,
    pub path: PathBuf,
}

impl HostLock {
    /// `Ok(None)` when another daemon holds it (the caller exits 0 quietly).
    pub fn acquire(data_dir: &Path) -> io::Result<Option<Self>> {
        let path = data_dir.join(naming::HOST_LOCK);
        Ok(win::lock_file(&path, false)?.map(|f| Self { _file: f, path }))
    }
}

// ------------------------------------------------------------------------------------------------
// Server

/// Pipe server configuration.
#[derive(Debug, Clone)]
pub struct PipeServerConfig {
    pub data_dir: PathBuf,
    /// Protocol this daemon speaks (tests override it).
    pub proto: u16,
    pub build: String,
}

impl PipeServerConfig {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            proto: sr_common::protocol::PROTO_VERSION,
            build: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

struct State {
    cfg: PipeServerConfig,
    pipe_name: String,
    effective_sddl: String,
    sessions: AtomicU32,
    /// Open `kind=app` sessions (the desktop app; R-UX30 launches it when this is 0).
    app_sessions: AtomicU32,
    next_session: AtomicU64,
    last_activity_ms: AtomicU64,
    draining: AtomicBool,
    drain_tx: watch::Sender<bool>,
    stopped_tx: watch::Sender<bool>,
}

impl State {
    fn begin_drain(&self) {
        if !self.draining.swap(true, Ordering::SeqCst) {
            let _ = self.drain_tx.send(true);
        }
    }
    fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }
}

/// Handle to a running pipe server.
#[derive(Clone)]
pub struct ServerHandle {
    st: Arc<State>,
}

impl ServerHandle {
    pub fn pipe_name(&self) -> &str {
        &self.st.pipe_name
    }
    /// Effective SDDL of the first instance (diagnostics).
    pub fn effective_sddl(&self) -> &str {
        &self.st.effective_sddl
    }
    /// Open sessions (MCP + app).
    pub fn sessions(&self) -> u32 {
        self.st.sessions.load(Ordering::SeqCst)
    }
    /// Open `kind=app` sessions (is the desktop app attached?).
    pub fn app_sessions(&self) -> u32 {
        self.st.app_sessions.load(Ordering::SeqCst)
    }
    /// Time since the last session opened or closed (for the idle-exit rule, U24).
    pub fn idle_for(&self) -> Duration {
        Duration::from_millis(
            now_ms().saturating_sub(self.st.last_activity_ms.load(Ordering::Relaxed)),
        )
    }
    pub fn is_draining(&self) -> bool {
        self.st.draining.load(Ordering::SeqCst)
    }
    /// Stop accepting, drain the service, then resolve [`stopped`](Self::stopped).
    pub fn begin_drain(&self) {
        self.st.begin_drain();
    }
    /// Resolves once draining has finished; the daemon then exits.
    pub async fn stopped(&self) {
        let mut rx = self.st.stopped_tx.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }
}

/// Opens the first pipe instance (fails if another process owns the name) and starts serving.
/// Call this **last** in the daemon start sequence: an open pipe means "ready".
pub fn serve<S: PipeService>(cfg: PipeServerConfig, svc: Arc<S>) -> io::Result<ServerHandle> {
    let sid = win::user_sid_string()?;
    let pipe_name = naming::pipe_name(&sid, &cfg.data_dir);
    let sec = Arc::new(PipeSecurity::for_sid(&sid)?);
    let first = sec.create_server(&pipe_name, true)?;
    let effective_sddl = win::handle_sddl(std::os::windows::io::AsRawHandle::as_raw_handle(&first))
        .unwrap_or_else(|e| format!("<GetSecurityInfo error {e}>"));
    let (drain_tx, drain_rx) = watch::channel(false);
    let (stopped_tx, _) = watch::channel(false);
    let st = Arc::new(State {
        cfg,
        pipe_name,
        effective_sddl,
        sessions: AtomicU32::new(0),
        app_sessions: AtomicU32::new(0),
        next_session: AtomicU64::new(1),
        last_activity_ms: AtomicU64::new(now_ms()),
        draining: AtomicBool::new(false),
        drain_tx,
        stopped_tx,
    });
    tokio::spawn(accept_loop(st.clone(), sec, first, svc, drain_rx));
    Ok(ServerHandle { st })
}

async fn accept_loop<S: PipeService>(
    st: Arc<State>,
    sec: Arc<PipeSecurity>,
    first: NamedPipeServer,
    svc: Arc<S>,
    mut drain_rx: watch::Receiver<bool>,
) {
    let mut listener = Some(first);
    while let Some(server) = listener.take() {
        tokio::select! {
            r = server.connect() => {
                if r.is_ok() {
                    tokio::spawn(session(st.clone(), svc.clone(), server));
                }
            }
            _ = drain_rx.wait_for(|v| *v) => break,
        }
        // Next listening instance; retry with backoff (e.g. instance limit) until draining.
        let mut delay = Duration::from_millis(10);
        while listener.is_none() && !st.draining.load(Ordering::SeqCst) {
            match sec.create_server(&st.pipe_name, false) {
                Ok(s) => listener = Some(s),
                Err(_) => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(1));
                }
            }
        }
    }
    // Listener dropped: no new connections. Drain the service, then report stopped.
    st.begin_drain();
    svc.drain().await;
    let _ = st.stopped_tx.send(true);
}

struct SessionGuard(Arc<State>);
impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.sessions.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

async fn read_line<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut v = Vec::with_capacity(160);
    let mut b = [0u8; 1];
    loop {
        if r.read(&mut b).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if b[0] == b'\n' {
            return Ok(v);
        }
        v.push(b[0]);
        if v.len() > MAX_HANDSHAKE_BYTES {
            return Err(io::Error::other("handshake too long"));
        }
    }
}

async fn reply<W: AsyncWrite + Unpin>(w: &mut W, r: &HandshakeReply) -> io::Result<()> {
    w.write_all(&r.to_line()).await?;
    w.flush().await
}

fn unavailable(st: &State, reason: UnavailableReason, client_id: &str) -> HandshakeReply {
    let code = reason.to_error(client_id).code;
    HandshakeReply::Unavailable {
        reason,
        code,
        message: reason.plain_message(client_id),
        proto: st.cfg.proto,
        min_proto: min_accepted(st.cfg.proto),
        build: st.cfg.build.clone(),
    }
}

async fn session<S: PipeService>(st: Arc<State>, svc: Arc<S>, mut pipe: NamedPipeServer) {
    let client_pid = win::pipe_client_pid(&pipe);
    let line = match tokio::time::timeout(
        Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
        read_line(&mut pipe),
    )
    .await
    {
        Ok(Ok(l)) => l,
        Ok(Err(e)) if e.kind() != io::ErrorKind::UnexpectedEof => {
            let _ = reply(
                &mut pipe,
                &HandshakeReply::Invalid {
                    message: e.to_string(),
                },
            )
            .await;
            return;
        }
        _ => return, // probe connection or timeout
    };
    let hs = match Handshake::parse(&line) {
        Ok(h) => h,
        Err(e) => {
            let _ = reply(&mut pipe, &HandshakeReply::Invalid { message: e }).await;
            return;
        }
    };
    if st.draining.load(Ordering::SeqCst) {
        let _ = reply(
            &mut pipe,
            &unavailable(&st, UnavailableReason::Draining, &hs.client_id),
        )
        .await;
        return;
    }
    match decide(st.cfg.proto, hs.proto, hs.drain_restart) {
        SkewDecision::Accept => {}
        SkewDecision::ClientTooOld => {
            let _ = reply(
                &mut pipe,
                &unavailable(&st, UnavailableReason::Upgrade, &hs.client_id),
            )
            .await;
            return;
        }
        SkewDecision::ClientNewer => {
            let _ = reply(
                &mut pipe,
                &unavailable(&st, UnavailableReason::DaemonOutdated, &hs.client_id),
            )
            .await;
            return;
        }
        // S-05: only our own binaries may stop the running daemon for a version skew.
        SkewDecision::DrainRestart if !drain_restart_allowed(client_pid) => {
            tracing::warn!(target: "sr_engine::pipe", "drain-restart refused: caller is not one of Selene Retrieval's programs");
            let _ = reply(
                &mut pipe,
                &unavailable(&st, UnavailableReason::DaemonOutdated, &hs.client_id),
            )
            .await;
            return;
        }
        SkewDecision::DrainRestart => {
            let _ = reply(
                &mut pipe,
                &unavailable(&st, UnavailableReason::Restarting, &hs.client_id),
            )
            .await;
            drop(pipe);
            st.begin_drain();
            return;
        }
    }
    let accepted = HandshakeReply::Accepted {
        proto: st.cfg.proto,
        min_proto: min_accepted(st.cfg.proto),
        build: st.cfg.build.clone(),
        daemon_pid: std::process::id(),
    };
    if reply(&mut pipe, &accepted).await.is_err() {
        return;
    }
    st.sessions.fetch_add(1, Ordering::SeqCst);
    st.touch();
    let _guard = SessionGuard(st.clone());
    let trusted_app = hs.kind == ConnKind::App && trusted_peer(client_pid);
    let info = SessionInfo {
        id: st.next_session.fetch_add(1, Ordering::Relaxed),
        client_pid,
        handshake: hs,
        trusted_app,
    };
    match info.handshake.kind {
        ConnKind::Mcp => svc.serve_mcp(pipe, info).await,
        ConnKind::App => {
            st.app_sessions.fetch_add(1, Ordering::SeqCst);
            let _app = AppGuard(st.clone());
            app_session(st.clone(), svc, pipe, info).await;
        }
    }
}

struct AppGuard(Arc<State>);
impl Drop for AppGuard {
    fn drop(&mut self) {
        self.0.app_sessions.fetch_sub(1, Ordering::SeqCst);
    }
}

// ------------------------------------------------------------------------------------------------
// App-RPC session

type Writer = Arc<Mutex<WriteHalf<NamedPipeServer>>>;

async fn read_frame_limited<R: AsyncRead + Unpin>(
    r: &mut R,
    max: Option<usize>,
) -> io::Result<Option<Vec<u8>>> {
    let mut h = [0u8; 4];
    match r.read_exact(&mut h).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = rpc::frame_len(h).map_err(io::Error::other)?;
    if max.is_some_and(|m| n > m) {
        // Closing the session is the answer (the body is never allocated).
        return Err(io::Error::other("request frame too large"));
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body).await?;
    Ok(Some(body))
}

async fn send<T: Serialize>(w: &Writer, msg: &T) -> io::Result<()> {
    let buf = rpc::encode_frame(msg).map_err(io::Error::other)?;
    let mut g = w.lock().await;
    g.write_all(&buf).await?;
    g.flush().await
}

fn to_value<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

async fn app_session<S: PipeService>(
    st: Arc<State>,
    svc: Arc<S>,
    pipe: NamedPipeServer,
    info: SessionInfo,
) {
    let (mut r, w) = tokio::io::split(pipe);
    let w: Writer = Arc::new(Mutex::new(w));
    let mut sub: Option<tokio::task::JoinHandle<()>> = None;
    let in_flight = Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT_PER_SESSION));
    let max_frame = (!info.trusted_app).then_some(MAX_UNTRUSTED_FRAME);
    loop {
        let frame = match read_frame_limited(&mut r, max_frame).await {
            Ok(Some(f)) => f,
            Ok(None) | Err(_) => break,
        };
        let req: Request = match serde_json::from_slice(&frame) {
            Ok(r) => r,
            Err(e) => {
                let err = RpcError::new(rpc::PARSE_ERROR, format!("bad request: {e}"));
                if send(&w, &Response::err(0, err)).await.is_err() {
                    break;
                }
                continue;
            }
        };
        let id = req.id;
        match req.method.as_str() {
            rpc::PING => {
                let p = PingResult {
                    daemon_pid: std::process::id(),
                    proto: st.cfg.proto,
                    build: st.cfg.build.clone(),
                    sessions: st.sessions.load(Ordering::SeqCst),
                    cursor: svc.events().cursor(),
                };
                if send(&w, &Response::ok(id, to_value(&p))).await.is_err() {
                    break;
                }
            }
            // S-05 (R31): stopping the service is privileged (see [`host_call_allowed`]).
            m @ (rpc::SHUTDOWN | HOST_RESTART)
                if !host_call_allowed(
                    m,
                    info.trusted_app,
                    info.client_pid.and_then(win::process_image_path).as_deref(),
                ) =>
            {
                tracing::warn!(target: "sr_engine::pipe", method = m, "privileged call refused: caller is not Selene Retrieval's app");
                let e = Error::new(ErrorCode::PermissionDenied)
                    .with_detail("this action is only available in the Selene Retrieval app");
                if send(&w, &Response::err(id, RpcError::from_error(&e)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            rpc::SHUTDOWN => {
                let _ = send(&w, &Response::ok(id, Value::Null)).await;
                st.begin_drain();
            }
            // M4-E (page 03, R-UX31): drain and exit; the app re-attaches, which launches a fresh
            // daemon through sr-launch / the scheduled task.
            HOST_RESTART => {
                let _ = send(
                    &w,
                    &Response::ok(id, serde_json::json!({ "restarting": true })),
                )
                .await;
                st.begin_drain();
            }
            rpc::SUBSCRIBE => {
                if sub.as_ref().is_some_and(|h| !h.is_finished()) {
                    let err = RpcError::new(rpc::INVALID_REQUEST, "already subscribed");
                    if send(&w, &Response::err(id, err)).await.is_err() {
                        break;
                    }
                    continue;
                }
                let params: SubscribeParams =
                    serde_json::from_value(req.params).unwrap_or_default();
                sub = Some(tokio::spawn(subscription(
                    svc.clone(),
                    w.clone(),
                    id,
                    params.cursor,
                )));
            }
            rpc::UNSUBSCRIBE => {
                if let Some(h) = sub.take() {
                    h.abort();
                }
                if send(&w, &Response::ok(id, Value::Null)).await.is_err() {
                    break;
                }
            }
            m if m.starts_with("host.") || m.starts_with("events.") => {
                let err = RpcError::new(rpc::METHOD_NOT_FOUND, format!("unknown method {m}"));
                if send(&w, &Response::err(id, err)).await.is_err() {
                    break;
                }
            }
            _ => {
                // Engine methods run concurrently; responses may complete out of order.
                // S-11: at most MAX_IN_FLIGHT_PER_SESSION at once per session.
                let Ok(permit) = in_flight.clone().acquire_owned().await else {
                    break;
                };
                let (svc, w, info) = (svc.clone(), w.clone(), info.clone());
                tokio::spawn(async move {
                    let _permit = permit;
                    let resp = match svc.call(req.method, req.params, info).await {
                        Ok(v) => Response::ok(id, v),
                        Err(e) => Response::err(id, RpcError::from_error(&e)),
                    };
                    let _ = send(&w, &resp).await;
                });
            }
        }
    }
    if let Some(h) = sub {
        h.abort();
    }
    svc.app_session_closed(&info);
}

/// Sends the subscribe response, the replayed events, then live events, in seq order.
async fn subscription<S: PipeService>(svc: Arc<S>, w: Writer, id: u64, cursor: Option<Cursor>) {
    let hub = svc.events();
    // Subscribe to live events *before* reading the replay/snapshot position, so nothing published
    // in between is lost (duplicates are filtered by seq).
    let mut rx = hub.subscribe();
    let stream = hub.stream();
    let (result, replay) = match cursor.map(|c| hub.replay_since(c)) {
        Some(Replay::Events(v)) => (
            SubscribeResult::Replay {
                cursor: cursor.unwrap_or(Cursor { stream, seq: 0 }),
            },
            v,
        ),
        Some(Replay::Resnapshot) | None => (
            SubscribeResult::Snapshot {
                snapshot: svc.snapshot(),
            },
            Vec::new(),
        ),
    };
    let mut last = match &result {
        SubscribeResult::Replay { cursor } => cursor.seq,
        SubscribeResult::Snapshot { snapshot } => snapshot.seq,
    };
    if send(&w, &Response::ok(id, to_value(&result)))
        .await
        .is_err()
    {
        return;
    }
    if send_events(&w, replay, &mut last).await.is_err() {
        return;
    }
    loop {
        match rx.recv().await {
            Ok(ev) if ev.seq <= last => {}
            Ok(ev) if ev.seq == last + 1 => {
                if send_events(&w, vec![ev], &mut last).await.is_err() {
                    return;
                }
            }
            // A gap (cannot happen with ordered publish) or a lag: catch up from the ring.
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                let r = match hub.replay_since(Cursor { stream, seq: last }) {
                    Replay::Events(v) => send_events(&w, v, &mut last).await,
                    Replay::Resnapshot => {
                        let snap = svc.snapshot();
                        last = snap.seq;
                        send(&w, &Notification::new(rpc::RESNAPSHOT, to_value(&snap))).await
                    }
                };
                if r.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn send_events<E: Serialize>(w: &Writer, evs: Vec<Seq<E>>, last: &mut u64) -> io::Result<()> {
    for ev in evs {
        if ev.seq <= *last {
            continue;
        }
        send(w, &Notification::new(rpc::EVENT, to_value(&ev))).await?;
        *last = ev.seq;
    }
    Ok(())
}

/// The error a daemon should surface when it is started in a packaged (MSIX) context
/// (it must refuse to own stores there; the client shows the U8 text).
pub fn packaged_context_error() -> Option<Error> {
    win::package_full_name().map(|p| {
        Error::new(ErrorCode::ServiceUnavailable).with_detail(format!(
            "started with package identity {p}; refusing to own stores"
        ))
    })
}
