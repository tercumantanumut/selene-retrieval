//! Daemon pipe protocol (docs/02-architecture.md §2.2, architect review D1). Owner: M1-C.
//!
//! Wire format on `\\.\pipe\sr-<sidhash>-<datadirhash>`:
//! 1. The client sends one [`Handshake`] as a single JSON line (`\n`-terminated, at most
//!    [`MAX_HANDSHAKE_BYTES`]) and waits.
//! 2. The daemon answers with one [`HandshakeReply`] line. Nothing else is sent before it, so
//!    neither side ever over-reads into the next protocol.
//! 3. `kind=mcp`: the rest of the stream is raw MCP JSON-RPC (the bridge is a byte proxy, so MCP
//!    version negotiation happens end to end). `kind=app`: length-prefixed frames
//!    (u32 little-endian length, then UTF-8 JSON, at most [`MAX_FRAME_BYTES`]) carrying
//!    JSON-RPC 2.0 messages ([`rpc`]).
//!
//! Version skew ([`decide`]): the daemon accepts `proto ∈ {N-1, N}`. An older client is refused
//! with `Unavailable{reason: upgrade}` (plain text: "Restart {client} to finish"). A newer client
//! (installed by an update) sets `drain_restart`; the old daemon answers
//! `Unavailable{reason: restarting}`, drains and exits, and the client launches the new daemon.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{ClientKind, ErrCtx, Error, ErrorCode, UserMessage};

/// One plain line for surfaces that show a single string (MCP error text, handshake refusals):
/// "Title. Body".
pub fn plain_text(m: &UserMessage) -> String {
    let t = m.title.trim_end();
    if t.ends_with(['.', '!', '?']) {
        format!("{t} {}", m.body)
    } else {
        format!("{t}. {}", m.body)
    }
}

/// Current protocol version. The daemon accepts `N-1` and `N`.
pub const PROTO_VERSION: u16 = 1;

/// Maximum size of the handshake line (and of the reply line) in bytes.
pub const MAX_HANDSHAKE_BYTES: usize = 4096;

/// Maximum size of one app-RPC frame payload in bytes.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// How long the daemon waits for the handshake line before dropping the connection.
pub const HANDSHAKE_TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnKind {
    /// Raw MCP JSON-RPC after the handshake (sr-bridge).
    Mcp,
    /// Length-prefixed app-RPC frames after the handshake (sr-app, admin tools).
    App,
}

/// First line every client sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Handshake {
    pub proto: u16,
    /// Client build (semver).
    pub build: String,
    /// `client_kind` in the architect review; both spellings are accepted.
    #[serde(alias = "client_kind")]
    pub kind: ConnKind,
    /// Which app is connecting (`claude`, `vscode`, `cursor`, `app`, ...). Used for plain text
    /// ("Restart Claude to finish") and diagnostics only; never for authorisation.
    pub client_id: String,
    /// Client process id (diagnostics only; the daemon also asks the kernel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// A client newer than the daemon asks it to drain and exit so the new build can start.
    #[serde(default)]
    pub drain_restart: bool,
}

impl Handshake {
    pub fn new(kind: ConnKind, client_id: impl Into<String>) -> Self {
        Self {
            proto: PROTO_VERSION,
            build: env!("CARGO_PKG_VERSION").to_string(),
            kind,
            client_id: client_id.into(),
            pid: Some(std::process::id()),
            drain_restart: true,
        }
    }

    /// One JSON line, `\n`-terminated.
    pub fn to_line(&self) -> Vec<u8> {
        to_line(self)
    }

    /// Parses a handshake line (with or without the trailing newline).
    pub fn parse(line: &[u8]) -> Result<Self, String> {
        parse_line(line)
    }
}

/// Why the daemon could not take the connection right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// The client is older than N-1: the user must restart the client app (`UpgradeRequired`).
    Upgrade,
    /// The client is newer and asked for a drain-restart: the daemon is exiting; re-attach.
    Restarting,
    /// The client is newer but did not ask for a restart.
    DaemonOutdated,
    /// The daemon is shutting down (updater drain, idle exit); re-attach.
    Draining,
}

