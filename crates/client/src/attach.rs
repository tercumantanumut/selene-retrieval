//! Attach sequence (docs/02-architecture.md §2.2, architect review D1, Amendment 3.9):
//!
//! 1. Open the pipe. `ERROR_PIPE_BUSY` means a daemon is serving: wait for a free instance.
//! 2. `ERROR_FILE_NOT_FOUND`: take `LockFileEx(spawn.lock)` (polled, so a client that sees the
//!    pipe appear while waiting connects at once) and re-check the pipe.
//! 3. Still absent: run the short-lived `sr-launch` (breakaway spawn verified with
//!    `IsProcessInJob`, else the per-user scheduled task). It exits as soon as the daemon is
//!    started, so the daemon's parent is dead and `taskkill /T` on the client tree can't reach it.
//! 4. Poll the pipe with backoff (5 ms → 200 ms) for at most `timeout` (10 s). If the pipe is still
//!    absent 3 s after a launch, launch again (host.lock + `FIRST_PIPE_INSTANCE` make a duplicate
//!    daemon exit at once, so this is always safe).
//! 5. Handshake. `Unavailable{restarting|draining}` (an old daemon draining for us) loops back to
//!    step 1 until the new daemon answers.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use sr_common::error::{Error, ErrorCode};
use sr_common::protocol::{
    ConnKind, Handshake, HandshakeReply, MAX_HANDSHAKE_BYTES, PROTO_VERSION, UnavailableReason,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

use crate::naming::{self, SPAWN_LOCK};
use crate::win::{self, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, OwnedProcess};

/// How long a client waits for the daemon's pipe after launching it.
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);
/// Relaunch if the pipe hasn't appeared this long after a launch.
const RELAUNCH_AFTER: Duration = Duration::from_secs(3);
/// At most this many launcher runs per attach.
const MAX_LAUNCHES: u32 = 3;
/// How long the launcher may take (task self-registration measured 1.26 s).
const LAUNCHER_TIMEOUT: Duration = Duration::from_secs(15);
/// Handshake reply deadline.
const HANDSHAKE_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Attach options.
#[derive(Debug, Clone)]
pub struct AttachOptions {
    pub kind: ConnKind,
    /// `claude`, `vscode`, `cursor`, `app`, ... (plain text and diagnostics only).
    pub client_id: String,
    /// `None` = the default data dir (`%LOCALAPPDATA%\SeleneRetrieval`).
    pub data_dir: Option<PathBuf>,
    /// Budget for the pipe to appear after a launch (and for a busy pipe to free an instance).
    pub timeout: Duration,
    /// Launch the daemon when absent (false = connect-only, e.g. a status probe).
    pub launch: bool,
    /// `None` = `sr-launch.exe` next to the current exe.
    pub launcher: Option<PathBuf>,
    /// Protocol version sent in the handshake (tests override it).
    pub proto: u16,
    /// Ask an older daemon to drain and restart (only clients that can launch a daemon).
    pub drain_restart: bool,
}

impl AttachOptions {
    pub fn new(kind: ConnKind, client_id: impl Into<String>) -> Self {
        Self {
            kind,
            client_id: client_id.into(),
            data_dir: None,
            timeout: ATTACH_TIMEOUT,
            launch: true,
            launcher: None,
            proto: PROTO_VERSION,
            drain_restart: true,
        }
    }

    pub fn handshake(&self) -> Handshake {
        let mut h = Handshake::new(self.kind, self.client_id.clone());
        h.proto = self.proto;
        h.drain_restart = self.drain_restart && self.launch;
        h
    }
}

/// How the connection was obtained (diagnostics and tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachPath {
    /// The daemon was already serving.
    Direct,
    /// The daemon was serving but every instance was busy for a moment.
    Busy,
    /// Another client launched the daemon while we waited for `spawn.lock`.
    AfterSpawnLock,
    /// We launched it. `launcher_exit` is the last `sr-launch` exit code (see `launch::code`).
    Launched { launcher_exit: u32, launches: u32 },
}

/// A connected, handshaken pipe.
#[derive(Debug)]
pub struct Connection {
    pub stream: NamedPipeClient,
    pub pipe_name: String,
    pub data_dir: PathBuf,
    pub daemon_pid: u32,
    pub daemon_proto: u16,
    pub daemon_build: String,
    pub path: AttachPath,
    /// Drain-restart rounds that were needed (0 normally).
    pub restarts: u32,
    pub elapsed: Duration,
}

