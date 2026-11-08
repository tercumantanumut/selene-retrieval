//! sr-harness (M0-D spike): emulates MCP clients and runs tests 5 and 6.
//!
//!   sr-harness selfjob                 job state of this process (are we already in a job?)
//!   sr-harness e2e                     test 5a: initialize + tools/call through bridge->pipe->daemon
//!   sr-harness second                  test 5b: second daemon fails (host.lock, then FIRST_PIPE_INSTANCE)
//!   sr-harness dacl                    test 5c: effective DACL + restricted-token denial
//!   sr-harness race [N]                test 5d: N bridges, no daemon => one daemon, all connect
//!   sr-harness version                 test 5e: {proto, build, client_kind} accept/refuse
//!   sr-harness latency [N]             bridge cold start + attach latency
//!   sr-harness job <mode>              test 6: bridge inside a kill-on-close Job Object
//!   sr-harness inspect [exe names..]   read-only Job Object inspection of running clients
//!   sr-harness stop                    stop any spike daemon

#[path = "../../../bridge/src/common.rs"]
mod common;
#[path = "../winutil.rs"]
mod winutil;

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, STILL_ACTIVE,
};
use windows_sys::Win32::Security::{
    CreateRestrictedToken, DuplicateTokenEx, GetTokenInformation, SID_AND_ATTRIBUTES, SecurityImpersonation,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY, TOKEN_USER, TokenImpersonation,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, READ_CONTROL};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, GetCurrentProcess, GetCurrentThread, GetExitCodeProcess, OpenProcess, OpenProcessToken,
    OpenThread, PROCESS_DUP_HANDLE, PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread, SetThreadToken,
    THREAD_SUSPEND_RESUME,
};

type R<T> = Result<T, String>;

fn exe_dir() -> PathBuf {
    std::env::current_exe().unwrap().parent().unwrap().to_path_buf()
}
fn bridge_exe() -> PathBuf {
    exe_dir().join("sr-bridge.exe")
}
fn daemon_exe() -> PathBuf {
    exe_dir().join("sr-daemon.exe")
}

// ---------------------------------------------------------------- MCP client
struct Mcp {
    r: BufReader<Box<dyn Read + Send>>,
    w: Box<dyn Write + Send>,
    id: u64,
    notes: Vec<Value>,
}

impl Mcp {
    fn new(r: Box<dyn Read + Send>, w: Box<dyn Write + Send>) -> Self {
        Mcp { r: BufReader::new(r), w, id: 0, notes: vec![] }
    }
    fn send(&mut self, v: &Value) -> R<()> {
        let mut s = v.to_string();
        s.push('\n');
        self.w.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
        self.w.flush().map_err(|e| e.to_string())
    }
    fn request(&mut self, method: &str, params: Value) -> R<Value> {
        self.id += 1;
        let id = self.id;
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        loop {
            let mut line = String::new();
            let n = self.r.read_line(&mut line).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err(format!("EOF waiting for {method}"));
            }
            let v: Value = serde_json::from_str(line.trim()).map_err(|e| format!("bad json {e}: {line}"))?;
            if v.get("id").and_then(|x| x.as_u64()) == Some(id) && v.get("method").is_none() {
                if let Some(err) = v.get("error") {
                    return Err(format!("{method} error: {err}"));
                }
                return Ok(v["result"].clone());
            }
            self.notes.push(v);
        }
    }
    fn notify(&mut self, method: &str) -> R<()> {
        self.send(&json!({"jsonrpc":"2.0","method":method}))
    }
    fn init(&mut self, ver: &str) -> R<Value> {
        let r = self.request(
            "initialize",
            json!({"protocolVersion":ver,"capabilities":{},"clientInfo":{"name":"sr-harness","version":"0"}}),
        )?;
        self.notify("notifications/initialized")?;
        Ok(r)
    }
    fn call(&mut self, tool: &str, args: Value, progress: Option<&str>) -> R<Value> {
        let mut p = json!({"name":tool,"arguments":args});
        if let Some(t) = progress {
            p["_meta"] = json!({"progressToken": t});
        }
        self.request("tools/call", p)
    }
    fn ping_json(&mut self) -> R<Value> {
        let r = self.call("ping", json!({}), None)?;
        let t = r["content"][0]["text"].as_str().ok_or("no text")?;
        serde_json::from_str(t).map_err(|e| e.to_string())
    }
}

fn bridge_mcp(child: &mut Child) -> Mcp {
    let out = child.stdout.take().unwrap();
    let inp = child.stdin.take().unwrap();
    Mcp::new(Box::new(out), Box::new(inp))
}

