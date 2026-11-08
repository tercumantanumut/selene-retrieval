//! Opt-in streamable HTTP for other local tools (M4-E; decision 6 "no TCP port by default";
//! 02 §2.6 "Opt-in HTTP"; page 29 Advanced).
//!
//! Security is fixed and invisible (P1.7):
//! * binds **127.0.0.1 only** (there is no address setting; a non-loopback peer is dropped);
//! * every request needs `Authorization: Bearer <token>` of one HTTP tool (256-bit, Credential
//!   Manager; checked against the engine's in-memory hash map), else `401`;
//! * `Host` must be `127.0.0.1:<port>` or `localhost:<port>` and an `Origin`, if present,
//!   `http://127.0.0.1:<port>` / `http://localhost:<port>` (DNS-rebinding guard), else `403`. rmcp
//!   checks the same lists again (`allowed_hosts`, `allowed_origins`);
//! * bodies ≤ 4 MiB (rmcp), ≤ 16 requests in flight (`503`), ≤ 64 connections, 10 s to send the
//!   headers, 30 s until the response starts (`504`);
//! * only `/mcp` exists (`404`).
//!
//! The authenticated tool's id travels in the request extensions ([`HttpClientId`]); rmcp hands the
//! request parts to every tool call, so each call runs in that tool's scope.
//!
//! The listener follows [`sr_engine::EngineHandle::http_wanted`]: it starts when HTTP is turned on,
//! stops when it is turned off, and restarts (ending every session) when a token is revoked or
//! rotated. It reports its port and state back with `http_report`.
//!
//! S-03 (M5-E): the port is picked once and then never changes by itself. If the remembered port
//! is held by another program, HTTP stays off with `PortInUse` ("Port in use", [Retry]) and the
//! same port is tried again every [`PORT_RETRY`]; moving would leave configured tools sending
//! their token to whoever holds the old port. Bound with `SO_EXCLUSIVEADDRUSE`.
//!
//! R35 (M5-E): sessions don't live forever. rmcp ends a session's worker after 5 minutes without
//! traffic but keeps its entry until the client sends `DELETE`; a sweeper ([`SESSION_SWEEP`])
//! closes every session idle for [`SESSION_IDLE`] and keeps at most [`MAX_SESSIONS`] (the
//! longest-idle ones go first).

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode, header};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use sr_common::error::ErrorCode;
use sr_engine::EngineHandle;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tower_service::Service;

use crate::server::{HttpClientId, McpServer};
use crate::slot::EngineSlot;

/// The only path.
pub const PATH: &str = "/mcp";
/// Requests handled at once (more get `503`).
pub const MAX_IN_FLIGHT: usize = 16;
/// Open connections (more wait for a free slot).
pub const MAX_CONNECTIONS: usize = 64;
/// Until the response starts (streams continue after it).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// To send the request headers (slow-loris guard).
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// Request body cap (02 §2.6: 4 MiB).
pub const MAX_BODY: usize = 4 * 1024 * 1024;
/// R35: a session with no request for this long is closed.
pub const SESSION_IDLE: Duration = Duration::from_secs(15 * 60);
/// R35: how often idle sessions are looked for.
pub const SESSION_SWEEP: Duration = Duration::from_secs(60);
/// R35: open sessions kept at most (the longest-idle ones are closed first).
pub const MAX_SESSIONS: usize = 64;
/// S-11: open sessions one HTTP tool (token) keeps at most; the sweeper closes its longest-idle
/// ones beyond this …
pub const MAX_SESSIONS_PER_TOOL: usize = 8;
/// … and a new session is refused (`429`) while the tool already holds twice as many.
pub const MAX_SESSIONS_PER_TOOL_HARD: usize = 2 * MAX_SESSIONS_PER_TOOL;
/// S-03: while the remembered port is held by another program, try it again this often.
pub const PORT_RETRY: Duration = Duration::from_secs(30);
/// The MCP session header (rmcp's `HEADER_SESSION_ID`).
const SESSION_HEADER: &str = "mcp-session-id";

