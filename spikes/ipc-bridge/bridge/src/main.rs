//! sr-bridge (M0-D spike): stdio <-> named-pipe byte proxy.
//!
//! Attach sequence (architecture 2.2 / review D1):
//!   1. connect \\.\pipe\sr-<sid>-<datadir>  (ERROR_PIPE_BUSY => backoff + retry)
//!   2. absent => LockFileEx(spawn.lock) (blocking) => re-connect
//!   3. still absent => launch daemon using SR_LAUNCH_ORDER (default "breakaway,task,injob")
//!   4. poll the pipe with backoff for <= 10 s, release spawn.lock
//!   5. one handshake line, then raw MCP bytes both ways.
//!
//! Deps: tokio + windows-sys only.

#[path = "common.rs"]
mod common;

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    CreateProcessW, DETACHED_PROCESS, GetExitCodeProcess, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
    WaitForSingleObject,
};

const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PIPE_BUSY: i32 = 231;

fn blog(msg: &str) {
    common::log_line("bridge.log", msg);
}

fn daemon_exe() -> PathBuf {
    if let Some(p) = std::env::var_os("SR_DAEMON_EXE") {
        return PathBuf::from(p);
    }
    let me = std::env::current_exe().expect("current_exe");
    me.with_file_name("sr-daemon.exe")
}

/// CreateProcessW with bInheritHandles = FALSE. (std::process::Command always passes
/// TRUE, which would leak the MCP client's stdio pipe handles into the daemon and keep
/// the client's pipes open after the bridge dies.)
fn create_process(app: &str, cmdline: &str, flags: u32) -> Result<(u32, isize), u32> {
    let app_w = common::wide(app);
    let mut cmd_w = common::wide(cmdline);
    unsafe {
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            app_w.as_ptr(),
            cmd_w.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0, // bInheritHandles = FALSE
            flags | CREATE_UNICODE_ENVIRONMENT,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        );
        if ok == 0 {
            return Err(GetLastError());
        }
        CloseHandle(pi.hThread);
        Ok((pi.dwProcessId, pi.hProcess as isize))
    }
}

fn run_wait(app: &str, cmdline: &str) -> Result<u32, u32> {
    let (_pid, h) = create_process(app, cmdline, CREATE_NO_WINDOW)?;
    unsafe {
        let h = h as windows_sys::Win32::Foundation::HANDLE;
        if WaitForSingleObject(h, INFINITE) != WAIT_OBJECT_0 {
            CloseHandle(h);
            return Err(GetLastError());
        }
        let mut code = 0u32;
        GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        Ok(code)
    }
}

fn system32(exe: &str) -> String {
    let win = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{win}\System32\{exe}")
}

fn task_xml(daemon: &str, sid: &str) -> String {
    // No trigger: on-demand only (`schtasks /run`). Settings override the schtasks.exe
    // defaults that would break a daemon: 72 h ExecutionTimeLimit, no start on battery,
    // stop when going on battery, priority 7 (below normal).
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Selene Retrieval M0-D spike. Safe to delete.</Description></RegistrationInfo>
  <Principals><Principal id="Author"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>"{daemon}"</Command><Arguments>--launched-by task</Arguments></Exec></Actions>
</Task>
"#
    )
}

