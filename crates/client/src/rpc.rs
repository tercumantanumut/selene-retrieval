//! App-RPC client (`kind=app`): requests, event subscription with seq resume, and a
//! [`Subscription`] that survives daemon restarts.
//!
//! Frames: u32 LE length + JSON (sr_common::protocol::rpc). One reader task demultiplexes
//! responses (by id) and `events.*` notifications. Backpressure: the notification channel is
//! bounded, so a slow consumer slows the reader, the daemon's writes, and finally lags its
//! broadcast receiver, which the daemon answers with `events.resnapshot` (never a silent gap).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sr_common::error::{Error, ErrorCode};
use sr_common::events::{Cursor, Seq, Snapshot};
use sr_common::protocol::ConnKind;
use sr_common::protocol::rpc::{
    self, Incoming, Notification, PingResult, Request, ShutdownParams, SubscribeParams,
    SubscribeResult,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::attach::{AttachOptions, Connection, attach};
use crate::sys::Stream;

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<rpc::Response>>>>;

/// macOS (R50): `host.challenge` returns `{nonce}`; the same process then answers with
/// `host.attest {nonce}`, and the daemon trusts the session if that process is our own app
/// (`sr_engine::pipe`). Other platforms don't serve these methods.
pub const HOST_CHALLENGE: &str = "host.challenge";
/// See [`HOST_CHALLENGE`]. Returns `{trusted}`.
pub const HOST_ATTEST: &str = "host.attest";

/// Reads one frame. `Ok(None)` on clean EOF at a frame boundary.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut h = [0u8; 4];
    match r.read_exact(&mut h).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = rpc::frame_len(h).map_err(std::io::Error::other)?;
    let mut body = vec![0u8; n];
    r.read_exact(&mut body).await?;
    Ok(Some(body))
}

/// Writes one frame (header + body in one write).
pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
) -> std::io::Result<()> {
    let buf = rpc::encode_frame(msg).map_err(std::io::Error::other)?;
    w.write_all(&buf).await?;
    w.flush().await
}

fn disconnected() -> Error {
    Error::new(ErrorCode::ServiceUnavailable).with_detail("connection to the service closed")
}

/// A connected app-RPC client.
pub struct AppClient {
    writer: Arc<Mutex<WriteHalf<Stream>>>,
    pending: Pending,
    next_id: AtomicU64,
    notes: std::sync::Mutex<Option<mpsc::Receiver<Notification>>>,
    reader: tokio::task::JoinHandle<()>,
    pub daemon_pid: u32,
    pub daemon_proto: u16,
}

impl Drop for AppClient {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl AppClient {
    /// Attach (launching the daemon if needed) as `kind=app`.
    pub async fn connect(opts: &AttachOptions) -> Result<Self, Error> {
        let mut o = opts.clone();
        o.kind = ConnKind::App;
        let c = Self::from_connection(attach(&o).await?);
        #[cfg(target_os = "macos")]
        c.attest().await;
        Ok(c)
    }

    /// macOS (R50): proves to the daemon that this process holds the connection, so our own
    /// app gets its privileged calls. Best effort: on failure the session stays untrusted and
    /// privileged calls are refused with the plain "only in the app" error.
    #[cfg(target_os = "macos")]
    async fn attest(&self) {
        let Ok(v) = self.request(HOST_CHALLENGE, serde_json::json!({})).await else {
            return;
        };
        if let Some(nonce) = v.get("nonce").and_then(Value::as_str) {
            let _ = self
                .request(HOST_ATTEST, serde_json::json!({ "nonce": nonce }))
                .await;
        }
    }

    /// Wraps an attached `kind=app` connection.
    pub fn from_connection(conn: Connection) -> Self {
        let (r, w) = tokio::io::split(conn.stream);
        let pending: Pending = Arc::default();
        let (tx, rx) = mpsc::channel(1024);
        let reader = tokio::spawn(read_loop(r, pending.clone(), tx));
        Self {
            writer: Arc::new(Mutex::new(w)),
            pending,
            next_id: AtomicU64::new(1),
            notes: std::sync::Mutex::new(Some(rx)),
            reader,
            daemon_pid: conn.daemon_pid,
            daemon_proto: conn.daemon_proto,
        }
    }

