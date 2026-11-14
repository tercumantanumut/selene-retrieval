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
    // Unix: a process group of its own, so the job can stop the whole tree (see `job.rs`), and
    // a fixed `PATH` (a GUI app's own one has no package-manager folders).
    #[cfg(unix)]
    c.process_group(0).env("PATH", child_path(&exe.path));
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

/// Waits for the CLI. Unix: through the job, which ends the rest of the process group after the
/// CLI exited but before it is reaped (see `job.rs`).
async fn wait_child(
    child: &mut Child,
    job: Option<&Job>,
) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    if let Some(j) = job {
        return j.wait(child).await;
    }
    #[cfg(not(unix))]
    let _ = job;
    child.wait().await
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
        let status = wait_child(&mut child, job.as_ref()).await.ok();
        Output {
            code: status.and_then(|s| s.code()),
            stdout,
            stderr,
        }
    };
    let res = tokio::time::timeout(limit, work).await;
    // Dropping the job (and the child, kill_on_drop) ends anything still running; then the
    // temp folder can go. The job goes first: on Unix the leader must not be reaped yet.
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
    // Field order = drop order: the job (tree kill) before the folder is removed. On Unix the
    // tree is killed in `Drop` already, before the child (see there).
    job: Option<Job>,
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
            job,
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
        let waited = tokio::time::timeout(
            Duration::from_secs(10),
            wait_child(&mut self.child, self.job.as_ref()),
        )
        .await;
        // Unix: still running, so not reaped: the group id is still ours to kill.
        #[cfg(unix)]
        if waited.is_err()
            && let Some(j) = &self.job
        {
            j.kill_tree();
        }
        let code = waited.ok().and_then(Result::ok).and_then(|s| s.code());
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

/// Unix: the group is killed while the leader is surely not reaped. Dropping the child first
/// (kill_on_drop) hands it to tokio's reaper, after which the group id could be reused.
#[cfg(unix)]
impl Drop for Running {
    fn drop(&mut self) {
        if let Some(j) = &self.job {
            j.kill_tree();
        }
    }
}

/// Arguments that are fixed words chosen by us (letters, digits, `-`, `_`).
fn safe_word(a: &str) -> bool {
    !a.is_empty()
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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
#[cfg(windows)]
pub fn spawn_console(exe: &CliPath, args: &[&str], cwd: &Path, label: &str) -> Result<Child> {
    let path = exe.path.to_string_lossy();
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
    c.raw_arg(&line);
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

/// macOS: the sign-in runs in a Terminal window. Terminal is opened by Launch Services
/// (`/usr/bin/open -b com.apple.Terminal <script>`), so no Automation permission is asked.
///
/// The script is written to a new folder (random name, mode 0700, never an existing one) under
/// `cwd` (which must be this user's and not writable by others), mode 0700 and created
/// exclusively. Every path in it is a single-quoted shell word
/// ([`sh_quote`]): nothing in a path can end the quote, so no path can inject a command. `args`
/// are fixed words chosen by us. The script `cd`s to `cwd`, sets the same environment as
/// [`base_env`] and a `PATH` with the CLI's folder first, and runs the CLI.
///
/// Telling the app when the sign-in is over, like `start /wait`: the returned launcher is
/// `/bin/cat` reading a FIFO in that folder. The script holds the FIFO open (read-write, so it
/// never blocks) while it runs; when its shell ends (the CLI finished or the window was closed)
/// the kernel closes it, `cat` reads end-of-file and exits. The script removes its folder on
/// exit. The launcher is killed if dropped (only the waiter; the Terminal window is not ours).
#[cfg(target_os = "macos")]
pub fn spawn_console(exe: &CliPath, args: &[&str], cwd: &Path, label: &str) -> Result<Child> {
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let unsafe_path = |p: &Path| !p.is_absolute() || p.as_os_str().as_bytes().contains(&0);
    if unsafe_path(&exe.path) || unsafe_path(cwd) || !args.iter().all(|a| safe_word(a)) {
        return Err(Error::new(ErrorCode::InvalidInput)
            .with_provider(label.to_string())
            .with_detail("the program's path can't be opened safely in a window"));
    }
    // The script's folder goes inside `cwd`: only this user may change `cwd` (else another
    // account could swap the script before Terminal runs it).
    let private = std::fs::symlink_metadata(cwd).is_ok_and(|m| {
        use std::os::unix::fs::MetadataExt;
        m.is_dir() && m.uid() == crate::sys::uid() && m.mode() & 0o022 == 0
    });
    if !private {
        return Err(Error::new(ErrorCode::InvalidInput)
            .with_provider(label.to_string())
            .with_detail("the sign-in folder is not private to this user"));
    }
    let internal = |what: &str, e: std::io::Error| {
        Error::new(ErrorCode::Internal)
            .with_provider(label.to_string())
            .with_detail(format!("sign-in window: {what}"))
            .with_source(e)
    };
    let mut rnd = [0u8; 8];
    getrandom::fill(&mut rnd)
        .map_err(|e| Error::new(ErrorCode::Internal).with_detail(format!("random: {e}")))?;
    let hex: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    let dir = cwd.join(format!("signin-{hex}"));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| internal("folder", e))?;
    let cleanup = |e: Error| {
        let _ = std::fs::remove_dir_all(&dir);
        e
    };
    let fifo = dir.join("done");
    let script = dir.join("signin.command");
    mkfifo(&fifo).map_err(|e| cleanup(internal("fifo", e)))?;
    let text = console_script(&exe.path, args, cwd, &dir, &fifo);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&script)
        .and_then(|mut f| f.write_all(&text))
        .map_err(|e| cleanup(internal("script", e)))?;
    // The waiter first, so it is waiting on the FIFO before the window starts.
    let mut waiter = Command::new("/bin/cat");
    waiter
        .arg(&fifo)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = waiter
        .spawn()
        .map_err(|e| cleanup(spawn_error(&e, label)))?;
    let opened = std::process::Command::new("/usr/bin/open")
        .args(["-b", "com.apple.Terminal", "--"])
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !opened.is_ok_and(|s| s.success()) {
        let _ = child.start_kill();
        return Err(cleanup(
            Error::new(ErrorCode::ProviderUnavailable)
                .with_provider(label.to_string())
                .with_detail("Terminal could not be opened for the sign-in"),
        ));
    }
    Ok(child)
}