fn ensure_task(daemon: &str) -> Result<(), String> {
    let sid = common::user_sid_string().map_err(|e| e.to_string())?;
    let xml = task_xml(daemon, &sid);
    let path = common::data_dir().join("task.xml");
    let mut bytes = vec![0xFF, 0xFE];
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let sch = system32("schtasks.exe");
    let cmd = format!(r#""{sch}" /create /tn "{}" /xml "{}" /f"#, common::TASK_NAME, path.display());
    match run_wait(&sch, &cmd) {
        Ok(0) => Ok(()),
        Ok(c) => Err(format!("schtasks /create exit {c}")),
        Err(e) => Err(format!("CreateProcess schtasks err {e}")),
    }
}

fn launch(method: &str) -> Result<String, String> {
    let d = daemon_exe();
    let ds = d.to_string_lossy().to_string();
    match method {
        "breakaway" => {
            let cmd = format!(r#""{ds}" --launched-by breakaway"#);
            create_process(&ds, &cmd, DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
                .map(|(pid, h)| {
                    unsafe { CloseHandle(h as _) };
                    format!("breakaway pid={pid}")
                })
                .map_err(|e| format!("breakaway CreateProcess failed: win32 error {e}"))
        }
        "trampoline" => {
            let cmd = format!(r#""{ds}" --trampoline"#);
            create_process(&ds, &cmd, DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
                .map(|(pid, h)| {
                    unsafe { CloseHandle(h as _) };
                    format!("trampoline pid={pid}")
                })
                .map_err(|e| format!("trampoline CreateProcess failed: win32 error {e}"))
        }
        "injob" => {
            let cmd = format!(r#""{ds}" --launched-by injob"#);
            create_process(&ds, &cmd, DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
                .map(|(pid, h)| {
                    unsafe { CloseHandle(h as _) };
                    format!("injob pid={pid}")
                })
                .map_err(|e| format!("injob CreateProcess failed: win32 error {e}"))
        }
        "task" => {
            let sch = system32("schtasks.exe");
            let run = format!(r#""{sch}" /run /tn "{}""#, common::TASK_NAME);
            match run_wait(&sch, &run) {
                Ok(0) => Ok("task /run ok".into()),
                Ok(code) => {
                    // Task missing (or broken): self-register, then run again (U7).
                    blog(&format!("schtasks /run exit {code}; recreating task"));
                    ensure_task(&ds)?;
                    match run_wait(&sch, &run) {
                        Ok(0) => Ok("task created + /run ok".into()),
                        Ok(c) => Err(format!("schtasks /run exit {c} after create")),
                        Err(e) => Err(format!("schtasks spawn err {e}")),
                    }
                }
                Err(e) => Err(format!("schtasks spawn err {e}")),
            }
        }
        other => Err(format!("unknown launch method {other}")),
    }
}

fn try_open(name: &str) -> io::Result<NamedPipeClient> {
    ClientOptions::new().open(name)
}

/// Polls for the pipe with exponential backoff (5 ms .. 200 ms) until `deadline`.
async fn wait_pipe(name: &str, deadline: Instant) -> io::Result<NamedPipeClient> {
    let mut backoff = Duration::from_millis(5);
    loop {
        match try_open(name) {
            Ok(c) => return Ok(c),
            Err(e) if matches!(e.raw_os_error(), Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PIPE_BUSY)) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_millis(200));
            }
            Err(e) => return Err(e),
        }
    }
}

async fn attach(name: &str) -> io::Result<(NamedPipeClient, String)> {
    let t0 = Instant::now();
    // 1. Fast path. Busy => the daemon exists, just wait for a free instance.
    match try_open(name) {
        Ok(c) => return Ok((c, "direct".into())),
        Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
            let c = wait_pipe(name, Instant::now() + Duration::from_secs(10)).await?;
            return Ok((c, "direct-after-busy".into()));
        }
        Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => {}
        Err(e) => return Err(e),
    }
    // 2. Serialise spawners. Blocking wait is fine: nothing else runs yet.
    let lock = common::lock_file("spawn.lock", true)?;
    let waited = t0.elapsed();
    match try_open(name) {
        Ok(c) => {
            drop(lock);
            return Ok((c, format!("after-spawn-lock wait={}ms", waited.as_millis())));
        }
        // BUSY = the pipe name exists, so a daemon is serving and every instance is
        // momentarily taken. Never spawn on BUSY; wait for a free instance.
        Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
            drop(lock);
            let c = wait_pipe(name, Instant::now() + Duration::from_secs(10)).await?;
            return Ok((c, format!("after-spawn-lock(busy) wait={}ms", waited.as_millis())));
        }
        Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => {}
        Err(e) => {
            drop(lock);
            return Err(e);
        }
    }
    // 3. We are the spawner.
    let order = std::env::var("SR_LAUNCH_ORDER").unwrap_or_else(|_| "breakaway,task,injob".into());
    let mut how = String::new();
    for m in order.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let tl = Instant::now();
        match launch(m) {
            Ok(s) => {
                blog(&format!("launch {m}: {s} ({} ms)", tl.elapsed().as_millis()));
                // 4. Wait for the daemon's pipe.
                match wait_pipe(name, Instant::now() + Duration::from_secs(10)).await {
                    Ok(c) => {
                        how = format!("spawned via {m} ({s}); pipe ready after {} ms", tl.elapsed().as_millis());
                        drop(lock);
                        return Ok((c, how));
                    }
                    Err(e) => blog(&format!("launch {m}: pipe never appeared: {e}")),
                }
            }
            Err(e) => {
                blog(&format!("launch {m} failed: {e}"));
                how.push_str(&format!("{m}:{e}; "));
            }
        }
    }
    drop(lock);
    Err(io::Error::other(format!("ServiceUnavailable: all launch methods failed: {how}")))
}

enum Hs {
    Ok(String),
    /// Daemon refused; carries its plain-language `message`.
    Refused(String, String),
}

/// Pulls a top-level string field out of a one-line JSON object (no serde in the bridge).
fn json_str_field(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let i = line.find(&pat)? + pat.len();
    let mut out = String::new();
    let mut esc = false;
    for c in line[i..].chars() {
        match (esc, c) {
            (true, c) => { out.push(c); esc = false; }
            (false, '\\') => esc = true,
            (false, '"') => return Some(out),
            (false, c) => out.push(c),
        }
    }
    None
}

/// Raw JSON value of a top-level "id" (number or string), for echoing in an error.
fn json_id(line: &str) -> Option<String> {
    let i = line.find("\"id\":")? + 5;
    let rest = line[i..].trim_start();
    if let Some(r) = rest.strip_prefix('"') {
        let end = r.find('"')?;
        return Some(format!("\"{}\"", &r[..end]));
    }
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn handshake(pipe: &mut NamedPipeClient, client: &str) -> io::Result<Hs> {
    // SR_PROTO_OVERRIDE: test hook to impersonate an older/newer bridge.
    let proto: u16 = std::env::var("SR_PROTO_OVERRIDE").ok().and_then(|s| s.parse().ok()).unwrap_or(common::PROTO);
    let hs = format!(
        "{{\"proto\":{proto},\"build\":\"{}\",\"client_kind\":\"mcp\",\"client_id\":\"{}\",\"bridge_pid\":{}}}\n",
        common::BUILD,
        escape_json(client),
        std::process::id()
    );
    pipe.write_all(hs.as_bytes()).await?;
    let mut line = Vec::with_capacity(128);
    let mut b = [0u8; 1];
    loop {
        let n = pipe.read(&mut b).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed during handshake"));
        }
        if b[0] == b'\n' {
            break;
        }
        line.push(b[0]);
        if line.len() > 4096 {
            return Err(io::Error::other("handshake reply too long"));
        }
    }
    let s = String::from_utf8_lossy(&line).to_string();
    if s.starts_with("{\"ok\":true") {
        return Ok(Hs::Ok(s));
    }
    let msg = json_str_field(&s, "message").unwrap_or_else(|| "Selene Retrieval refused the connection.".into());
    Ok(Hs::Refused(msg, s))
}

/// Surfaces a plain error to the MCP client: answer its first request (normally
/// `initialize`) with a JSON-RPC error carrying `msg`, then exit. A bare exit would
/// show up in clients as "server disconnected" with no reason.
async fn reply_plain_error(msg: &str) {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    let read = async {
        while let Ok(1) = stdin.read(&mut b).await {
            if b[0] == b'\n' {
                break;
            }
            line.push(b[0]);
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(10), read).await;
    let l = String::from_utf8_lossy(&line);
    let id = json_id(&l).unwrap_or_else(|| "null".into());
    let e = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32002,\"message\":\"{}\"}}}}\n",
        escape_json(msg)
    );
    let _ = stdout.write_all(e.as_bytes()).await;
    let _ = stdout.flush().await;
}

async fn run(client: String, probe: bool) -> io::Result<()> {
    let t0 = Instant::now();
    let name = common::pipe_name();
    let (mut pipe, how) = attach(&name).await?;
    let hs = match handshake(&mut pipe, &client).await? {
        Hs::Ok(s) => s,
        Hs::Refused(msg, raw) => {
            blog(&format!("handshake refused: {raw}"));
            eprintln!("sr-bridge: {msg}");
            if !probe {
                reply_plain_error(&msg).await;
            }
            return Err(io::Error::other(format!("handshake refused: {msg}")));
        }
    };
    let ms = t0.elapsed().as_micros() as f64 / 1000.0;
    blog(&format!("attached client={client} how=[{how}] total={ms:.1}ms reply={hs}"));
    if probe {
        eprintln!("{{\"attach_ms\":{ms:.2},\"how\":\"{how}\",\"reply\":{hs}}}");
        return Ok(());
    }
    let (mut pr, mut pw) = tokio::io::split(pipe);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        r = tokio::io::copy(&mut stdin, &mut pw) => { blog(&format!("stdin closed: {r:?}")); }
        r = tokio::io::copy(&mut pr, &mut stdout) => { let _ = stdout.flush().await; blog(&format!("pipe closed: {r:?}")); }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("sr-bridge 0.0.1 (m0-d spike)");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--pipe-name") {
        println!("{}", common::pipe_name());
        return ExitCode::SUCCESS;
    }
    let client = args
        .iter()
        .position(|a| a == "--client")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "other".into());
    let probe = args.iter().any(|a| a == "--probe");
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let res = rt.block_on(run(client, probe));
    let code = match res {
        Ok(()) => 0,
        Err(e) => {
            blog(&format!("error: {e}"));
            eprintln!("sr-bridge: {e}");
            1
        }
    };
    // Don't wait for tokio's blocking stdin reader thread.
    std::process::exit(code)
}