/// The daemon's answer to a [`Handshake`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HandshakeReply {
    Accepted {
        proto: u16,
        min_proto: u16,
        build: String,
        daemon_pid: u32,
    },
    Unavailable {
        reason: UnavailableReason,
        code: ErrorCode,
        /// Plain, user-facing text (title + body of the code's `UserMessage`).
        message: String,
        proto: u16,
        min_proto: u16,
        build: String,
    },
    /// Malformed handshake (bad JSON, unknown kind, too long).
    Invalid { message: String },
}

impl HandshakeReply {
    pub fn to_line(&self) -> Vec<u8> {
        to_line(self)
    }
    pub fn parse(line: &[u8]) -> Result<Self, String> {
        parse_line(line)
    }
}

fn to_line<T: Serialize>(v: &T) -> Vec<u8> {
    // Serialising these plain structs cannot fail; fall back to an empty object just in case.
    let mut out = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    out.push(b'\n');
    out
}

fn parse_line<T: for<'de> Deserialize<'de>>(line: &[u8]) -> Result<T, String> {
    if line.len() > MAX_HANDSHAKE_BYTES {
        return Err("handshake line too long".into());
    }
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    serde_json::from_slice(line).map_err(|e| e.to_string())
}

/// Whether a daemon speaking `ours` accepts a client speaking `theirs` (N-1 and N).
pub const fn accepts(ours: u16, theirs: u16) -> bool {
    theirs == ours || (ours > 0 && theirs == ours - 1)
}

/// Oldest client protocol a daemon speaking `ours` accepts.
pub const fn min_accepted(ours: u16) -> u16 {
    ours.saturating_sub(1)
}

/// Version-skew decision (daemon side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkewDecision {
    Accept,
    /// Client older than N-1 → `Unavailable{upgrade}`.
    ClientTooOld,
    /// Client newer and asked to restart → `Unavailable{restarting}`, then drain and exit.
    DrainRestart,
    /// Client newer but did not ask for a restart → `Unavailable{daemon_outdated}`.
    ClientNewer,
}

/// Daemon-side skew rule for a daemon speaking `ours` and a client speaking `theirs`.
pub const fn decide(ours: u16, theirs: u16, drain_restart: bool) -> SkewDecision {
    if accepts(ours, theirs) {
        SkewDecision::Accept
    } else if theirs < ours {
        SkewDecision::ClientTooOld
    } else if drain_restart {
        SkewDecision::DrainRestart
    } else {
        SkewDecision::ClientNewer
    }
}

/// Maps a handshake `client_id` to a known client kind for user-facing text.
pub fn client_kind_from_id(client_id: &str) -> ClientKind {
    let id = client_id.to_ascii_lowercase();
    if id.starts_with("claude") {
        ClientKind::ClaudeDesktop
    } else if id.starts_with("vscode") || id.starts_with("code") {
        ClientKind::VsCode
    } else if id.starts_with("cursor") {
        ClientKind::Cursor
    } else {
        ClientKind::Other
    }
}

impl UnavailableReason {
    /// The error a client surfaces when it cannot recover from this reason by itself.
    pub fn to_error(self, client_id: &str) -> Error {
        let code = match self {
            UnavailableReason::Upgrade => ErrorCode::UpgradeRequired,
            UnavailableReason::Restarting
            | UnavailableReason::DaemonOutdated
            | UnavailableReason::Draining => ErrorCode::ServiceUnavailable,
        };
        Error::new(code).with_ctx(ErrCtx {
            client: Some(client_kind_from_id(client_id)),
            detail: Some(format!("pipe handshake refused: {self:?}")),
            ..ErrCtx::default()
        })
    }

    /// Plain text carried in [`HandshakeReply::Unavailable`]`::message`.
    pub fn plain_message(self, client_id: &str) -> String {
        plain_text(&self.to_error(client_id).user_message())
    }
}

