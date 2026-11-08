//! A tiny HTTP/1.1 server on `127.0.0.1` for provider contract tests (M3-P). No real provider,
//! key or network is ever involved.
//!
//! * Routes match on method + path suffix (or the CONNECT authority) and reply with recorded
//!   fixtures: JSON, raw text, SSE (optionally slow), or [`Reply::Hang`] (never answers).
//! * Every request is recorded (method, path, headers, body) and every accepted connection is
//!   counted, so a test can prove that **no** request was sent (consent gate) and which headers
//!   were sent (auth form, no user identifiers).
//! * [`FakeHttp::wait_closed`] observes the client closing a connection while a reply is still
//!   pending or streaming: the proof that cancellation aborts the HTTP request.
//!
//! Downstream crates (sr-engine answer tests) can use it too, through `sr_providers::testing`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// What the server answers.
#[derive(Debug, Clone)]
pub enum Reply {
    /// `status`, extra headers, JSON body.
    Json {
        status: u16,
        headers: Vec<(String, String)>,
        body: serde_json::Value,
    },
    /// `status`, extra headers, body as is (e.g. an HTML proxy page).
    Raw {
        status: u16,
        headers: Vec<(String, String)>,
        content_type: String,
        body: String,
    },
    /// `200 text/event-stream`: each block is written followed by a blank line, `delay` apart.
    Sse {
        blocks: Vec<String>,
        delay: Duration,
    },
    /// Never answer; keep the connection open until the client goes away.
    Hang,
}

impl Reply {
    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Reply::Json {
            status,
            headers: Vec::new(),
            body,
        }
    }
}

/// One recorded request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub method: String,
    /// Path (origin-form) or authority (CONNECT) or absolute URL (proxy form).
    pub target: String,
    /// Lower-cased header names.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        let n = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == n)
            .map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

#[derive(Debug, Clone)]
struct Route {
    method: String,
    suffix: String,
    reply: Reply,
    /// `Some(n)`: used `n` more times, then skipped.
    times: Option<usize>,
}

#[derive(Default)]
struct State {
    routes: Mutex<Vec<Route>>,
    requests: Mutex<Vec<Recorded>>,
    connections: AtomicUsize,
    closed_early: AtomicUsize,
    changed: Notify,
}

/// The server. Dropping it stops accepting connections.
pub struct FakeHttp {
    addr: std::net::SocketAddr,
    state: Arc<State>,
    stop: CancellationToken,
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl FakeHttp {
    /// Binds `127.0.0.1:0` and serves until dropped.
    pub async fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(State::default());
        let stop = CancellationToken::new();
        let (st, sp) = (state.clone(), stop.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = sp.cancelled() => break,
                    acc = listener.accept() => {
                        let Ok((sock, _)) = acc else { continue };
                        st.connections.fetch_add(1, Ordering::SeqCst);
                        st.changed.notify_waiters();
                        let (st2, sp2) = (st.clone(), sp.clone());
                        tokio::spawn(async move { serve(sock, st2, sp2).await });
                    }
                }
            }
        });
        Ok(Self { addr, state, stop })
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Adds a route (`method`, path suffix or CONNECT authority). Earlier routes win.
    pub fn route(&self, method: &str, suffix: &str, reply: Reply) -> &Self {
        self.push(method, suffix, reply, None)
    }
    /// Like [`route`](Self::route) but only for the next `n` matching requests.
    pub fn route_times(&self, method: &str, suffix: &str, reply: Reply, n: usize) -> &Self {
        self.push(method, suffix, reply, Some(n))
    }
    fn push(&self, method: &str, suffix: &str, reply: Reply, times: Option<usize>) -> &Self {
        if let Ok(mut r) = self.state.routes.lock() {
            r.push(Route {
                method: method.to_ascii_uppercase(),
                suffix: suffix.to_string(),
                reply,
                times,
            });
        }
        self
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state
            .requests
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default()
    }
    pub fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }
    /// Connections the client closed while a reply was pending or streaming.
    pub fn closed_early(&self) -> usize {
        self.state.closed_early.load(Ordering::SeqCst)
    }

    /// Waits until `n` requests were received (or `timeout`). Returns whether it happened.
    pub async fn wait_requests(&self, n: usize, timeout: Duration) -> bool {
        self.wait_until(timeout, || self.requests().len() >= n)
            .await
    }
    /// Waits until `n` connections were closed early by the client (or `timeout`).
    pub async fn wait_closed(&self, n: usize, timeout: Duration) -> bool {
        self.wait_until(timeout, || self.closed_early() >= n).await
    }

    async fn wait_until(&self, timeout: Duration, ok: impl Fn() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.state.changed.notified();
            if ok() {
                return true;
            }
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep_until(deadline) => return ok(),
                // Belt and braces against a missed wake-up.
                () = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
    }
}