fn unavailable(detail: impl Into<String>) -> Error {
    Error::new(ErrorCode::ServiceUnavailable).with_detail(detail)
}

fn io_unavailable(what: &str, e: io::Error) -> Error {
    unavailable(format!("{what}: {e}")).with_source(e)
}

fn try_open(name: &str) -> io::Result<NamedPipeClient> {
    ClientOptions::new().open(name)
}

enum Probe {
    Open(NamedPipeClient),
    Busy,
    Absent,
}

fn probe(name: &str) -> Result<Probe, Error> {
    match try_open(name) {
        Ok(c) => Ok(Probe::Open(c)),
        Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => Ok(Probe::Busy),
        Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => Ok(Probe::Absent),
        Err(e) => Err(io_unavailable("open pipe", e)),
    }
}

/// Backoff that doubles from 5 ms to 200 ms.
struct Backoff(Duration);
impl Backoff {
    fn new() -> Self {
        Self(Duration::from_millis(5))
    }
    async fn wait(&mut self) {
        tokio::time::sleep(self.0).await;
        self.0 = (self.0 * 2).min(Duration::from_millis(200));
    }
}

/// While the pipe is busy, waits for a free instance (up to `deadline`). Returns `None` as soon
/// as the pipe disappears (an old daemon finished draining), so the caller can launch.
async fn wait_busy(name: &str, deadline: Instant) -> Result<Option<NamedPipeClient>, Error> {
    let mut b = Backoff::new();
    loop {
        match probe(name)? {
            Probe::Open(c) => return Ok(Some(c)),
            Probe::Absent => return Ok(None),
            Probe::Busy => {}
        }
        if Instant::now() >= deadline {
            return Err(unavailable(
                "timed out waiting for a free service pipe instance",
            ));
        }
        b.wait().await;
    }
}