/// App-RPC: JSON-RPC 2.0 envelopes carried in length-prefixed frames after a `kind=app`
/// handshake. Methods under `host.*` and `events.*` are served by the pipe server itself;
/// everything else is forwarded to the engine's app-RPC service.
pub mod rpc {
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    use super::MAX_FRAME_BYTES;
    use crate::error::{Error, ErrorCode};
    use crate::events::{Cursor, Snapshot};

    pub const JSONRPC: &str = "2.0";

    /// `host.ping` → [`PingResult`].
    pub const PING: &str = "host.ping";
    /// `host.shutdown {drain}` → `null`; the daemon stops accepting, drains and exits.
    pub const SHUTDOWN: &str = "host.shutdown";
    /// `events.subscribe {cursor?}` → [`SubscribeResult`], then `events.event` notifications.
    pub const SUBSCRIBE: &str = "events.subscribe";
    /// `events.unsubscribe` → `null`.
    pub const UNSUBSCRIBE: &str = "events.unsubscribe";
    /// Notification: one `Seq<EngineEvent>` (params); `seq` strictly increasing, no gaps.
    pub const EVENT: &str = "events.event";
    /// Notification: the subscriber fell behind the replay ring; params = fresh `Snapshot`.
    /// Events with `seq > snapshot.seq` follow.
    pub const RESNAPSHOT: &str = "events.resnapshot";

    /// Standard JSON-RPC codes.
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    /// Application error; `data` carries the [`ErrorCode`].
    pub const APP_ERROR: i32 = -32000;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct Request {
        pub jsonrpc: String,
        pub id: u64,
        pub method: String,
        #[serde(default)]
        pub params: Value,
    }