fn take_route(state: &State, method: &str, target: &str) -> Option<Reply> {
    let mut routes = state.routes.lock().ok()?;
    let path = target.split('?').next().unwrap_or(target);
    let i = routes.iter().position(|r| {
        r.method == method && path.ends_with(&r.suffix) && r.times.is_none_or(|n| n > 0)
    })?;
    let reply = routes[i].reply.clone();
    if let Some(n) = routes[i].times.as_mut() {
        *n -= 1;
    }
    Some(reply)
}

/// Reads one request head + body. `None` on EOF / malformed input.
async fn read_request(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Option<Recorded> {
    let head_end = loop {
        if let Some(i) = find(buf, b"\r\n\r\n") {
            break i;
        }
        let mut tmp = [0u8; 4096];
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 1 << 20 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_ascii_uppercase();
    let target = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let body_start = head_end + 4;
    while buf.len() < body_start + len {
        let mut tmp = [0u8; 8192];
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[body_start..body_start + len]).to_string();
    buf.drain(..body_start + len);
    Some(Recorded {
        method,
        target,
        headers,
        body,
    })
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        407 => "Proxy Authentication Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Waits for the peer to close (read returns 0 / error). Used while hanging or streaming.
async fn peer_closed(sock: &mut tokio::net::tcp::ReadHalf<'_>) {
    let mut tmp = [0u8; 1024];
    loop {
        match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

async fn serve(mut sock: TcpStream, state: Arc<State>, stop: CancellationToken) {
    let mut buf = Vec::new();
    loop {
        let req = tokio::select! {
            () = stop.cancelled() => return,
            r = read_request(&mut sock, &mut buf) => r,
        };
        let Some(req) = req else { return };
        let reply = take_route(&state, &req.method, &req.target).unwrap_or_else(|| {
            Reply::json(
                404,
                serde_json::json!({"error": {"message": "no fixture for this route"}}),
            )
        });
        if let Ok(mut r) = state.requests.lock() {
            r.push(req);
        }
        state.changed.notify_waiters();
        let keep_going = match reply {
            Reply::Json {
                status,
                headers,
                body,
            } => {
                let b = body.to_string();
                write_simple(&mut sock, status, &headers, "application/json", &b).await
            }
            Reply::Raw {
                status,
                headers,
                content_type,
                body,
            } => write_simple(&mut sock, status, &headers, &content_type, &body).await,
            Reply::Hang => {
                let (mut rd, _wr) = sock.split();
                tokio::select! {
                    () = stop.cancelled() => {}
                    () = peer_closed(&mut rd) => {
                        state.closed_early.fetch_add(1, Ordering::SeqCst);
                        state.changed.notify_waiters();
                    }
                }
                false
            }
            Reply::Sse { blocks, delay } => {
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n";
                if sock.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                let (mut rd, mut wr) = sock.split();
                let send_all = async {
                    for b in blocks {
                        tokio::time::sleep(delay).await;
                        if wr.write_all(format!("{b}\n\n").as_bytes()).await.is_err() {
                            return false;
                        }
                        let _ = wr.flush().await;
                    }
                    true
                };
                tokio::select! {
                    () = stop.cancelled() => {}
                    done = send_all => {
                        if !done {
                            state.closed_early.fetch_add(1, Ordering::SeqCst);
                            state.changed.notify_waiters();
                        }
                    }
                    () = peer_closed(&mut rd) => {
                        state.closed_early.fetch_add(1, Ordering::SeqCst);
                        state.changed.notify_waiters();
                    }
                }
                false
            }
        };
        if !keep_going {
            return;
        }
    }
}

async fn write_simple(
    sock: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
    content_type: &str,
    body: &str,
) -> bool {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n",
        reason(status),
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes()).await.is_ok() && sock.write_all(body.as_bytes()).await.is_ok()
}