/// Runs `sr-launch` and waits for it (it is short-lived). Returns its exit code.
async fn run_launcher(
    launcher: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<u32, Error> {
    let args = format!("--data-dir {}", win::quote_arg(&data_dir.to_string_lossy()));
    let p = OwnedProcess::spawn(
        launcher,
        &args,
        CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP,
        false,
        launcher.parent(),
    )
    .map_err(|e| io_unavailable("start sr-launch", e))?;
    match p.wait_async(LAUNCHER_TIMEOUT).await {
        Ok(Some(code)) => Ok(code),
        Ok(None) => {
            let _ = p.terminate(1);
            Err(unavailable("sr-launch did not finish"))
        }
        Err(e) => Err(io_unavailable("wait for sr-launch", e)),
    }
}

fn launcher_path(opts: &AttachOptions) -> Result<PathBuf, Error> {
    if let Some(p) = &opts.launcher {
        return Ok(p.clone());
    }
    if let Some(p) = naming::dev_env("SR_LAUNCH_EXE") {
        return Ok(PathBuf::from(p));
    }
    naming::sibling_exe("sr-launch.exe")
}

/// Steps 1–4: an open pipe instance (not yet handshaken).
async fn open_or_launch(
    opts: &AttachOptions,
    name: &str,
    data_dir: &std::path::Path,
) -> Result<(NamedPipeClient, AttachPath), Error> {
    // 1. Fast path.
    match probe(name)? {
        Probe::Open(c) => return Ok((c, AttachPath::Direct)),
        Probe::Busy => {
            if let Some(c) = wait_busy(name, Instant::now() + opts.timeout).await? {
                return Ok((c, AttachPath::Busy));
            }
        }
        Probe::Absent => {}
    }
    if !opts.launch {
        return Err(unavailable("service not running (connect-only)"));
    }
    // 2. Serialise spawners. Poll the lock so the pipe appearing meanwhile wins at once.
    let lock_path = data_dir.join(SPAWN_LOCK);
    let lock_deadline = Instant::now() + opts.timeout + LAUNCHER_TIMEOUT;
    let mut b = Backoff::new();
    let lock = loop {
        match probe(name)? {
            Probe::Open(c) => return Ok((c, AttachPath::AfterSpawnLock)),
            Probe::Busy => {
                if let Some(c) = wait_busy(name, Instant::now() + opts.timeout).await? {
                    return Ok((c, AttachPath::AfterSpawnLock));
                }
            }
            Probe::Absent => {}
        }
        match win::lock_file(&lock_path, false) {
            Ok(Some(f)) => break f,
            Ok(None) => {}
            Err(e) => return Err(io_unavailable("spawn.lock", e)),
        }
        if Instant::now() >= lock_deadline {
            return Err(unavailable(
                "timed out waiting for another client to start the service",
            ));
        }
        b.wait().await;
    };
    // Re-check under the lock: the previous holder may have just finished.
    match probe(name)? {
        Probe::Open(c) => return Ok((c, AttachPath::AfterSpawnLock)),
        Probe::Busy => {
            if let Some(c) = wait_busy(name, Instant::now() + opts.timeout).await? {
                return Ok((c, AttachPath::AfterSpawnLock));
            }
        }
        Probe::Absent => {}
    }
    // 3. We are the spawner.
    let launcher = launcher_path(opts)?;
    let mut launches = 0u32;
    let mut last_exit = 0u32;
    let mut last_launch = Instant::now();
    // The pipe-poll budget starts when a launcher run *exits* (the task path can take seconds).
    let mut deadline = Instant::now() + opts.timeout;
    let mut launch_err: Option<Error> = None;
    let mut b = Backoff::new();
    loop {
        if launches == 0 || (last_launch.elapsed() >= RELAUNCH_AFTER && launches < MAX_LAUNCHES) {
            launches += 1;
            let r = run_launcher(&launcher, data_dir).await;
            // Count from the launcher's exit: the task path alone can take seconds under load.
            last_launch = Instant::now();
            if launches == 1 {
                deadline = last_launch + opts.timeout;
            }
            match r {
                Ok(code) => {
                    last_exit = code;
                    if !crate::launch::code::is_started(code) {
                        launch_err = Some(unavailable(format!(
                            "sr-launch could not start the service (exit {code})"
                        )));
                    }
                }
                Err(e) => launch_err = Some(e),
            }
        }
        // 4. Poll for the pipe.
        match probe(name)? {
            Probe::Open(c) => {
                drop(lock);
                return Ok((
                    c,
                    AttachPath::Launched {
                        launcher_exit: last_exit,
                        launches,
                    },
                ));
            }
            Probe::Busy => {
                let until = deadline.max(Instant::now() + Duration::from_secs(1));
                if let Some(c) = wait_busy(name, until).await? {
                    drop(lock);
                    return Ok((
                        c,
                        AttachPath::Launched {
                            launcher_exit: last_exit,
                            launches,
                        },
                    ));
                }
            }
            Probe::Absent => {}
        }
        if Instant::now() >= deadline {
            return Err(launch_err.unwrap_or_else(|| {
                unavailable(format!(
                    "the service did not open its pipe within {} s",
                    opts.timeout.as_secs()
                ))
            }));
        }
        b.wait().await;
    }
}

/// Sends the handshake and reads the one-line reply (byte by byte: nothing may be over-read).
pub async fn handshake(
    pipe: &mut NamedPipeClient,
    hs: &Handshake,
) -> Result<HandshakeReply, Error> {
    let io = async {
        pipe.write_all(&hs.to_line()).await?;
        pipe.flush().await?;
        let mut line = Vec::with_capacity(160);
        let mut b = [0u8; 1];
        loop {
            if pipe.read(&mut b).await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "service closed the pipe during the handshake",
                ));
            }
            if b[0] == b'\n' {
                return Ok(line);
            }
            line.push(b[0]);
            if line.len() > MAX_HANDSHAKE_BYTES {
                return Err(io::Error::other("handshake reply too long"));
            }
        }
    };
    let line = tokio::time::timeout(HANDSHAKE_REPLY_TIMEOUT, io)
        .await
        .map_err(|_| unavailable("handshake reply timed out"))?
        .map_err(|e| io_unavailable("handshake", e))?;
    HandshakeReply::parse(&line).map_err(|e| unavailable(format!("bad handshake reply: {e}")))
}