/// Other Unix systems: no sign-in window yet.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn spawn_console(_exe: &CliPath, _args: &[&str], _cwd: &Path, label: &str) -> Result<Child> {
    Err(Error::new(ErrorCode::ProviderUnavailable)
        .with_provider(label.to_string())
        .with_detail("no sign-in window on this system"))
}

/// `mkfifo(path, 0600)`.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // libc FFI: mkfifo. Reviewed (macOS port).
fn mkfifo(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `c` is a NUL-terminated path that outlives the call.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// One single-quoted POSIX shell word: `'` inside becomes `'\''`. Nothing else is special
/// inside single quotes, so any bytes stay literal.
#[cfg(unix)]
#[doc(hidden)]
pub fn sh_quote(s: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut out = vec![b'\''];
    for &b in s.as_bytes() {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    out
}

/// The Terminal script [`spawn_console`] writes (for tests).
#[cfg(unix)]
#[doc(hidden)]
pub fn console_script(exe: &Path, args: &[&str], cwd: &Path, dir: &Path, fifo: &Path) -> Vec<u8> {
    let mut s: Vec<u8> = Vec::new();
    let mut var = |name: &str, value: &std::ffi::OsStr| {
        s.extend_from_slice(name.as_bytes());
        s.push(b'=');
        s.extend_from_slice(&sh_quote(value));
        s.push(b'\n');
    };
    var("sr_exe", exe.as_os_str());
    var("sr_cwd", cwd.as_os_str());
    var("sr_dir", dir.as_os_str());
    var("sr_done", fifo.as_os_str());
    var("PATH", &child_path(exe));
    let words: Vec<&str> = args.iter().copied().filter(|a| safe_word(a)).collect();
    let body = format!(
        concat!(
            "export PATH NO_COLOR=1 DISABLE_AUTOUPDATER=1\n",
            "unset CLAUDECODE\n",
            "# Held open until this shell ends: tells Selene Retrieval the sign-in is over.\n",
            "exec 3<>\"$sr_done\"\n",
            "trap '/bin/rm -rf \"$sr_dir\"' EXIT\n",
            "trap 'exit 129' HUP\n",
            "trap 'exit 130' INT\n",
            "trap 'exit 143' TERM\n",
            "clear\n",
            "echo 'Selene Retrieval: sign-in'\n",
            "echo\n",
            "cd \"$sr_cwd\" || exit 1\n",
            "\"$sr_exe\" {} 3<&-\n",
            "echo\n",
            "echo 'You can close this window now.'\n",
        ),
        words.join(" ")
    );
    let mut out = b"#!/bin/sh\n".to_vec();
    out.extend_from_slice(&s);
    out.extend_from_slice(body.as_bytes());
    out
}

/// `PATH` for a CLI child on Unix: the CLI's own folder first (an npm shim starts `node` through
/// `/usr/bin/env`, and npm puts `node` next to it), then the system folders. Never the
/// inherited `PATH`.
#[cfg(unix)]
fn child_path(exe: &Path) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStrExt;
    let mut p = std::ffi::OsString::new();
    if let Some(dir) = exe.parent()
        && dir.is_absolute()
        && !dir.as_os_str().as_bytes().contains(&b':')
    {
        p.push(dir);
        p.push(":");
    }
    p.push("/usr/bin:/bin:/usr/sbin:/sbin");
    p
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    fn sh() -> CliPath {
        CliPath {
            path: PathBuf::from("/bin/sh"),
        }
    }

    /// Whether process `pid` is gone (reaped by launchd once its parent shell died).
    async fn gone(pid: &str) -> bool {
        for _ in 0..50 {
            let alive = std::process::Command::new("/bin/kill")
                .args(["-0", pid])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !alive {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[test]
    fn sign_in_script_quotes_every_path() {
        use std::ffi::OsStr;
        assert_eq!(sh_quote(OsStr::new("a b")), b"'a b'");
        assert_eq!(sh_quote(OsStr::new("it's")), b"'it'\\''s'");
        // A hostile folder name stays one literal word: no `$(...)`, `;` or quote escapes.
        let exe = Path::new("/Users/a/x'; touch /tmp/pwn; echo '$(id)`id`/bin/claude");
        let text = console_script(
            exe,
            &["auth", "login", "; rm -rf ~"],
            Path::new("/tmp/w d"),
            Path::new("/tmp/w d/signin-1"),
            Path::new("/tmp/w d/signin-1/done"),
        );
        let text = String::from_utf8(text).unwrap();
        assert!(text.starts_with("#!/bin/sh\n"));
        assert!(text.contains(
            "sr_exe='/Users/a/x'\\''; touch /tmp/pwn; echo '\\''$(id)`id`/bin/claude'\n"
        ));
        assert!(text.contains("PATH='/Users/a/x'\\''; touch /tmp/pwn; echo '\\''$(id)`id`/bin:/usr/bin:/bin:/usr/sbin:/sbin'\n"));
        assert!(text.contains("\"$sr_exe\" auth login 3<&-\n"), "{text}");
        assert!(!text.contains("rm -rf ~"), "only fixed words are passed");
        // The script itself parses and, run with a harmless "CLI", ends the waiter.
        let dir = tempfile::tempdir().unwrap();
        let done = dir.path().join("done");
        let made = std::process::Command::new("/usr/bin/mkfifo")
            .arg(&done)
            .status()
            .unwrap();
        assert!(made.success());
        let script = console_script(
            // Long enough for the waiter to open the FIFO first, as the window's start is.
            Path::new("/bin/sleep"),
            &["1"],
            dir.path(),
            &dir.path().join("gone"),
            &done,
        );
        let waiter = std::process::Command::new("/bin/cat")
            .arg(&done)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let st = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(String::from_utf8(script).unwrap())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env("TERM", "dumb")
            .status()
            .unwrap();
        assert!(st.success());
        assert!(waiter.wait_with_output().unwrap().status.success());
    }

    #[test]
    fn cli_children_get_a_fixed_path() {
        assert_eq!(
            child_path(Path::new("/opt/homebrew/bin/codex")),
            "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        );
        assert_eq!(
            child_path(Path::new("/a:b/claude")),
            "/usr/bin:/bin:/usr/sbin:/sbin"
        );
    }

    #[tokio::test]
    async fn helpers_are_killed_when_the_cli_exits_or_times_out() {
        // The CLI exits normally and leaves a helper behind: the exit code is kept and the
        // helper (same process group) is killed before the CLI is reaped.
        let script = "sleep 30 >/dev/null 2>&1 & echo $!".to_string();
        let out = run(
            &sh(),
            &["-c".into(), script],
            None,
            Duration::from_secs(10),
            "t",
        )
        .await
        .unwrap();
        assert_eq!(out.code, Some(0));
        assert!(gone(out.stdout.trim()).await, "helper still running");
        // A run that times out: the whole group is killed.
        let marker = std::env::temp_dir().join(format!("sr-job-test-{}", std::process::id()));
        let script = format!(
            "sleep 30 >/dev/null 2>&1 & echo $! > '{}'; sleep 30",
            marker.display()
        );
        let e = run(
            &sh(),
            &["-c".into(), script],
            None,
            Duration::from_millis(500),
            "t",
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Timeout);
        let pid = std::fs::read_to_string(&marker).unwrap();
        let _ = std::fs::remove_file(&marker);
        assert!(gone(pid.trim()).await, "helper still running after timeout");
    }
}
