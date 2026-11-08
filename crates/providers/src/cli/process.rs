//! Running the CLI (M5-C): absolute path, explicit arguments (never a shell string), an empty
//! temp working folder, no console window (or, for the sign-in only, the CLI's own window), the
//! prompt on stdin, a Job Object around the whole tree, and a hard time limit.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};

use super::discover::CliPath;
use super::job::Job;

/// Most text kept from one stream of a status command.
const STATUS_CAP: u64 = 256 * 1024;
/// Most stderr kept from a chat run (only for error classification; never logged whole).
const STDERR_CAP: u64 = 64 * 1024;
/// Longest single output line accepted from a chat run.
const LINE_CAP: usize = 16 * 1024 * 1024;

/// An empty folder under `%TEMP%` used as the working directory, so neither CLI finds a
/// `CLAUDE.md`, `AGENTS.md`, `.claude/` or `.codex/` of any project. Removed on drop.
#[derive(Debug)]
pub struct WorkDir(PathBuf);

impl WorkDir {
    pub fn new() -> Result<Self> {
        let mut rnd = [0u8; 8];
        getrandom::fill(&mut rnd)
            .map_err(|e| Error::new(ErrorCode::Internal).with_detail(format!("random: {e}")))?;
        let hex: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
        let p = std::env::temp_dir().join(format!("sr-cli-{}-{hex}", std::process::id()));
        std::fs::create_dir(&p).map_err(|e| {
            Error::new(ErrorCode::Internal)
                .with_detail("could not create a temp folder for the AI provider program")
                .with_source(e)
        })?;
        Ok(Self(p))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The common environment of every CLI process we start.
fn base_env(c: &mut Command) {
    c
        // Plain output, and no self-update of the user's CLI in the middle of our run.
        .env("NO_COLOR", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        // Set when started from inside a Claude Code session; never ours.
        .env_remove("CLAUDECODE");
    #[cfg(windows)]
    c.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
}

/// The command for `exe args` in `cwd`, without a window. Arguments are passed one by one (Rust
/// quotes them; for a `.cmd` shim it refuses any argument it can't quote safely for `cmd.exe`).
pub fn command(exe: &CliPath, args: &[String], cwd: &Path) -> Command {
    let mut c = Command::new(&exe.path);
    c.args(args).current_dir(cwd).kill_on_drop(true);
    base_env(&mut c);
    c
}

/// Spawn errors in plain codes: a missing file is "not installed".
pub(crate) fn spawn_error(e: &std::io::Error, label: &str) -> Error {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => ErrorCode::ProviderNotConfigured,
        std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        _ => ErrorCode::ProviderUnavailable,
    };
    Error::new(code)
        .with_provider(label.to_string())
        .with_detail(format!("could not start the program ({:?})", e.kind()))
}

/// What a finished short command printed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// Exit code (`None` = killed).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(r: Option<R>, cap: u64) -> String {
    let mut buf = Vec::new();
    if let Some(r) = r {
        let _ = r.take(cap).read_to_end(&mut buf).await;
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Runs `exe args` hidden, in a fresh empty temp folder, with `stdin` (or nothing), and waits at
/// most `limit`. On timeout the whole tree is killed and `Timeout` returned.
pub async fn run(
    exe: &CliPath,
    args: &[String],
    stdin: Option<String>,
    limit: Duration,
    label: &str,
) -> Result<Output> {
    let dir = WorkDir::new()?;
    let mut cmd = command(exe, args, dir.path());
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let job = Job::new();
    let mut child = cmd.spawn().map_err(|e| spawn_error(&e, label))?;
    if let Some(j) = &job {
        j.assign_child(&child);
    }
    if let (Some(text), Some(mut w)) = (stdin, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = w.write_all(text.as_bytes()).await;
            let _ = w.shutdown().await;
        });
    }
    let out = child.stdout.take();
    let err = child.stderr.take();
    let work = async {
        let (stdout, stderr) =
            tokio::join!(read_capped(out, STATUS_CAP), read_capped(err, STATUS_CAP));
        let status = child.wait().await.ok();
        Output {
            code: status.and_then(|s| s.code()),
            stdout,
            stderr,
        }
    };
    let res = tokio::time::timeout(limit, work).await;
    // Dropping the job (and the child, kill_on_drop) ends anything still running; then the
    // temp folder can go.
    drop(job);
    drop(child);
    drop(dir);
    res.map_err(|_| {
        Error::new(ErrorCode::Timeout)
            .with_provider(label.to_string())
            .with_detail(format!("no answer within {} s", limit.as_secs()))
    })
}

/// A chat run in progress: stdout read line by line. Dropping it kills the whole tree and
/// removes the temp folder.
pub struct Running {
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
    child: Child,
    stderr: Option<tokio::task::JoinHandle<String>>,
    // Field order = drop order: the job (tree kill) before the folder is removed.
    _job: Option<Job>,
    _dir: WorkDir,
}

impl std::fmt::Debug for Running {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Running").finish_non_exhaustive()
    }
}