/// R35: when each HTTP session last saw a request, and (S-11) which tool opened it.
#[derive(Debug, Default)]
pub struct SessionTracker {
    seen: Mutex<HashMap<String, Instant>>,
    owner: Mutex<HashMap<String, String>>,
}

impl SessionTracker {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn owners(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A request for (or the creation of) session `id`.
    pub fn touch(&self, id: &str, now: Instant) {
        self.map().insert(id.to_string(), now);
    }

    /// S-11: session `id` belongs to HTTP tool `client` (first one wins).
    pub fn set_owner(&self, id: &str, client: &str) {
        self.owners()
            .entry(id.to_string())
            .or_insert_with(|| client.to_string());
    }

    /// S-11: sessions tool `client` holds now.
    pub fn count_for(&self, client: &str) -> usize {
        self.owners().values().filter(|c| *c == client).count()
    }

    /// The client ended the session itself.
    pub fn forget(&self, id: &str) {
        self.map().remove(id);
        self.owners().remove(id);
    }

    /// Sessions to close now: idle for `idle` or more, plus the longest-idle ones beyond `max`.
    /// `open` lists the sessions the server still holds (unknown ones start their clock now;
    /// ones the server no longer holds are dropped from the tracker). Removed from the tracker.
    pub fn expired(
        &self,
        open: &[String],
        now: Instant,
        idle: Duration,
        max: usize,
    ) -> Vec<String> {
        let mut m = self.map();
        m.retain(|k, _| open.iter().any(|o| o == k));
        for o in open {
            m.entry(o.clone()).or_insert(now);
        }
        let mut out: Vec<String> = m
            .iter()
            .filter(|(_, t)| now.saturating_duration_since(**t) >= idle)
            .map(|(k, _)| k.clone())
            .collect();
        let mut live: Vec<(String, Instant)> = m
            .iter()
            .filter(|(k, _)| !out.contains(k))
            .map(|(k, t)| (k.clone(), *t))
            .collect();
        live.sort_by_key(|(_, t)| *t);
        if live.len() > max {
            let extra = live.len() - max;
            out.extend(live.drain(..extra).map(|(k, _)| k));
        }
        // S-11: per tool, keep its MAX_SESSIONS_PER_TOOL most recently used sessions.
        let mut owners = self.owners();
        owners.retain(|k, _| m.contains_key(k));
        let mut per: HashMap<String, usize> = HashMap::new();
        for (k, _) in live.iter().rev() {
            if let Some(c) = owners.get(k) {
                let n = per.entry(c.clone()).or_insert(0);
                *n += 1;
                if *n > MAX_SESSIONS_PER_TOOL {
                    out.push(k.clone());
                }
            }
        }
        for k in &out {
            m.remove(k);
            owners.remove(k);
        }
        out
    }
}

type Body = BoxBody<Bytes, Infallible>;
type Mcp = StreamableHttpService<McpServer, LocalSessionManager>;

fn plain(status: StatusCode, text: &'static str) -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from_static(text.as_bytes())).boxed());
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

/// `Host` is our own loopback authority.
pub fn host_ok(host: Option<&str>, port: u16) -> bool {
    host.is_some_and(|h| {
        let h = h.trim().to_ascii_lowercase();
        h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
    })
}

/// An `Origin`, when present, is our own loopback origin (browsers send one on cross-site
/// requests; command-line tools send none).
pub fn origin_ok(origin: Option<&str>, port: u16) -> bool {
    origin.is_none_or(|o| {
        let o = o.trim().to_ascii_lowercase();
        o == format!("http://127.0.0.1:{port}") || o == format!("http://localhost:{port}")
    })
}

/// The token of an `Authorization: Bearer <token>` header.
pub fn bearer(value: Option<&str>) -> Option<&str> {
    let (scheme, rest) = value?.trim().split_once(' ')?;
    let t = rest.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !t.is_empty()).then_some(t)
}

/// Admission (path, Host, Origin, bearer token): the HTTP tool's client id, or the refusal
/// (boxed: a `Response` is large and the refusal is the rare path).
pub fn admit(
    parts: &http::request::Parts,
    port: u16,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String, Box<Response<Body>>> {
    let h = |name: header::HeaderName| parts.headers.get(name).and_then(|v| v.to_str().ok());
    if parts.uri.path() != PATH {
        return Err(Box::new(plain(StatusCode::NOT_FOUND, "Not found")));
    }
    if !host_ok(h(header::HOST), port) {
        return Err(Box::new(plain(StatusCode::FORBIDDEN, "Forbidden host")));
    }
    if !origin_ok(h(header::ORIGIN), port) {
        return Err(Box::new(plain(StatusCode::FORBIDDEN, "Forbidden origin")));
    }
    match bearer(h(header::AUTHORIZATION)).and_then(lookup) {
        Some(id) => Ok(id),
        None => {
            let mut r = plain(
                StatusCode::UNAUTHORIZED,
                "A valid token is required. Create one in Selene Retrieval > Connected apps.",
            );
            r.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            Err(Box::new(r))
        }
    }
}

async fn handle(
    req: Request<Incoming>,
    engine: EngineHandle,
    mut mcp: Mcp,
    in_flight: Arc<Semaphore>,
    port: u16,
    tracker: Arc<SessionTracker>,
) -> Result<Response<Body>, Infallible> {
    let (mut parts, body) = req.into_parts();
    let id = match admit(&parts, port, |t| engine.http_client_for_token(t)) {
        Ok(id) => id,
        Err(r) => return Ok(*r),
    };
    let Ok(_permit) = in_flight.try_acquire() else {
        return Ok(plain(StatusCode::SERVICE_UNAVAILABLE, "Busy, try again"));
    };
    let client = id.clone();
    parts.extensions.insert(HttpClientId(id));
    // R35: remember when each session was last used (the sweeper closes idle ones).
    let session = parts
        .headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // S-11: a tool that already holds too many sessions can't open another one.
    if session.is_none() && tracker.count_for(&client) >= MAX_SESSIONS_PER_TOOL_HARD {
        return Ok(plain(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many open sessions for this tool",
        ));
    }
    let ending = parts.method == http::Method::DELETE;
    if let Some(s) = &session {
        tracker.touch(s, Instant::now());
    }
    let req = Request::from_parts(parts, body);
    match tokio::time::timeout(REQUEST_TIMEOUT, mcp.call(req)).await {
        Ok(Ok(r)) => {
            if let Some(s) = r
                .headers()
                .get(SESSION_HEADER)
                .and_then(|v| v.to_str().ok())
            {
                tracker.touch(s, Instant::now());
                tracker.set_owner(s, &client);
            }
            if ending
                && r.status().is_success()
                && let Some(s) = &session
            {
                tracker.forget(s);
            }
            Ok(r)
        }
        Ok(Err(never)) => match never {},
        Err(_) => Ok(plain(StatusCode::GATEWAY_TIMEOUT, "Timed out")),
    }
}

/// R35: close idle HTTP sessions (and the oldest beyond [`MAX_SESSIONS`]) until `stop`.
async fn sweep_sessions(
    sessions: Arc<LocalSessionManager>,
    tracker: Arc<SessionTracker>,
    stop: CancellationToken,
) {
    use rmcp::transport::streamable_http_server::session::SessionManager;
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(SESSION_SWEEP) => {}
        }
        let open: Vec<String> = sessions
            .sessions
            .read()
            .await
            .keys()
            .map(|k| k.to_string())
            .collect();
        let gone = tracker.expired(&open, Instant::now(), SESSION_IDLE, MAX_SESSIONS);
        for id in &gone {
            let sid: rmcp::transport::streamable_http_server::session::SessionId =
                Arc::from(id.as_str());
            if let Err(e) = sessions.close_session(&sid).await {
                tracing::debug!(target: "sr_mcp::http", err = %e, "idle session already gone");
            }
        }
        if !gone.is_empty() {
            tracing::info!(target: "sr_mcp::http", closed = gone.len(), open = open.len() - gone.len().min(open.len()), "idle HTTP sessions closed");
        }
    }
}

