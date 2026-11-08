//! One sandboxed worker process and the synchronous request/response exchange with it.
//!
//! The supervisor treats the worker as hostile: every frame is capped before allocation, the
//! response id, sizes and UTF-8 are validated, and any violation kills the worker. The
//! [`Watchdog`] wall-clock timer is the real timeout (M0-C: the Job CPU limit overshoots 2-3× and
//! is only a backstop); killing the job also unblocks any pipe read or write in progress.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sr_common::protocol::extract::{
    DocKind, EXTRACT_PROTO, MAX_JSON_FRAME, WorkerFailure, WorkerReady, WorkerRequest,
    WorkerResponse, WorkerResult, read_frame, write_frame, write_frame_len,
};

use crate::sandbox::{self, AppContainer, ChildProcess, Job, JobKiller, TokenKind};

/// How to start a worker.
#[derive(Debug, Clone)]
pub(crate) struct WorkerSpec {
    pub exe: PathBuf,
    pub pdfium: Option<PathBuf>,
    pub token: TokenKind,
    pub app_container: Option<Arc<AppContainer>>,
    pub memory_limit: u64,
    pub spawn_timeout: Duration,
}

/// Result of one request.
#[derive(Debug)]
pub(crate) enum Outcome {
    Ok {
        text: String,
        detected: DocKind,
        encoding: Option<String>,
        pages: Option<u32>,
        truncated: bool,
    },
    Failed {
        code: WorkerFailure,
        detail: String,
    },
    /// Wall-clock timeout (the job was killed). `memory_capped`: the job also hit its memory cap
    /// (worker wedged at the cap).
    Timeout {
        memory_capped: bool,
    },
    /// The Job CPU backstop fired.
    CpuLimit,
    /// The job hit its memory cap and the worker died.
    MemoryLimit,
    /// The worker died for another reason (access violation, stack overflow, abort ...).
    Crashed {
        exit_code: Option<u32>,
    },
    /// The worker broke the protocol (over-cap frame, bad JSON, wrong id, bad UTF-8 ...).
    Protocol(String),
    /// Reading the source file failed or it shrank while streaming.
    Source(io::Error),
}

impl Outcome {
    /// Whether the worker is dead or must not be reused.
    pub fn worker_unusable(&self) -> bool {
        !matches!(self, Outcome::Ok { .. } | Outcome::Failed { .. })
    }
}

enum Fault {
    Io(io::Error),
    Protocol(String),
    Source(io::Error),
}

// ------------------------------------------------------------------ watchdog

#[derive(Default)]
struct WdState {
    armed: Option<(Instant, JobKiller)>,
    fired: bool,
    stop: bool,
}