    /// Raw request.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);
        {
            let mut w = self.writer.lock().await;
            if let Err(e) = write_frame(&mut *w, &Request::new(id, method, params)).await {
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id);
                return Err(disconnected().with_source(e));
            }
        }
        let resp = rx.await.map_err(|_| disconnected())?;
        match (resp.result, resp.error) {
            (_, Some(e)) => Err(e.to_error()),
            (Some(v), None) => Ok(v),
            (None, None) => Ok(Value::Null),
        }
    }

    /// Typed request.
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, Error> {
        let p = serde_json::to_value(params)
            .map_err(|e| Error::new(ErrorCode::InvalidInput).with_source(e))?;
        let v = self.request(method, p).await?;
        serde_json::from_value(v).map_err(|e| {
            Error::new(ErrorCode::Internal)
                .with_detail(format!("bad {method} result"))
                .with_source(e)
        })
    }

    pub async fn ping(&self) -> Result<PingResult, Error> {
        self.call(rpc::PING, &serde_json::json!({})).await
    }

    /// `host.shutdown{drain}` (updater path). The daemon stops accepting, drains and exits.
    pub async fn shutdown(&self, drain: bool) -> Result<(), Error> {
        self.request(
            rpc::SHUTDOWN,
            serde_json::to_value(ShutdownParams { drain }).unwrap_or_default(),
        )
        .await
        .map(|_| ())
    }

    /// Subscribes to engine events from `cursor` (or from a fresh snapshot when `None`).
    /// Only one subscription per connection.
    pub async fn subscribe(&self, cursor: Option<Cursor>) -> Result<EventStream, Error> {
        let rx = self
            .notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| Error::new(ErrorCode::InvalidInput).with_detail("already subscribed"))?;
        let res: SubscribeResult = self
            .call(rpc::SUBSCRIBE, &SubscribeParams { cursor })
            .await?;
        let (start, first) = match res {
            SubscribeResult::Replay { cursor } => (cursor, None),
            SubscribeResult::Snapshot { snapshot } => {
                (snapshot.cursor(), Some(StreamItem::Snapshot(snapshot)))
            }
        };
        Ok(EventStream {
            rx,
            cursor: start,
            first,
        })
    }
}

async fn read_loop(mut r: ReadHalf<Stream>, pending: Pending, notes: mpsc::Sender<Notification>) {
    loop {
        let frame = match read_frame(&mut r).await {
            Ok(Some(f)) => f,
            Ok(None) | Err(_) => break,
        };
        match serde_json::from_slice::<Incoming>(&frame) {
            Ok(Incoming::Response(resp)) => {
                let tx = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&resp.id);
                if let Some(tx) = tx {
                    let _ = tx.send(resp);
                }
            }
            Ok(Incoming::Notification(n)) => {
                // Receiver gone (not subscribed / dropped): ignore.
                let _ = notes.send(n).await;
            }
            Err(_) => break,
        }
    }
    // Fail every in-flight request: dropping the senders wakes them with an error.
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// One item of an event stream.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
    /// Replace local state with this snapshot; later events have `seq > snapshot.seq`.
    Snapshot(Snapshot<Value>),
    /// The next event (`seq == cursor.seq + 1`).
    Event(Seq<Value>),
}

/// Events of one subscription on one connection. Ends (`None`) when the connection drops.
pub struct EventStream {
    rx: mpsc::Receiver<Notification>,
    cursor: Cursor,
    first: Option<StreamItem>,
}