impl Running {
    /// Starts `exe args` hidden with `stdin` written in the background.
    pub fn spawn(exe: &CliPath, args: &[String], stdin: String, label: &str) -> Result<Self> {
        let dir = WorkDir::new()?;
        let mut cmd = command(exe, args, dir.path());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let job = Job::new();
        let mut child = cmd.spawn().map_err(|e| spawn_error(&e, label))?;
        let assigned = job.as_ref().is_some_and(|j| j.assign_child(&child));
        if !assigned {
            tracing::warn!(target: "sr_providers::cli", "process tree not in a job; only the direct child is stopped on cancel");
        }
        if let Some(mut w) = child.stdin.take() {
            tokio::spawn(async move {
                let _ = w.write_all(stdin.as_bytes()).await;
                let _ = w.shutdown().await;
            });
        }
        let Some(out) = child.stdout.take() else {
            return Err(Error::new(ErrorCode::Internal).with_detail("no stdout pipe"));
        };
        let err = child.stderr.take();
        let stderr = Some(tokio::spawn(read_capped(err, STDERR_CAP)));
        Ok(Self {
            lines: BufReader::new(out).lines(),
            child,
            stderr,
            _job: job,
            _dir: dir,
        })
    }

    /// The next stdout line (`None` at the end).
    pub async fn next_line(&mut self) -> Result<Option<String>> {
        match self.lines.next_line().await {
            Ok(Some(l)) if l.len() > LINE_CAP => Err(Error::new(ErrorCode::ProviderRequest)
                .with_detail("the AI provider program printed an over-long line")),
            Ok(l) => Ok(l),
            Err(e) => Err(Error::new(ErrorCode::ProviderUnavailable)
                .with_detail(format!("reading the program's output: {:?}", e.kind()))),
        }
    }

    /// After stdout ended: the exit code (`None` if it didn't exit within 10 s) and stderr.
    pub async fn finish(&mut self) -> (Option<i32>, String) {
        let code = tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|s| s.code());
        let err = match self.stderr.take() {
            Some(h) => tokio::time::timeout(Duration::from_secs(2), h)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default(),
            None => String::new(),
        };
        (code, err)
    }
}

/// Starts the CLI's interactive sign-in in a console window of its own and returns the
/// launcher. Not in a kill-on-close job: it must outlive a cancelled request.
///
/// Rust always hands a child its standard handles, which would tie a new console's I/O to the
/// daemon's invisible one, so the window is opened the way Explorer does it: `cmd.exe` (by
/// absolute System32 path, `/d /v:off`, hidden itself) runs `start "" /wait "<exe>" <args>`.
/// The path is quoted (Windows file names can't contain `"`); a path with `%` (which `cmd.exe`
/// would expand inside quotes) is refused. `args` are fixed words chosen by us. The launcher
/// exits when the sign-in window closes.
pub fn spawn_console(exe: &CliPath, args: &[&str], cwd: &Path, label: &str) -> Result<Child> {
    let path = exe.path.to_string_lossy();
    let safe_word = |a: &str| {
        !a.is_empty()
            && a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if path.contains(['%', '"']) || !exe.path.is_absolute() || !args.iter().all(|a| safe_word(a)) {
        return Err(Error::new(ErrorCode::InvalidInput)
            .with_provider(label.to_string())
            .with_detail("the program's path can't be opened safely in a window"));
    }
    let Some(cmd_exe) = super::job::system_dir().map(|d| d.join("cmd.exe")) else {
        return Err(Error::new(ErrorCode::Internal).with_detail("System32 not found"));
    };
    let line = console_line(&exe.path, args);
    let mut c = Command::new(cmd_exe);
    #[cfg(windows)]
    c.raw_arg(&line);
    #[cfg(not(windows))]
    c.arg(&line);
    c.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    base_env(&mut c);
    c.spawn().map_err(|e| spawn_error(&e, label))
}

/// The `cmd.exe` argument line [`spawn_console`] builds (for tests).
#[doc(hidden)]
pub fn console_line(exe: &Path, args: &[&str]) -> String {
    let mut line = format!(
        "/d /v:off /c start \"\" /wait \"{}\"",
        exe.to_string_lossy()
    );
    for a in args {
        line.push(' ');
        line.push_str(a);
    }
    line
}