    impl Request {
        pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
            Self {
                jsonrpc: JSONRPC.into(),
                id,
                method: method.into(),
                params,
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct Response {
        pub jsonrpc: String,
        pub id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub error: Option<RpcError>,
    }

    impl Response {
        pub fn ok(id: u64, result: Value) -> Self {
            Self {
                jsonrpc: JSONRPC.into(),
                id,
                result: Some(result),
                error: None,
            }
        }
        pub fn err(id: u64, error: RpcError) -> Self {
            Self {
                jsonrpc: JSONRPC.into(),
                id,
                result: None,
                error: Some(error),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct Notification {
        pub jsonrpc: String,
        pub method: String,
        #[serde(default)]
        pub params: Value,
    }

    impl Notification {
        pub fn new(method: impl Into<String>, params: Value) -> Self {
            Self {
                jsonrpc: JSONRPC.into(),
                method: method.into(),
                params,
            }
        }
    }

    /// What a client reads from the daemon.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(untagged)]
    pub enum Incoming {
        Response(Response),
        Notification(Notification),
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct RpcError {
        pub code: i32,
        /// Plain text (UserMessage title + body for application errors).
        pub message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub data: Option<RpcErrorData>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct RpcErrorData {
        pub code: ErrorCode,
        pub retryable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub retry_after_s: Option<u64>,
        /// Display context for the message ("{name}", "{other}", "{limit}", …; M2-A R4), so the
        /// app renders the same text as the daemon. Never the internal detail; a path only for the
        /// codes whose text names the folder the user picked (ApprovalRequired, PathRefused).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ctx: Option<Box<crate::error::ErrCtx>>,
    }

    impl RpcError {
        pub fn new(code: i32, message: impl Into<String>) -> Self {
            Self {
                code,
                message: message.into(),
                data: None,
            }
        }

        /// Application error from the one error model.
        pub fn from_error(e: &Error) -> Self {
            Self {
                code: if e.code == ErrorCode::InvalidInput {
                    INVALID_PARAMS
                } else {
                    APP_ERROR
                },
                message: super::plain_text(&e.user_message()),
                data: Some(RpcErrorData {
                    code: e.code,
                    retryable: e.retryable,
                    retry_after_s: e.retry_after.map(|d| d.as_secs()),
                    ctx: display_ctx(e),
                }),
            }
        }

        /// Back into the one error model (client side).
        pub fn to_error(&self) -> Error {
            match &self.data {
                Some(d) => {
                    let mut e = Error::new(d.code);
                    if let Some(c) = &d.ctx {
                        e.ctx = c.clone();
                    }
                    e.retryable = d.retryable;
                    if let Some(s) = d.retry_after_s {
                        e = e.with_retry_after(u32::try_from(s).unwrap_or(u32::MAX));
                    }
                    e.with_detail(self.message.clone())
                }
                None => {
                    let code = match self.code {
                        INVALID_PARAMS | INVALID_REQUEST => ErrorCode::InvalidInput,
                        _ => ErrorCode::Internal,
                    };
                    Error::new(code).with_detail(format!("rpc {}: {}", self.code, self.message))
                }
            }
        }
    }

    /// The display fields of an error's context (M2-A R4). `detail` stays in the daemon's log;
    /// `path` travels only for the codes whose text names the user's own pick.
    fn display_ctx(e: &Error) -> Option<Box<crate::error::ErrCtx>> {
        let mut c = e.ctx.clone();
        c.detail = None;
        if !matches!(e.code, ErrorCode::ApprovalRequired | ErrorCode::PathRefused) {
            c.path = None;
        }
        (*c != crate::error::ErrCtx::default()).then_some(c)
    }

    /// `events.subscribe` params. `cursor = None` starts with a snapshot.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct SubscribeParams {
        #[serde(default)]
        pub cursor: Option<Cursor>,
    }

    /// `events.subscribe` result.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    #[serde(tag = "mode", rename_all = "snake_case")]
    pub enum SubscribeResult {
        /// The cursor is still replayable: every event after it follows, then live events.
        Replay { cursor: Cursor },
        /// Fresh start, other daemon run, or the ring moved on: apply this snapshot; events with
        /// `seq > snapshot.seq` follow.
        Snapshot { snapshot: Snapshot<Value> },
    }

    /// `host.shutdown` params.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct ShutdownParams {
        /// Finish the current batch and commit first (updater path). Default true.
        #[serde(default = "yes")]
        pub drain: bool,
    }

    fn yes() -> bool {
        true
    }

    /// `host.ping` result.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct PingResult {
        pub daemon_pid: u32,
        pub proto: u16,
        pub build: String,
        pub sessions: u32,
        pub cursor: Cursor,
    }

    /// Frame header for a payload of `len` bytes.
    pub fn frame_header(len: usize) -> Result<[u8; 4], String> {
        if len > MAX_FRAME_BYTES {
            return Err(format!("frame of {len} bytes exceeds {MAX_FRAME_BYTES}"));
        }
        u32::try_from(len)
            .map(u32::to_le_bytes)
            .map_err(|e| e.to_string())
    }

    /// Payload length from a frame header.
    pub fn frame_len(header: [u8; 4]) -> Result<usize, String> {
        let n = usize::try_from(u32::from_le_bytes(header)).map_err(|e| e.to_string())?;
        if n > MAX_FRAME_BYTES {
            return Err(format!("frame of {n} bytes exceeds {MAX_FRAME_BYTES}"));
        }
        Ok(n)
    }

    /// Header + payload in one buffer (one write per frame keeps frames contiguous).
    pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, String> {
        let body = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity(body.len() + 4);
        out.extend_from_slice(&frame_header(body.len())?);
        out.extend_from_slice(&body);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::rpc::*;
    use super::*;
    use crate::events::{Cursor, Snapshot};
    use crate::ids::StreamId;

    #[test]
    fn skew_rules() {
        assert_eq!(decide(5, 5, false), SkewDecision::Accept);
        assert_eq!(decide(5, 4, true), SkewDecision::Accept);
        assert_eq!(decide(5, 3, true), SkewDecision::ClientTooOld);
        assert_eq!(decide(5, 0, false), SkewDecision::ClientTooOld);
        assert_eq!(decide(5, 6, true), SkewDecision::DrainRestart);
        assert_eq!(decide(5, 6, false), SkewDecision::ClientNewer);
        assert_eq!(decide(0, 1, true), SkewDecision::DrainRestart);
        assert_eq!(min_accepted(0), 0);
        assert_eq!(min_accepted(PROTO_VERSION), PROTO_VERSION - 1);
    }

    #[test]
    fn handshake_line_roundtrip_and_alias() {
        let h = Handshake::new(ConnKind::Mcp, "claude");
        let line = h.to_line();
        assert_eq!(line.last(), Some(&b'\n'));
        assert_eq!(Handshake::parse(&line).unwrap(), h);
        // Architect-review spelling + missing optional fields (an older client).
        let old = br#"{"proto":1,"build":"0.1.0","client_kind":"app","client_id":"app"}"#;
        let p = Handshake::parse(old).unwrap();
        assert_eq!(p.kind, ConnKind::App);
        assert!(!p.drain_restart);
        assert!(
            Handshake::parse(br#"{"proto":1,"build":"x","kind":"bogus","client_id":"a"}"#).is_err()
        );
        assert!(Handshake::parse(&vec![b' '; MAX_HANDSHAKE_BYTES + 1]).is_err());
    }

    #[test]
    fn replies_are_tagged_and_plain() {
        let r = HandshakeReply::Unavailable {
            reason: UnavailableReason::Upgrade,
            code: ErrorCode::UpgradeRequired,
            message: UnavailableReason::Upgrade.plain_message("claude"),
            proto: 3,
            min_proto: 2,
            build: "0.1.0".into(),
        };
        let v: serde_json::Value = serde_json::from_slice(&r.to_line()).unwrap();
        assert_eq!(v["status"], "unavailable");
        assert_eq!(v["reason"], "upgrade");
        assert_eq!(
            v["message"],
            "Finish updating. Selene Retrieval was updated. Restart Claude to finish."
        );
        assert_eq!(HandshakeReply::parse(&r.to_line()).unwrap(), r);
        assert!(
            !UnavailableReason::Restarting
                .plain_message("x")
                .contains("daemon")
        );
    }

    #[test]
    fn rpc_envelopes() {
        let req = Request::new(7, SUBSCRIBE, serde_json::json!({"cursor": null}));
        let f = encode_frame(&req).unwrap();
        let n = frame_len([f[0], f[1], f[2], f[3]]).unwrap();
        assert_eq!(n, f.len() - 4);
        let back: Request = serde_json::from_slice(&f[4..]).unwrap();
        assert_eq!(back, req);
        assert!(frame_len(u32::MAX.to_le_bytes()).is_err());

        let resp: Incoming =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":7,"result":1}"#).unwrap();
        assert!(matches!(resp, Incoming::Response(r) if r.id == 7));
        let note: Incoming =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"events.event","params":{}}"#)
                .unwrap();
        assert!(matches!(note, Incoming::Notification(n) if n.method == EVENT));

        let e = Error::new(ErrorCode::Busy).with_retry_after(30);
        let re = RpcError::from_error(&e);
        assert_eq!(re.code, APP_ERROR);
        let back = re.to_error();
        assert_eq!(back.code, ErrorCode::Busy);
        assert_eq!(back.retry_after, e.retry_after);

        let cur = Cursor {
            stream: StreamId::new(),
            seq: 4,
        };
        let v = serde_json::to_value(SubscribeResult::Replay { cursor: cur }).unwrap();
        assert_eq!(v["mode"], "replay");
        let snap = SubscribeResult::Snapshot {
            snapshot: Snapshot {
                stream: cur.stream,
                seq: 4,
                value: serde_json::json!({"x": 1}),
            },
        };
        let back: SubscribeResult =
            serde_json::from_value(serde_json::to_value(&snap).unwrap()).unwrap();
        assert_eq!(back, snap);
        let sp: ShutdownParams = serde_json::from_str("{}").unwrap();
        assert!(sp.drain);
    }
}