fn spawn_bridge(order: Option<&str>, suspended: bool, client: &str) -> std::io::Result<Child> {
    use std::os::windows::process::CommandExt;
    let mut c = Command::new(bridge_exe());
    c.args(["--client", client]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    if let Some(o) = order {
        c.env("SR_LAUNCH_ORDER", o);
    }
    if suspended {
        c.creation_flags(CREATE_SUSPENDED);
    }
    c.spawn()
}

/// Opens the pipe directly (no bridge), with busy retry, and does the sr handshake.
fn direct(timeout: Duration) -> R<Mcp> {
    direct_hs(timeout, common::PROTO as u64, "mcp").and_then(|(m, reply)| {
        if reply.starts_with("{\"ok\":true") { Ok(m) } else { Err(format!("handshake refused: {reply}")) }
    })
}

/// Raw handshake with an arbitrary proto / client_kind; returns the reply line.
fn direct_hs(timeout: Duration, proto: u64, kind: &str) -> R<(Mcp, String)> {
    let name = common::pipe_name();
    let dl = Instant::now() + timeout;
    let f: File = loop {
        match OpenOptions::new().read(true).write(true).open(&name) {
            Ok(f) => break f,
            Err(e) if Instant::now() < dl && matches!(e.raw_os_error(), Some(2) | Some(231)) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => return Err(format!("open {name}: {e}")),
        }
    };
    let mut w = f.try_clone().map_err(|e| e.to_string())?;
    let hs = json!({"proto": proto, "build": common::BUILD, "client_kind": kind, "client_id": "harness-direct", "bridge_pid": std::process::id()});
    w.write_all(format!("{hs}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut rd = f;
    let mut line = vec![];
    let mut b = [0u8; 1];
    loop {
        if rd.read(&mut b).map_err(|e| e.to_string())? == 0 {
            return Err("eof in handshake".into());
        }
        if b[0] == b'\n' {
            break;
        }
        line.push(b[0]);
    }
    Ok((Mcp::new(Box::new(rd), Box::new(w)), String::from_utf8_lossy(&line).into_owned()))
}

fn pipe_exists() -> bool {
    // WaitNamedPipe-free probe: a zero-length open attempt. 2 = not found.
    match OpenOptions::new().read(true).open(common::pipe_name()) {
        Ok(_) => true, // consumes an instance; the daemon logs a handshake timeout, harmless
        Err(e) => e.raw_os_error() != Some(2),
    }
}

fn stop_daemons() {
    if let Ok(mut m) = direct(Duration::from_millis(500)) {
        if m.init("2025-11-25").is_ok() {
            let _ = m.call("shutdown", json!({}), None);
        }
    }
    let dl = Instant::now() + Duration::from_secs(5);
    while !winutil::count_named("sr-daemon.exe").is_empty() && Instant::now() < dl {
        std::thread::sleep(Duration::from_millis(100));
    }
    let left = winutil::count_named("sr-daemon.exe");
    if !left.is_empty() {
        println!("  WARN sr-daemon still running: {left:?}");
    }
}

fn pid_alive(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        code == STILL_ACTIVE as u32
    }
}

fn resume_all_threads(pid: u32) {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        let mut te: THREADENTRY32 = std::mem::zeroed();
        te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        if Thread32First(snap, &mut te) != 0 {
            loop {
                if te.th32OwnerProcessID == pid {
                    let th = OpenThread(THREAD_SUSPEND_RESUME, 0, te.th32ThreadID);
                    if !th.is_null() {
                        ResumeThread(th);
                        CloseHandle(th);
                    }
                }
                if Thread32Next(snap, &mut te) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------- tests
fn t_selfjob() {
    let me = unsafe { GetCurrentProcess() };
    println!("harness pid={} in_any_job={:?}", std::process::id(), winutil::in_any_job(me));
    println!(
        "  own job limit flags: {}",
        winutil::job_limit_flags(std::ptr::null_mut()).map(winutil::job_flag_names).unwrap_or_else(|| "n/a".into())
    );
    println!("  package: {:?}", winutil::package_full_name());
    println!("  pipe: {}", common::pipe_name());
    println!("  data dir: {}", common::data_dir().display());
}

fn t_e2e() -> R<()> {
    stop_daemons();
    for ver in ["2025-11-25", "2025-06-18", "2024-11-05"] {
        let t0 = Instant::now();
        let mut ch = spawn_bridge(Some("breakaway,task"), false, "e2e").map_err(|e| e.to_string())?;
        let mut m = bridge_mcp(&mut ch);
        let init = m.init(ver)?;
        let t_init = t0.elapsed();
        let tools = m.request("tools/list", json!({}))?;
        let names: Vec<String> = tools["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        let ping = m.ping_json()?;
        let pr = m.call("progress", json!({"steps": 5}), Some("tok-1"))?;
        let progress: Vec<String> = m
            .notes
            .iter()
            .filter(|n| n["method"] == "notifications/progress")
            .map(|n| format!("{}/{}", n["params"]["progress"], n["params"]["total"]))
            .collect();
        println!(
            "e2e client_ver={ver} negotiated={} server={} init_ms={} tools={names:?}",
            init["protocolVersion"], init["serverInfo"]["name"], t_init.as_millis()
        );
        println!("  ping: {ping}");
        println!("  progress result={} notifications={progress:?}", pr["content"][0]["text"]);
        drop(m);
        let _ = ch.wait();
    }
    // Sessionless 2026-07-28 revision: what does the server do with initialize?
    let mut ch = spawn_bridge(None, false, "e2e").map_err(|e| e.to_string())?;
    let mut m = bridge_mcp(&mut ch);
    match m.init("2026-07-28") {
        Ok(r) => println!("e2e client_ver=2026-07-28 via initialize => negotiated {}", r["protocolVersion"]),
        Err(e) => println!("e2e client_ver=2026-07-28 via initialize => {e}"),
    }
    drop(m);
    let _ = ch.wait();
    stop_daemons();
    Ok(())
}

fn t_second() -> R<()> {
    stop_daemons();
    let mut ch = spawn_bridge(Some("breakaway"), false, "first").map_err(|e| e.to_string())?;
    let mut m = bridge_mcp(&mut ch);
    m.init("2025-11-25")?;
    let first = m.ping_json()?;
    println!("first daemon pid={}", first["daemon_pid"]);
    for (label, extra) in [("normal (host.lock)", vec![]), ("--skip-host-lock (FIRST_PIPE_INSTANCE only)", vec!["--skip-host-lock"])] {
        let t = Instant::now();
        let st = Command::new(daemon_exe()).args(["--launched-by", "second"]).args(&extra).status().map_err(|e| e.to_string())?;
        println!("second daemon [{label}] exit={:?} after {} ms", st.code(), t.elapsed().as_millis());
    }
    let log = std::fs::read_to_string(common::data_dir().join("daemon.log")).unwrap_or_default();
    for l in log.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev() {
        if l.contains("host.lock") || l.contains("FIRST_PIPE_INSTANCE") || l.contains("second") {
            println!("  log: {l}");
        }
    }
    let again = m.ping_json()?;
    println!("first daemon still serving: pid={} sessions={} sr-daemon processes={:?}", again["daemon_pid"], again["sessions"], winutil::count_named("sr-daemon.exe"));
    drop(m);
    let _ = ch.wait();
    stop_daemons();
    Ok(())
}

unsafe fn open_pipe_raw(name: &str, access: u32) -> Result<HANDLE, u32> {
    // Retries ERROR_PIPE_BUSY (231) for up to 2 s so each probe gets a real access decision.
    let w = wide(name);
    let dl = Instant::now() + Duration::from_secs(2);
    loop {
        let h = unsafe { CreateFileW(w.as_ptr(), access, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
        if h != INVALID_HANDLE_VALUE {
            return Ok(h);
        }
        let e = unsafe { GetLastError() };
        if e != 231 || Instant::now() >= dl {
            return Err(e);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn t_dacl() -> R<()> {
    stop_daemons();
    let mut ch = spawn_bridge(Some("breakaway"), false, "dacl").map_err(|e| e.to_string())?;
    let mut m = bridge_mcp(&mut ch);
    m.init("2025-11-25")?;
    let sid = common::user_sid_string().map_err(|e| e.to_string())?;
    let r = m.call("pipe_sddl", json!({}), None)?;
    println!("user SID: {sid}");
    println!("daemon-side: {}", r["content"][0]["text"]);
    let name = common::pipe_name();
    unsafe {
        // Client-side view of the same object.
        let h = open_pipe_raw(&name, READ_CONTROL | GENERIC_READ).map_err(|e| format!("open: {e}"))?;
        let sddl = winutil::handle_sddl(h).map_err(|e| e.to_string())?;
        CloseHandle(h);
        let dacl = sddl.split("D:").nth(1).unwrap_or("");
        let aces: Vec<&str> = dacl.split('(').skip(1).map(|a| a.trim_end_matches(')')).collect();
        let protected = dacl.starts_with('P');
        let only_user = aces.len() == 1 && aces[0].starts_with("A;") && aces[0].ends_with(&sid);
        println!("client-side GetSecurityInfo: {sddl}");
        println!("  protected={protected} ace_count={} only_user_sid_allow={only_user}", aces.len());

        // Another principal: a restricted token in which the user SID is deny-only.
        let mut tok: HANDLE = std::ptr::null_mut();
        OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_IMPERSONATE, &mut tok);
        let mut buf = vec![0u8; 256];
        let mut len = 0;
        GetTokenInformation(tok, TokenUser, buf.as_mut_ptr().cast(), 256, &mut len);
        let user_sid = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
        let impersonate = |disable_user: bool| -> HANDLE {
            let dis = SID_AND_ATTRIBUTES { Sid: user_sid, Attributes: 0 };
            let mut rt: HANDLE = std::ptr::null_mut();
            let n = if disable_user { 1 } else { 0 };
            if CreateRestrictedToken(tok, 0, n, &dis, 0, std::ptr::null(), 0, std::ptr::null(), &mut rt) == 0 {
                panic!("CreateRestrictedToken {}", GetLastError());
            }
            let mut imp: HANDLE = std::ptr::null_mut();
            DuplicateTokenEx(rt, TOKEN_IMPERSONATE | TOKEN_QUERY, std::ptr::null(), SecurityImpersonation, TokenImpersonation, &mut imp);
            CloseHandle(rt);
            imp
        };
        // Control pipe readable by Everyone.
        let ctrl_name = format!(r"\\.\pipe\sr-m0d-control-{}", std::process::id());
        let sd = winutil::sd_from_sddl("D:P(A;;GA;;;WD)").map_err(|e| e.to_string())?;
        let sa = winutil::security_attributes(sd);
        let cw = wide(&ctrl_name);
        let ctrl = CreateNamedPipeW(cw.as_ptr(), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, 512, 512, 0, &*sa);
        if ctrl == INVALID_HANDLE_VALUE {
            return Err(format!("control pipe create failed {}", GetLastError()));
        }
        let ctrl2 = CreateNamedPipeW(cw.as_ptr(), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, 512, 512, 0, &*sa);
        for (label, disable) in [("restricted token, user SID deny-only (other principal)", true), ("restricted token, user SID intact (control)", false)] {
            let imp = impersonate(disable);
            if SetThreadToken(std::ptr::null(), imp) == 0 {
                println!("  SetThreadToken failed {}", GetLastError());
            }
            let _ = GetCurrentThread();
            let r_sr = open_pipe_raw(&name, GENERIC_READ | GENERIC_WRITE);
            let r_ctrl = open_pipe_raw(&ctrl_name, GENERIC_READ | GENERIC_WRITE);
            SetThreadToken(std::ptr::null(), std::ptr::null_mut());
            CloseHandle(imp);
            let fmt = |r: &Result<HANDLE, u32>| match r {
                Ok(_) => "OPENED".to_string(),
                Err(5) => "DENIED (ERROR_ACCESS_DENIED)".to_string(),
                Err(e) => format!("error {e}"),
            };
            println!("  [{label}] sr pipe: {} | Everyone control pipe: {}", fmt(&r_sr), fmt(&r_ctrl));
            if let Ok(h) = r_sr { CloseHandle(h); }
            if let Ok(h) = r_ctrl { CloseHandle(h); }
        }
        // Different integrity, same user: Low IL (S-1-16-4096) and Medium IL control. The pipe
        // has no explicit mandatory label => default Medium NO_WRITE_UP.
        for (label, il) in [("same user, Low IL (S-1-16-4096)", "S-1-16-4096"), ("same user, Medium IL (control)", "S-1-16-8192")] {
            use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
            use windows_sys::Win32::Security::{SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_MANDATORY_LABEL, TokenIntegrityLevel};
            let mut dup: HANDLE = std::ptr::null_mut();
            let mut t2: HANDLE = std::ptr::null_mut();
            OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_ADJUST_DEFAULT, &mut t2);
            DuplicateTokenEx(t2, TOKEN_IMPERSONATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT, std::ptr::null(), SecurityImpersonation, TokenImpersonation, &mut dup);
            CloseHandle(t2);
            let w = wide(il);
            let mut psid: *mut c_void = std::ptr::null_mut();
            ConvertStringSidToSidW(w.as_ptr(), &mut psid);
            let tml = TOKEN_MANDATORY_LABEL { Label: SID_AND_ATTRIBUTES { Sid: psid, Attributes: 0x20 /* SE_GROUP_INTEGRITY */ } };
            let set = SetTokenInformation(dup, TokenIntegrityLevel, (&tml as *const TOKEN_MANDATORY_LABEL).cast(), std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + 16);
            if set == 0 {
                println!("  [{label}] SetTokenInformation failed {}", GetLastError());
            }
            SetThreadToken(std::ptr::null(), dup);
            let r_rw = open_pipe_raw(&name, GENERIC_READ | GENERIC_WRITE);
            let r_ro = open_pipe_raw(&name, GENERIC_READ);
            SetThreadToken(std::ptr::null(), std::ptr::null_mut());
            CloseHandle(dup);
            windows_sys::Win32::Foundation::LocalFree(psid);
            let fmt = |r: &Result<HANDLE, u32>| match r {
                Ok(_) => "OPENED".to_string(),
                Err(5) => "DENIED (5 ACCESS_DENIED)".to_string(),
                Err(e) => format!("error {e}"),
            };
            println!("  [{label}] sr pipe read+write: {} | read-only: {}", fmt(&r_rw), fmt(&r_ro));
            if let Ok(h) = r_rw { CloseHandle(h); }
            if let Ok(h) = r_ro { CloseHandle(h); }
        }
        // PIPE_REJECT_REMOTE_CLIENTS: \\localhost\pipe\... goes through the SMB redirector,
        // so the server sees a remote client. The control pipe lacks the flag.
        let ctrl3 = CreateNamedPipeW(cw.as_ptr(), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, 512, 512, 0, &*sa);
        let remote = |n: &str| n.replacen(r"\\.\", r"\\localhost\", 1);
        let r_sr = open_pipe_raw(&remote(&name), GENERIC_READ | GENERIC_WRITE);
        let r_ctrl = open_pipe_raw(&remote(&ctrl_name), GENERIC_READ | GENERIC_WRITE);
        let fmt = |r: &Result<HANDLE, u32>| match r {
            Ok(_) => "OPENED".to_string(),
            Err(5) => "DENIED (5 ACCESS_DENIED)".to_string(),
            Err(e) => format!("error {e}"),
        };
        println!("  [SMB loopback \\\\localhost\\pipe] sr pipe (reject-remote): {} | control pipe (no flag): {}", fmt(&r_sr), fmt(&r_ctrl));
        if let Ok(h) = r_sr { CloseHandle(h); }
        if let Ok(h) = r_ctrl { CloseHandle(h); }
        CloseHandle(ctrl);
        if ctrl2 != INVALID_HANDLE_VALUE { CloseHandle(ctrl2); }
        if ctrl3 != INVALID_HANDLE_VALUE { CloseHandle(ctrl3); }
        CloseHandle(tok);
    }
    drop(m);
    let _ = ch.wait();
    stop_daemons();
    Ok(())
}

fn t_race(n: usize) -> R<()> {
    stop_daemons();
    println!("pipe present before race: {}  sr-daemon before: {:?}", pipe_exists(), winutil::count_named("sr-daemon.exe"));
    let race_t0 = common::now_ms();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(n));
    let order = std::env::var("SR_LAUNCH_ORDER").unwrap_or_else(|_| "breakaway,task".into());
    let hs: Vec<_> = (0..n)
        .map(|i| {
            let b = barrier.clone();
            let order = order.clone();
            std::thread::spawn(move || -> R<(u128, Value)> {
                b.wait();
                let t = Instant::now();
                let mut ch = spawn_bridge(Some(&order), false, &format!("race{i}")).map_err(|e| e.to_string())?;
                let mut m = bridge_mcp(&mut ch);
                m.init("2025-11-25")?;
                let tl = m.request("tools/list", json!({}))?;
                let ntools = tl["tools"].as_array().map(|a| a.len()).unwrap_or(0);
                if ntools == 0 {
                    return Err("tools/list returned no tools".into());
                }
                let ms = t.elapsed().as_millis();
                let mut p = m.ping_json()?;
                p["ntools"] = json!(ntools);
                // keep the session open briefly so all are concurrent
                std::thread::sleep(Duration::from_millis(500));
                drop(m);
                let _ = ch.wait();
                Ok((ms, p))
            })
        })
        .collect();
    let mut pids = std::collections::BTreeSet::new();
    let mut ok = 0;
    for (i, h) in hs.into_iter().enumerate() {
        match h.join().unwrap() {
            Ok((ms, p)) => {
                ok += 1;
                pids.insert(p["daemon_pid"].as_u64().unwrap_or(0));
                println!("  bridge {i}: initialize+tools/list({} tools) in {ms} ms -> daemon pid {} (launched_by {})", p["ntools"], p["daemon_pid"], p["launched_by"]);
            }
            Err(e) => println!("  bridge {i}: FAILED {e}"),
        }
    }
    let running = winutil::count_named("sr-daemon.exe");
    println!("race N={n}: connected={ok}/{n} distinct daemon pids={pids:?} sr-daemon processes now={running:?}");
    let after_t0 = |l: &&str| l.split(' ').next().and_then(|t| t.parse::<u128>().ok()).is_some_and(|t| t >= race_t0);
    let log = std::fs::read_to_string(common::data_dir().join("bridge.log")).unwrap_or_default();
    let blines: Vec<&str> = log.lines().filter(after_t0).collect();
    for l in blines.iter().filter(|l| l.contains("attached") || l.contains("launch")) {
        println!("  bridge.log: {l}");
    }
    // A successful launch logs "launch <method>: ..."; failures log "launch <m> failed".
    let spawners = blines.iter().filter(|l| l.contains(" launch ") && !l.contains(" failed")).count();
    let dlog = std::fs::read_to_string(common::data_dir().join("daemon.log")).unwrap_or_default();
    let since: Vec<&str> = dlog.lines().filter(after_t0).collect();
    let starts = since.iter().filter(|l| l.contains(" start ")).count();
    let served = since.iter().filter(|l| l.contains("pipe ready")).count();
    println!("  daemon processes started during race: {starts}; of which created the pipe: {served}");
    let pass = ok == n && pids.len() == 1 && served == 1 && spawners == 1;
    println!(
        "RACE {} (order={order}): {ok}/{n} complete, {} distinct daemon(s), {served} pipe owner(s), {spawners} spawning bridge(s), {starts} daemon process start(s)",
        if pass { "PASS" } else { "FAIL" },
        pids.len()
    );
    stop_daemons();
    if pass { Ok(()) } else { Err("race failed".into()) }
}

fn t_latency(n: usize) -> R<()> {
    let b = bridge_exe();
    println!("sr-bridge.exe size: {} bytes", std::fs::metadata(&b).map(|m| m.len()).unwrap_or(0));
    let mut v = vec![];
    for _ in 0..n {
        let t = Instant::now();
        Command::new(&b).arg("--version").stdout(Stdio::null()).status().map_err(|e| e.to_string())?;
        v.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("cold start (--version, process create->exit) n={n}: min {:.1} median {:.1} p95 {:.1} ms", v[0], v[n / 2], v[(n * 95) / 100]);
    // Attach latency with a warm daemon.
    stop_daemons();
    let mut keep = spawn_bridge(Some("breakaway"), false, "keep").map_err(|e| e.to_string())?;
    let mut km = bridge_mcp(&mut keep);
    km.init("2025-11-25")?;
    let mut a = vec![];
    for _ in 0..n {
        let t = Instant::now();
        let mut ch = spawn_bridge(None, false, "lat").map_err(|e| e.to_string())?;
        let mut m = bridge_mcp(&mut ch);
        m.init("2025-11-25")?;
        a.push(t.elapsed().as_secs_f64() * 1000.0);
        drop(m);
        let _ = ch.wait();
    }
    a.sort_by(|x, y| x.partial_cmp(y).unwrap());
    println!("spawn bridge -> initialize result (warm daemon) n={n}: min {:.1} median {:.1} p95 {:.1} ms", a[0], a[n / 2], a[(n * 95) / 100]);
    drop(km);
    let _ = keep.wait();
    // Cold daemon via each launch method.
    for order in ["breakaway", "task"] {
        stop_daemons();
        std::thread::sleep(Duration::from_millis(300));
        let t = Instant::now();
        let mut ch = spawn_bridge(Some(order), false, "cold").map_err(|e| e.to_string())?;
        let mut m = bridge_mcp(&mut ch);
        m.init("2025-11-25")?;
        let p = m.ping_json()?;
        println!("cold daemon via {order}: bridge spawn -> initialize result {} ms (launched_by={}, parent={})", t.elapsed().as_millis(), p["launched_by"], p["parent"]);
        drop(m);
        let _ = ch.wait();
    }
    stop_daemons();
    Ok(())
}

/// Test 5e: version handshake. N and N-1 accepted; older/newer/garbage refused with a plain
/// message; an old *bridge* turns the refusal into a JSON-RPC error on `initialize`.
fn t_version() -> R<()> {
    stop_daemons();
    let mut keep = spawn_bridge(Some("breakaway,task"), false, "ver-keep").map_err(|e| e.to_string())?;
    let mut km = bridge_mcp(&mut keep);
    km.init("2025-11-25")?;
    let n = common::PROTO as u64;
    let mut all = true;
    for (proto, kind, expect_ok) in [(n, "mcp", true), (n - 1, "mcp", true), (n - 2, "mcp", false), (0, "mcp", false), (n + 1, "mcp", false), (n, "bogus", false)] {
        let (mut m, reply) = direct_hs(Duration::from_secs(2), proto, kind)?;
        let ok = reply.starts_with("{\"ok\":true");
        let mut extra = String::new();
        if ok {
            extra = match m.init("2025-11-25") { Ok(r) => format!(" -> MCP initialize ok ({})", r["protocolVersion"]), Err(e) => format!(" -> init FAILED {e}") };
        }
        let good = ok == expect_ok;
        all &= good;
        println!("  [{}] direct proto={proto} kind={kind}: {reply}{extra}", if good { "ok" } else { "MISMATCH" });
    }
    // Old bridge (proto N-2) as an MCP client sees it.
    let mut c = Command::new(bridge_exe());
    c.args(["--client", "old-bridge"]).env("SR_PROTO_OVERRIDE", (n - 2).to_string()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut ch = c.spawn().map_err(|e| e.to_string())?;
    let mut m = bridge_mcp(&mut ch);
    let r = m.init("2025-11-25");
    let st = ch.wait().map_err(|e| e.to_string())?;
    let mut err = String::new();
    if let Some(mut e) = ch.stderr.take() { let _ = e.read_to_string(&mut err); }
    let plain = matches!(&r, Err(e) if e.contains("Restart your AI app"));
    all &= plain;
    println!("  [{}] old bridge (proto {}) initialize => {:?}; exit={:?}; stderr={}", if plain { "ok" } else { "MISMATCH" }, n - 2, r.err(), st.code(), err.trim());
    let still = km.ping_json()?;
    println!("  daemon unaffected: pid {} sessions {}", still["daemon_pid"], still["sessions"]);
    drop(km);
    let _ = keep.wait();
    println!("VERSION {}", if all { "PASS" } else { "FAIL" });
    stop_daemons();
    if all { Ok(()) } else { Err("version test failed".into()) }
}

fn make_job(flags: u32) -> HANDLE {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = flags;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        job
    }
}

fn t_job(mode: &str) -> R<()> {
    let (flags, order) = match mode {
        // Client allows breakaway; bridge tries breakaway first.
        "breakaway-ok" => (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK, "breakaway,task"),
        // Client forbids breakaway; bridge falls back to the scheduled task.
        "no-breakaway" => (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, "breakaway,task"),
        // Task first (the plan's primary order), breakaway forbidden.
        "task-first" => (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, "task,breakaway"),
        // Control: plain detached spawn inside the job => should die with the job.
        "injob" => (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK, "injob"),
        // libuv's global job (node/Electron child_process): includes SILENT_BREAKAWAY_OK.
        "libuv" => (
            JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            "injob",
        ),
        // Nested: inner (client) job allows breakaway, outer (e.g. terminal/IDE/launcher) job
        // does not. Inner job is the one we close.
        "nested" | "nested-task-first" => (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK, if mode == "nested" { "breakaway,task" } else { "task,breakaway" }),
        m => return Err(format!("unknown mode {m}")),
    };
    stop_daemons();
    std::thread::sleep(Duration::from_millis(300));
    let outer = mode.starts_with("nested").then(|| make_job(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE));
    let job = make_job(flags);
    let t = Instant::now();
    let mut ch = spawn_bridge(Some(order), true, &format!("job-{mode}")).map_err(|e| e.to_string())?;
    let bpid = ch.id();
    unsafe {
        let h = OpenProcess(0x1FFFFF, 0, bpid);
        if let Some(o) = outer {
            if AssignProcessToJobObject(o, h) == 0 {
                return Err(format!("AssignProcessToJobObject(outer) failed {}", GetLastError()));
            }
            println!("job[{mode}] outer flags={} (no BREAKAWAY_OK)", winutil::job_flag_names(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE));
        }
        if AssignProcessToJobObject(job, h) == 0 {
            return Err(format!("AssignProcessToJobObject failed {}", GetLastError()));
        }
        println!("job[{mode}] flags={} bridge pid={bpid} in_job={:?} order={order}", winutil::job_flag_names(flags), winutil::in_job(h, job));
        CloseHandle(h);
    }
    resume_all_threads(bpid);
    let mut m = bridge_mcp(&mut ch);
    let res = (|| -> R<Value> {
        m.init("2025-11-25")?;
        m.ping_json()
    })();
    let p = match res {
        Ok(p) => p,
        Err(e) => {
            println!("  bridge failed to attach: {e}");
            unsafe { CloseHandle(job) };
            return Ok(());
        }
    };
    let dpid = p["daemon_pid"].as_u64().unwrap_or(0) as u32;
    let d_in_this_job = unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, dpid);
        let r = winutil::in_job(h, job);
        if let Some(o) = outer {
            println!("  daemon in outer job: {:?}", winutil::in_job(h, o));
        }
        CloseHandle(h);
        r
    };
    println!(
        "  attached in {} ms; daemon pid={dpid} launched_by={} parent={} daemon_in_any_job={} daemon_in_client_job={d_in_this_job:?}",
        t.elapsed().as_millis(), p["launched_by"], p["parent"], p["in_job"]
    );
    // Client exits / job handle closes (outer too: the whole launcher tree goes away).
    unsafe {
        CloseHandle(job);
        if let Some(o) = outer {
            CloseHandle(o);
        }
    }
    let tw = Instant::now();
    let _ = ch.wait();
    println!("  job closed -> bridge exited after {} ms (exit {:?})", tw.elapsed().as_millis(), ch.try_wait().ok().flatten().map(|s| s.code()));
    std::thread::sleep(Duration::from_millis(800));
    let alive = pid_alive(dpid);
    let served = direct(Duration::from_secs(1)).and_then(|mut m| {
        m.init("2025-11-25")?;
        m.ping_json()
    });
    println!(
        "  RESULT daemon alive after job close: {alive}; new client served: {}",
        match &served { Ok(v) => format!("yes (pid {})", v["daemon_pid"]), Err(e) => format!("no ({e})") }
    );
    stop_daemons();
    Ok(())
}

// ---------------------------------------------------------------- inspect (read-only)
#[repr(C)]
#[derive(Clone, Copy)]
struct HandleEntryEx {
    object: *mut c_void,
    pid: usize,
    handle: usize,
    access: u32,
    backtrace: u16,
    type_index: u16,
    attrs: u32,
    reserved: u32,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(class: i32, buf: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

fn system_handles() -> Vec<HandleEntryEx> {
    let mut size: usize = 1 << 22;
    loop {
        let mut buf = vec![0u8; size];
        let mut ret = 0u32;
        let st = unsafe { NtQuerySystemInformation(64, buf.as_mut_ptr().cast(), size as u32, &mut ret) };
        if st == 0xC000_0004u32 as i32 {
            size = (ret as usize).max(size * 2) + 65536;
            continue;
        }
        if st != 0 {
            println!("NtQuerySystemInformation status 0x{st:x}");
            return vec![];
        }
        let n = unsafe { *(buf.as_ptr() as *const usize) };
        let base = unsafe { buf.as_ptr().add(16) as *const HandleEntryEx };
        return (0..n).map(|i| unsafe { *base.add(i) }).collect();
    }
}

fn t_inspect(names: &[String]) -> R<()> {
    let names: Vec<String> = if names.is_empty() {
        vec!["claude.exe".into(), "Cursor.exe".into(), "Code.exe".into()]
    } else {
        names.to_vec()
    };
    let procs = winutil::processes();
    // Job object type index: create a job and find our own handle to it.
    let probe = make_job(0);
    let handles = system_handles();
    let me = std::process::id() as usize;
    let job_type = handles.iter().find(|h| h.pid == me && h.handle == probe as usize).map(|h| h.type_index);
    unsafe { CloseHandle(probe) };
    println!("system handles={} job type index={job_type:?}", handles.len());
    let Some(job_type) = job_type else { return Err("no job type index".into()) };
    let roots: Vec<&winutil::Proc> = procs
        .iter()
        .filter(|p| names.iter().any(|n| p.name.eq_ignore_ascii_case(n)))
        .filter(|p| !procs.iter().any(|q| q.pid == p.ppid && q.name.eq_ignore_ascii_case(&p.name)))
        .collect();
    if roots.is_empty() {
        println!("no running process matches {names:?}");
    }
    for root in roots {
        // descendants
        let mut tree = vec![root.clone()];
        let mut i = 0;
        while i < tree.len() {
            let pid = tree[i].pid;
            for c in procs.iter().filter(|p| p.ppid == pid && p.pid != pid) {
                if !tree.iter().any(|t| t.pid == c.pid) {
                    tree.push(c.clone());
                }
            }
            i += 1;
        }
        println!("== {} pid {} ({} processes in tree)", root.name, root.pid, tree.len());
        // job handles owned by tree members
        let mut jobs: Vec<(u32, HANDLE)> = vec![];
        for h in handles.iter().filter(|h| h.type_index == job_type && tree.iter().any(|t| t.pid as usize == h.pid)) {
            unsafe {
                let src = OpenProcess(PROCESS_DUP_HANDLE, 0, h.pid as u32);
                if src.is_null() {
                    continue;
                }
                let mut dup: HANDLE = std::ptr::null_mut();
                // JOB_OBJECT_QUERY (0x4) only: read-only.
                if DuplicateHandle(src, h.handle as HANDLE, GetCurrentProcess(), &mut dup, 0x4, 0, 0) != 0 {
                    jobs.push((h.pid as u32, dup));
                }
                CloseHandle(src);
            }
        }
        let _ = DUPLICATE_SAME_ACCESS;
        for p in &tree {
            unsafe {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, p.pid);
                let any = if h.is_null() { None } else { winutil::in_any_job(h) };
                let mut member = vec![];
                for (owner, j) in &jobs {
                    if !h.is_null() && winutil::in_job(h, *j) == Some(true) {
                        member.push(format!("job(owner {owner}) {}", winutil::job_limit_flags(*j).map(winutil::job_flag_names).unwrap_or("?".into())));
                    }
                }
                if !h.is_null() {
                    CloseHandle(h);
                }
                println!("  pid {:>6} ppid {:>6} {:<28} in_job={:?} {}", p.pid, p.ppid, p.name, any, member.join("; "));
            }
        }
        for (_, j) in jobs {
            unsafe { CloseHandle(j) };
        }
    }
    Ok(())
}

fn main() {
    // Watchdog: never hang the test run.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("harness watchdog: timeout");
        std::process::exit(9);
    });
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("selfjob");
    let n = |d: usize| args.get(1).and_then(|s| s.parse().ok()).unwrap_or(d);
    let r = match cmd {
        "selfjob" => {
            t_selfjob();
            Ok(())
        }
        "e2e" => t_e2e(),
        "second" => t_second(),
        "dacl" => t_dacl(),
        "race" => t_race(n(5)),
        "version" => t_version(),
        "latency" => t_latency(n(20)),
        "job" => t_job(args.get(1).map(String::as_str).unwrap_or("breakaway-ok")),
        "inspect" => t_inspect(&args[1..]),
        "alive" => direct(Duration::from_secs(1)).and_then(|mut m| {
            m.init("2025-11-25")?;
            let p = m.ping_json()?;
            println!("daemon alive: {p}");
            Ok(())
        }),
        "stop" => {
            stop_daemons();
            Ok(())
        }
        c => Err(format!("unknown command {c}")),
    };
    if let Err(e) = r {
        eprintln!("FAILED: {e}");
        stop_daemons();
        std::process::exit(1);
    }
}