/// M5-E S-01 (R38): what to do with an open pipe before anything is written to it.
#[derive(Debug, PartialEq, Eq)]
enum ServerCheck {
    Proceed,
    /// The server process vanished or its token couldn't be read (e.g. an old daemon exiting
    /// after a drain): close and try again until the attach budget runs out.
    Retry(String),
    /// Another account's server: refuse now, nothing sent.
    Refuse(String),
}

fn classify_server(id: win::ServerIdentity) -> ServerCheck {
    match id {
        win::ServerIdentity::Ours => ServerCheck::Proceed,
        win::ServerIdentity::Unverifiable(why) => ServerCheck::Retry(why),
        win::ServerIdentity::ForeignOwner(sid) => {
            ServerCheck::Refuse(format!("pipe owned by another account ({sid})"))
        }
        win::ServerIdentity::ForeignProcess(sid) => {
            ServerCheck::Refuse(format!("pipe served by another account ({sid})"))
        }
    }
}

fn not_trusted(detail: String) -> Error {
    Error::new(ErrorCode::ConnectionNotTrusted).with_detail(detail)
}

/// Connect to the daemon, launching it if needed, and handshake.
///
/// S-01: before the handshake (the first byte we send), the pipe's owner and the server
/// process's user must be the current user ([`win::pipe_server_identity`]); otherwise the
/// attach fails with `ConnectionNotTrusted` and nothing crosses the pipe.
pub async fn attach(opts: &AttachOptions) -> Result<Connection, Error> {
    let t0 = Instant::now();
    let data_dir = naming::resolve_data_dir(opts.data_dir.as_deref())?;
    let sid = win::user_sid_string().map_err(|e| io_unavailable("user SID", e))?;
    let name = naming::pipe_name(&sid, &data_dir);
    let hs = opts.handshake();
    // Drain-restart rounds get their own budget on top of the normal attach.
    let overall = Instant::now() + opts.timeout * 3;
    let mut restarts = 0u32;
    let mut b = Backoff::new();
    loop {
        let (mut pipe, path) = open_or_launch(opts, &name, &data_dir).await?;
        match classify_server(win::pipe_server_identity(&pipe, &sid)) {
            ServerCheck::Proceed => {}
            ServerCheck::Refuse(why) => return Err(not_trusted(why)),
            ServerCheck::Retry(why) => {
                drop(pipe);
                if Instant::now() >= overall {
                    return Err(not_trusted(format!("pipe server not verifiable: {why}")));
                }
                b.wait().await;
                continue;
            }
        }
        match handshake(&mut pipe, &hs).await? {
            HandshakeReply::Accepted {
                proto,
                build,
                daemon_pid,
                ..
            } => {
                return Ok(Connection {
                    stream: pipe,
                    pipe_name: name,
                    data_dir,
                    daemon_pid,
                    daemon_proto: proto,
                    daemon_build: build,
                    path,
                    restarts,
                    elapsed: t0.elapsed(),
                });
            }
            HandshakeReply::Unavailable { reason, .. }
                if matches!(
                    reason,
                    UnavailableReason::Restarting | UnavailableReason::Draining
                ) =>
            {
                drop(pipe);
                restarts += 1;
                if Instant::now() >= overall {
                    return Err(reason.to_error(&opts.client_id));
                }
                b.wait().await;
            }
            HandshakeReply::Unavailable { reason, .. } => {
                return Err(reason.to_error(&opts.client_id));
            }
            HandshakeReply::Invalid { message } => {
                return Err(Error::new(ErrorCode::Internal)
                    .with_detail(format!("handshake rejected: {message}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_check_refuses_foreign_and_retries_unverifiable() {
        assert_eq!(
            classify_server(win::ServerIdentity::Ours),
            ServerCheck::Proceed
        );
        assert!(matches!(
            classify_server(win::ServerIdentity::ForeignOwner("S-1-5-21-9".into())),
            ServerCheck::Refuse(_)
        ));
        assert!(matches!(
            classify_server(win::ServerIdentity::ForeignProcess("S-1-5-21-9".into())),
            ServerCheck::Refuse(_)
        ));
        assert!(matches!(
            classify_server(win::ServerIdentity::Unverifiable("gone".into())),
            ServerCheck::Retry(_)
        ));
        let e = not_trusted("x".into());
        assert_eq!(e.code, ErrorCode::ConnectionNotTrusted);
        assert!(!e.retryable);
        assert!(e.user_message().body.contains("nothing was sent"));
    }
}