/// Per-slot wall-clock timer that kills the armed job at its deadline.
pub(crate) struct Watchdog {
    state: Arc<(Mutex<WdState>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Watchdog {
    pub fn new() -> io::Result<Watchdog> {
        let state: Arc<(Mutex<WdState>, Condvar)> = Arc::default();
        let st = Arc::clone(&state);
        let thread = std::thread::Builder::new()
            .name("sr-extract-watchdog".into())
            .spawn(move || {
                let (m, cv) = &*st;
                let mut g = lock(m);
                loop {
                    if g.stop {
                        return;
                    }
                    match g.armed.as_ref().map(|(d, _)| *d) {
                        None => g = cv.wait(g).unwrap_or_else(|p| p.into_inner()),
                        Some(deadline) => {
                            let now = Instant::now();
                            if now >= deadline {
                                if let Some((_, killer)) = g.armed.take() {
                                    killer.kill();
                                }
                                g.fired = true;
                            } else {
                                g = cv
                                    .wait_timeout(g, deadline - now)
                                    .map(|(g, _)| g)
                                    .unwrap_or_else(|p| p.into_inner().0);
                            }
                        }
                    }
                }
            })?;
        Ok(Watchdog {
            state,
            thread: Some(thread),
        })
    }

    fn arm(&self, timeout: Duration, killer: JobKiller) {
        let (m, cv) = &*self.state;
        let mut g = lock(m);
        g.armed = Some((Instant::now() + timeout, killer));
        g.fired = false;
        cv.notify_all();
    }

    /// Disarms; returns whether the timer fired (the job was killed).
    fn disarm(&self) -> bool {
        let (m, _) = &*self.state;
        let mut g = lock(m);
        g.armed = None;
        std::mem::take(&mut g.fired)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        {
            let (m, cv) = &*self.state;
            lock(m).stop = true;
            cv.notify_all();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ------------------------------------------------------------------ worker

/// A live worker. Dropping it kills its job (and so the process).
pub(crate) struct Worker {
    job: Job,
    child: ChildProcess,
    pub ready: WorkerReady,
    pub token: TokenKind,
    pub jobs_done: u32,
    pub spawn_time: Duration,
    stderr: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("pid", &self.child.pid)
            .field("token", &self.token)
            .field("jobs_done", &self.jobs_done)
            .finish()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.job.terminate();
        let _ = self.child.wait_exit(Duration::from_secs(5));
        if let Some(t) = self.stderr.take() {
            let _ = t.join();
        }
    }
}

/// S-04: why a started worker never sent its ready frame (classified by the pool).
#[derive(Debug)]
pub(crate) struct NotReady {
    /// The spawn watchdog fired: a slow start, never a reason to leave the AppContainer.
    pub timed_out: bool,
    /// The worker's exit code, if it exited.
    pub exit: Option<u32>,
    pub msg: String,
}

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for NotReady {}

impl Worker {
    pub fn spawn(spec: &WorkerSpec, wd: &Watchdog) -> io::Result<Worker> {
        let t0 = Instant::now();
        let job = Job::new(spec.memory_limit, spec.spawn_timeout.saturating_mul(2))?;
        let mut args: Vec<&std::ffi::OsStr> = Vec::new();
        if let Some(p) = &spec.pdfium {
            args.push("--pdfium".as_ref());
            args.push(p.as_os_str());
        }
        let mut child = sandbox::spawn(
            &spec.exe,
            &args,
            spec.token,
            spec.app_container.as_deref(),
            &job,
        )?;
        let stderr = drain_stderr(&mut child)?;
        wd.arm(spec.spawn_timeout, job.killer());
        let first = read_frame(&mut child.stdout, MAX_JSON_FRAME.into());
        let fired = wd.disarm();
        let mut worker_guard = PartialWorker {
            job: Some(job),
            child: Some(child),
            stderr: Some(stderr),
        };
        let frame = match first {
            Ok(Some(f)) if !fired => f,
            other => {
                let code = worker_guard.exit_code();
                return Err(io::Error::other(NotReady {
                    // The watchdog fired (or the frame came after it did): a slow start.
                    timed_out: fired,
                    exit: code,
                    msg: format!(
                        "worker did not become ready ({}; exit {})",
                        match other {
                            Ok(Some(_)) => "spawn timeout".to_string(),
                            Ok(None) if fired => "spawn timeout".to_string(),
                            Ok(None) => "closed its output".to_string(),
                            Err(e) => e.to_string(),
                        },
                        code.map_or("none".into(), |c| format!("0x{c:X}"))
                    ),
                }));
            }
        };
        let ready: WorkerReady = serde_json::from_slice(&frame)
            .map_err(|e| io::Error::other(format!("bad ready frame: {e}")))?;
        if ready.proto != EXTRACT_PROTO {
            return Err(io::Error::other(format!(
                "worker speaks extract protocol {} (expected {EXTRACT_PROTO})",
                ready.proto
            )));
        }
        let (job, child, stderr) = worker_guard.take();
        Ok(Worker {
            job,
            child,
            ready,
            token: spec.token,
            jobs_done: 0,
            spawn_time: t0.elapsed(),
            stderr,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.pid
    }

    pub fn id(&self) -> sandbox::WorkerId {
        sandbox::WorkerId::of(&self.child.process, self.child.pid)
    }

    pub fn killer(&self) -> JobKiller {
        self.job.killer()
    }

    pub fn peak_memory(&self) -> u64 {
        self.job.peak_process_memory()
    }

    /// Sends one job and waits for the result (at most `timeout`, enforced by `wd`).
    #[allow(clippy::too_many_arguments)]
    pub fn request(
        &mut self,
        wd: &Watchdog,
        id: u64,
        kind: DocKind,
        src: &mut dyn Read,
        len: u64,
        max_text: u64,
        timeout: Duration,
        cpu_backstop: Duration,
    ) -> Outcome {
        self.job.rearm_cpu(cpu_backstop);
        let _ = self.job.drain_messages();
        wd.arm(timeout, self.job.killer());
        let res = self.exchange(id, kind, src, len, max_text);
        let fired = wd.disarm();
        self.jobs_done += 1;
        if fired {
            let msgs = self.job.drain_messages();
            let _ = self.child.wait_exit(Duration::from_secs(5));
            return Outcome::Timeout {
                memory_capped: msgs.iter().any(|m| {
                    *m == sandbox::MSG_PROCESS_MEMORY_LIMIT || *m == sandbox::MSG_JOB_MEMORY_LIMIT
                }),
            };
        }
        match res {
            Ok(o) => o,
            Err(Fault::Protocol(s)) => {
                self.job.terminate();
                Outcome::Protocol(s)
            }
            Err(Fault::Source(e)) => {
                // The payload frame is incomplete: the stream is out of sync, so the worker goes.
                self.job.terminate();
                Outcome::Source(e)
            }
            Err(Fault::Io(e)) => {
                tracing::debug!(pid = self.child.pid, error = %e, "extract worker pipe failed");
                self.classify_death()
            }
        }
    }

    fn exchange(
        &mut self,
        id: u64,
        kind: DocKind,
        src: &mut dyn Read,
        len: u64,
        max_text: u64,
    ) -> Result<Outcome, Fault> {
        let hdr = serde_json::to_vec(&WorkerRequest {
            id,
            kind,
            max_text_bytes: max_text,
        })
        .map_err(|e| Fault::Protocol(e.to_string()))?;
        let stdin = &mut self.child.stdin;
        write_frame(stdin, &hdr).map_err(Fault::Io)?;
        write_frame_len(stdin, len).map_err(Fault::Io)?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut left = len;
        while left > 0 {
            let want = usize::try_from(left.min(buf.len() as u64)).unwrap_or(buf.len());
            let n = match src.read(&mut buf[..want]) {
                Ok(0) => {
                    return Err(Fault::Source(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "file shrank while it was being read",
                    )));
                }
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Fault::Source(e)),
            };
            stdin.write_all(&buf[..n]).map_err(Fault::Io)?;
            left -= n as u64;
        }
        stdin.flush().map_err(Fault::Io)?;
        drop(buf);

        let out = &mut self.child.stdout;
        let frame = match read_frame(out, MAX_JSON_FRAME.into()) {
            Ok(Some(f)) => f,
            Ok(None) => return Err(Fault::Io(io::ErrorKind::UnexpectedEof.into())),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                return Err(Fault::Protocol(e.to_string()));
            }
            Err(e) => return Err(Fault::Io(e)),
        };
        let resp: WorkerResponse = serde_json::from_slice(&frame)
            .map_err(|e| Fault::Protocol(format!("bad response header: {e}")))?;
        if resp.id != id {
            return Err(Fault::Protocol(format!(
                "response id {} for request {id}",
                resp.id
            )));
        }
        match resp.result {
            WorkerResult::Failed { code, detail } => Ok(Outcome::Failed {
                code,
                detail: sr_common::protocol::extract::clean_detail(&detail),
            }),
            WorkerResult::Ok {
                text_bytes,
                detected,
                encoding,
                pages,
                truncated,
                parse_ms: _,
            } => {
                if text_bytes > max_text {
                    return Err(Fault::Protocol(format!(
                        "text of {text_bytes} bytes over the {max_text}-byte cap"
                    )));
                }
                let text = match read_frame(out, text_bytes) {
                    Ok(Some(t)) if t.len() as u64 == text_bytes => t,
                    Ok(Some(t)) => {
                        return Err(Fault::Protocol(format!(
                            "text frame of {} bytes, header said {text_bytes}",
                            t.len()
                        )));
                    }
                    Ok(None) => return Err(Fault::Io(io::ErrorKind::UnexpectedEof.into())),
                    Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                        return Err(Fault::Protocol(e.to_string()));
                    }
                    Err(e) => return Err(Fault::Io(e)),
                };
                let text = String::from_utf8(text)
                    .map_err(|_| Fault::Protocol("text frame is not UTF-8".into()))?;
                let encoding = encoding.map(|e| e.chars().take(32).collect());
                Ok(Outcome::Ok {
                    text,
                    detected,
                    encoding,
                    pages,
                    truncated,
                })
            }
        }
    }

    fn classify_death(&mut self) -> Outcome {
        let mut code = self.child.wait_exit(Duration::from_secs(5));
        if code.is_none() {
            self.job.terminate();
            code = self.child.wait_exit(Duration::from_secs(5));
        }
        let msgs = self.job.drain_messages();
        if msgs
            .iter()
            .any(|m| *m == sandbox::MSG_END_OF_JOB_TIME || *m == sandbox::MSG_END_OF_PROCESS_TIME)
        {
            Outcome::CpuLimit
        } else if msgs
            .iter()
            .any(|m| *m == sandbox::MSG_PROCESS_MEMORY_LIMIT || *m == sandbox::MSG_JOB_MEMORY_LIMIT)
        {
            Outcome::MemoryLimit
        } else {
            Outcome::Crashed { exit_code: code }
        }
    }
}

/// Cleanup for a worker that failed before becoming ready.
struct PartialWorker {
    job: Option<Job>,
    child: Option<ChildProcess>,
    stderr: Option<JoinHandle<()>>,
}

impl PartialWorker {
    fn exit_code(&mut self) -> Option<u32> {
        if let Some(j) = &self.job {
            j.terminate();
        }
        self.child
            .as_ref()
            .and_then(|c| c.wait_exit(Duration::from_secs(5)))
    }

    fn take(&mut self) -> (Job, ChildProcess, Option<JoinHandle<()>>) {
        match (self.job.take(), self.child.take()) {
            (Some(j), Some(c)) => (j, c, self.stderr.take()),
            // Constructed with both set and taken once.
            _ => unreachable!("PartialWorker taken twice"),
        }
    }
}

impl Drop for PartialWorker {
    fn drop(&mut self) {
        if let Some(j) = self.job.take() {
            j.terminate();
        }
        if let Some(c) = self.child.take() {
            let _ = c.wait_exit(Duration::from_secs(5));
        }
        if let Some(t) = self.stderr.take() {
            let _ = t.join();
        }
    }
}

/// Logs the first 8 KiB of the worker's stderr (panic messages etc.) and drains the rest.
fn drain_stderr(child: &mut ChildProcess) -> io::Result<JoinHandle<()>> {
    let mut err = child.stderr.try_clone()?;
    let pid = child.pid;
    std::thread::Builder::new()
        .name("sr-extract-stderr".into())
        .spawn(move || {
            let mut buf = vec![0u8; 4096];
            let mut logged = 0usize;
            while let Ok(n) = err.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if logged < 8192 {
                    logged += n;
                    let s = String::from_utf8_lossy(&buf[..n]);
                    tracing::debug!(pid, "sr-extract stderr: {}", s.trim_end());
                }
            }
        })
}