/// Why the listener could not bind.
#[derive(Debug)]
enum BindError {
    /// S-03: the remembered port is held by another program. We never move to another port:
    /// configured tools would keep sending their token to whoever holds the old one.
    PortInUse(u16),
    Other(std::io::Error),
}

/// Bind 127.0.0.1 exactly on the remembered port (a free one only the first time, when none is
/// remembered yet). Never any other address, never silently another port (S-03).
/// `SO_EXCLUSIVEADDRUSE` is set before `bind`, so nothing can bind next to us with `SO_REUSEADDR`.
async fn bind(port: Option<u16>) -> Result<TcpListener, BindError> {
    let want = port.filter(|p| *p >= 1024);
    let sock = tokio::net::TcpSocket::new_v4().map_err(BindError::Other)?;
    sr_client::win::set_exclusive_addr_use(&sock).map_err(BindError::Other)?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, want.unwrap_or(0)));
    if let Err(e) = sock.bind(addr) {
        // WSAEADDRINUSE, or WSAEACCES when the holder bound it exclusively.
        return Err(match want {
            Some(p)
                if e.kind() == std::io::ErrorKind::AddrInUse
                    || e.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                BindError::PortInUse(p)
            }
            _ => BindError::Other(e),
        });
    }
    sock.listen(1024).map_err(BindError::Other)
}