impl EventStream {
    /// Last applied position (pass it to `subscribe` to resume).
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Next item. Duplicates (`seq <= cursor`) are skipped. A gap is reported as an error so the
    /// caller resubscribes from its cursor (the daemon never sends one; this is a safety net).
    pub async fn next(&mut self) -> Option<Result<StreamItem, Error>> {
        if let Some(f) = self.first.take() {
            return Some(Ok(f));
        }
        loop {
            let n = self.rx.recv().await?;
            match n.method.as_str() {
                rpc::EVENT => {
                    let ev: Seq<Value> = match serde_json::from_value(n.params) {
                        Ok(e) => e,
                        Err(e) => {
                            return Some(Err(Error::new(ErrorCode::Internal).with_source(e)));
                        }
                    };
                    if ev.seq <= self.cursor.seq {
                        continue;
                    }
                    if ev.seq != self.cursor.seq + 1 {
                        return Some(Err(Error::new(ErrorCode::Internal).with_detail(format!(
                            "event gap: have {}, got {}",
                            self.cursor.seq, ev.seq
                        ))));
                    }
                    self.cursor.seq = ev.seq;
                    return Some(Ok(StreamItem::Event(ev)));
                }
                rpc::RESNAPSHOT => {
                    let s: Snapshot<Value> = match serde_json::from_value(n.params) {
                        Ok(s) => s,
                        Err(e) => {
                            return Some(Err(Error::new(ErrorCode::Internal).with_source(e)));
                        }
                    };
                    self.cursor = s.cursor();
                    return Some(Ok(StreamItem::Snapshot(s)));
                }
                _ => continue,
            }
        }
    }
}

/// Item of a resuming [`Subscription`].
#[derive(Debug, Clone, PartialEq)]
pub enum SubItem {
    Snapshot(Snapshot<Value>),
    Event(Seq<Value>),
    /// The connection dropped and was re-established (daemon restart, update). The next item is
    /// either the missed events (same daemon run) or a snapshot (new run).
    Reconnected {
        daemon_pid: u32,
    },
}

/// An event subscription that re-attaches (launching the daemon if needed) and resumes from its
/// cursor whenever the connection drops. Used by the app's Tauri channel bridge.
pub struct Subscription {
    opts: AttachOptions,
    client: Option<AppClient>,
    stream: Option<EventStream>,
    cursor: Option<Cursor>,
    reconnects: u32,
}

impl Subscription {
    /// Attach and subscribe from `cursor` (`None` = start with a snapshot).
    pub async fn start(opts: AttachOptions, cursor: Option<Cursor>) -> Result<Self, Error> {
        let mut s = Self {
            opts,
            client: None,
            stream: None,
            cursor,
            reconnects: 0,
        };
        s.connect().await?;
        Ok(s)
    }

    async fn connect(&mut self) -> Result<(), Error> {
        let c = AppClient::connect(&self.opts).await?;
        let st = c.subscribe(self.cursor).await?;
        self.client = Some(c);
        self.stream = Some(st);
        Ok(())
    }

    pub fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }

    pub fn client(&self) -> Option<&AppClient> {
        self.client.as_ref()
    }

    pub fn reconnects(&self) -> u32 {
        self.reconnects
    }

    /// Next item; transparently reconnects. Returns `Err` only when re-attaching failed
    /// (`ServiceUnavailable`); calling `next` again retries.
    pub async fn next(&mut self) -> Result<SubItem, Error> {
        if let Some(st) = self.stream.as_mut() {
            match st.next().await {
                Some(Ok(item)) => {
                    self.cursor = Some(st.cursor());
                    return Ok(match item {
                        StreamItem::Snapshot(s) => SubItem::Snapshot(s),
                        StreamItem::Event(e) => SubItem::Event(e),
                    });
                }
                Some(Err(_)) | None => {
                    // Gap or disconnect: drop the connection and resume from the cursor.
                    self.stream = None;
                    self.client = None;
                }
            }
        }
        let mut delay = Duration::from_millis(50);
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.connect().await {
                Ok(()) => break,
                Err(e) if attempt >= 3 => return Err(e),
                Err(_) => {
                    tokio::time::sleep(delay).await;
                    delay *= 4;
                }
            }
        }
        self.reconnects += 1;
        let pid = self.client.as_ref().map_or(0, |c| c.daemon_pid);
        Ok(SubItem::Reconnected { daemon_pid: pid })
    }
}