async fn run_server(
    engine: EngineHandle,
    slot: EngineSlot,
    port: Option<u16>,
    stop: CancellationToken,
) {
    let mut reported_taken = false;
    let listener = loop {
        match bind(port).await {
            Ok(l) => break l,
            Err(BindError::PortInUse(p)) => {
                // S-03: stay off with a plain notice ("Port in use", [Retry]); try the same port
                // again later. Turning HTTP on again in the app retries at once.
                if !reported_taken {
                    tracing::warn!(target: "sr_mcp::http", port = p, "remembered port is in use by another program; HTTP stays off");
                    engine
                        .http_report(false, Some(p), Some(ErrorCode::PortInUse))
                        .await;
                    reported_taken = true;
                }
                tokio::select! {
                    () = stop.cancelled() => return,
                    () = tokio::time::sleep(PORT_RETRY) => {}
                }
            }
            Err(BindError::Other(e)) => {
                tracing::error!(target: "sr_mcp::http", err = %e, "HTTP listener did not start");
                engine
                    .http_report(false, None, Some(ErrorCode::Internal))
                    .await;
                return;
            }
        }
    };
    let port = match listener.local_addr() {
        Ok(a) if a.ip().is_loopback() => a.port(),
        _ => {
            // Cannot happen (we bind 127.0.0.1); never serve anything else.
            engine
                .http_report(false, None, Some(ErrorCode::Internal))
                .await;
            return;
        }
    };
    let ct = stop.child_token();
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts([format!("127.0.0.1:{port}"), format!("localhost:{port}")])
        .with_allowed_origins([
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
        ])
        .with_max_request_body_bytes(MAX_BODY)
        .with_cancellation_token(ct.clone());
    let factory_slot = slot.clone();
    let sessions = Arc::new(LocalSessionManager::default());
    let tracker = Arc::new(SessionTracker::default());
    let mcp: Mcp = StreamableHttpService::new(
        // Every session starts as the anonymous "http" client; each call runs as the tool whose
        // token the request carried ([`HttpClientId`]).
        move || Ok(McpServer::with_slot(factory_slot.clone()).with_client("http")),
        sessions.clone(),
        config,
    );
    let sweeper = tokio::spawn(sweep_sessions(sessions, tracker.clone(), ct.clone()));
    engine.http_report(true, Some(port), None).await;
    tracing::info!(target: "sr_mcp::http", port, "HTTP for local tools is on (127.0.0.1 only)");
    let in_flight = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut conns = tokio::task::JoinSet::new();
    loop {
        let permit = tokio::select! {
            () = ct.cancelled() => break,
            p = connections.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            },
        };
        let accepted = tokio::select! {
            () = ct.cancelled() => break,
            a = listener.accept() => a,
        };
        let (stream, peer) = match accepted {
            Ok(x) => x,
            Err(e) => {
                tracing::debug!(target: "sr_mcp::http", err = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        let (engine, mcp, in_flight) = (engine.clone(), mcp.clone(), in_flight.clone());
        let tracker = tracker.clone();
        conns.spawn(async move {
            let _permit = permit;
            let svc = hyper::service::service_fn(move |req| {
                handle(
                    req,
                    engine.clone(),
                    mcp.clone(),
                    in_flight.clone(),
                    port,
                    tracker.clone(),
                )
            });
            let mut b = hyper::server::conn::http1::Builder::new();
            b.timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .keep_alive(true);
            if let Err(e) = b.serve_connection(TokioIo::new(stream), svc).await {
                tracing::debug!(target: "sr_mcp::http", err = %e, "HTTP connection ended");
            }
        });
        while conns.try_join_next().is_some() {}
    }
    ct.cancel();
    sweeper.abort();
    conns.abort_all();
    engine.http_report(false, Some(port), None).await;
    tracing::info!(target: "sr_mcp::http", "HTTP for local tools is off");
}

/// Keep the listener in line with the engine's HTTP policy (on / off / restart on token
/// revocation). Runs for the daemon's life.
pub fn supervise(engine: EngineHandle, slot: EngineSlot) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = engine.http_wanted();
        let mut running: Option<(CancellationToken, tokio::task::JoinHandle<()>, u64)> = None;
        loop {
            let want = *rx.borrow_and_update();
            // A listener that stopped by itself (bind failure) can be started again.
            if running.as_ref().is_some_and(|r| r.1.is_finished()) {
                running = None;
            }
            let restart = running
                .as_ref()
                .is_some_and(|r| !want.enabled || r.2 != want.generation);
            if restart && let Some((ct, h, _)) = running.take() {
                ct.cancel();
                let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
            }
            if want.enabled && running.is_none() {
                let ct = CancellationToken::new();
                let h = tokio::spawn(run_server(
                    engine.clone(),
                    slot.clone(),
                    want.port,
                    ct.clone(),
                ));
                running = Some((ct, h, want.generation));
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
        if let Some((ct, h, _)) = running.take() {
            ct.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(path: &str, headers: &[(&str, &str)]) -> http::request::Parts {
        let mut b = Request::builder().uri(path).method("POST");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).unwrap().into_parts().0
    }

    fn lookup(t: &str) -> Option<String> {
        (t == "good").then(|| "http-01".to_string())
    }

    /// R35: idle sessions and the oldest beyond the cap are closed; active ones stay.
    #[test]
    fn idle_sessions_expire_and_are_capped() {
        let t = SessionTracker::default();
        let t0 = Instant::now();
        t.touch("a", t0);
        t.touch("b", t0 + Duration::from_secs(600));
        let open = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        // "c" is unknown to the tracker: its clock starts now.
        let now = t0 + SESSION_IDLE + Duration::from_secs(1);
        assert_eq!(
            t.expired(&open, now, SESSION_IDLE, 10),
            vec!["a".to_string()]
        );
        // Cap: three live sessions, keep two → the longest idle ("b") goes.
        let open = vec!["b".to_string(), "c".to_string(), "d".to_string()];
        t.touch("d", now);
        let mut gone = t.expired(&open, now, SESSION_IDLE, 2);
        gone.sort();
        assert_eq!(gone, vec!["b".to_string()]);
        // A session the server dropped is forgotten; a DELETE forgets at once.
        t.forget("c");
        assert!(
            t.expired(&["d".to_string()], now, SESSION_IDLE, 10)
                .is_empty()
        );
    }

    /// S-11: one tool keeps at most MAX_SESSIONS_PER_TOOL sessions (its newest), others untouched.
    #[test]
    fn sessions_are_capped_per_tool() {
        let t = SessionTracker::default();
        let t0 = Instant::now();
        let mut open = Vec::new();
        for i in 0..(MAX_SESSIONS_PER_TOOL + 3) {
            let id = format!("s{i}");
            t.touch(&id, t0 + Duration::from_secs(i as u64));
            t.set_owner(&id, "http-greedy");
            open.push(id);
        }
        t.touch("other", t0);
        t.set_owner("other", "http-calm");
        open.push("other".into());
        assert_eq!(t.count_for("http-greedy"), MAX_SESSIONS_PER_TOOL + 3);
        let now = t0 + Duration::from_secs(60);
        let mut gone = t.expired(&open, now, SESSION_IDLE, MAX_SESSIONS);
        gone.sort();
        assert_eq!(
            gone,
            vec!["s0".to_string(), "s1".into(), "s2".into()],
            "oldest three"
        );
        assert_eq!(t.count_for("http-greedy"), MAX_SESSIONS_PER_TOOL);
        assert_eq!(t.count_for("http-calm"), 1);
        t.forget("other");
        assert_eq!(t.count_for("http-calm"), 0);
    }

    #[test]
    fn admission_rules() {
        let ok = parts(
            "/mcp",
            &[("host", "127.0.0.1:4100"), ("authorization", "Bearer good")],
        );
        assert_eq!(admit(&ok, 4100, lookup).unwrap(), "http-01");
        let local = parts(
            "/mcp",
            &[("host", "LOCALHOST:4100"), ("authorization", "bearer good")],
        );
        assert!(admit(&local, 4100, lookup).is_ok());
        let status = |p: &http::request::Parts| admit(p, 4100, lookup).unwrap_err().status();
        assert_eq!(
            status(&parts(
                "/other",
                &[("host", "127.0.0.1:4100"), ("authorization", "Bearer good")]
            )),
            StatusCode::NOT_FOUND
        );
        // DNS rebinding: a page on evil.example resolving to 127.0.0.1.
        assert_eq!(
            status(&parts(
                "/mcp",
                &[
                    ("host", "evil.example:4100"),
                    ("authorization", "Bearer good")
                ]
            )),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(&parts(
                "/mcp",
                &[
                    ("host", "127.0.0.1:4100"),
                    ("origin", "https://evil.example"),
                    ("authorization", "Bearer good")
                ]
            )),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(&parts("/mcp", &[("host", "127.0.0.1:4100")])),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(&parts(
                "/mcp",
                &[("host", "127.0.0.1:4100"), ("authorization", "Bearer bad")]
            )),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(&parts(
                "/mcp",
                &[("host", "127.0.0.1:4101"), ("authorization", "Bearer good")]
            )),
            StatusCode::FORBIDDEN,
            "another port's authority"
        );
        let with_origin = parts(
            "/mcp",
            &[
                ("host", "127.0.0.1:4100"),
                ("origin", "http://127.0.0.1:4100"),
                ("authorization", "Bearer good"),
            ],
        );
        assert!(admit(&with_origin, 4100, lookup).is_ok());
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer(Some("  bearer   abc  ")), Some("abc"));
        assert_eq!(bearer(Some("Basic abc")), None);
        assert_eq!(bearer(Some("Bearer ")), None);
        assert_eq!(bearer(None), None);
    }

    #[tokio::test]
    async fn binds_loopback_only() {
        let l = bind(None).await.unwrap();
        let a = l.local_addr().unwrap();
        assert!(a.ip().is_loopback());
        assert!(a.port() > 0);
        // S-03: a taken remembered port is reported, never swapped for another one.
        assert!(matches!(
            bind(Some(a.port())).await,
            Err(BindError::PortInUse(p)) if p == a.port()
        ));
        // Taken by someone else's plain listener (no exclusive flag) too.
        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let op = other.local_addr().unwrap().port();
        assert!(matches!(bind(Some(op)).await, Err(BindError::PortInUse(p)) if p == op));
        // Free again → bound exactly there.
        drop(l);
        let back = bind(Some(a.port())).await.unwrap();
        assert_eq!(back.local_addr().unwrap().port(), a.port());
    }
}
