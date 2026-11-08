//! M0-C spike: supervisor for `sr-extract`. Launches the worker in a Job Object
//! (memory + CPU-time limits, KILL_ON_JOB_CLOSE, 1 active process, UI restrictions),
//! with a low-integrity restricted token or an AppContainer token, an explicit handle
//! allowlist, a child-process-creation ban and process mitigations. Talks to it over
//! length-prefixed anonymous-pipe frames. Kills and restarts on timeout/crash and runs
//! the hostile-fixture suite, printing a markdown table.
//!
//! Usage: harness <sr-extract.exe> <pdfium.dll> <fixtures dir> [--mode none|lowil|appcontainer]
//!                [--mem-mb 512] [--cpu-ms 10000] [--wall-ms 15000] [--no-mitig] [--win32k-off]
//!        harness --orphan ...same args...   (internal: KILL_ON_JOB_CLOSE test)

#[path = "../proto.rs"]
mod proto;

use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use proto::*;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::Isolation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::System::IO::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::System::SystemServices::{SE_GROUP_INTEGRITY, JOB_OBJECT_MSG_END_OF_JOB_TIME, JOB_OBJECT_MSG_END_OF_PROCESS_TIME, JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT, JOB_OBJECT_MSG_JOB_MEMORY_LIMIT};

static LIVE_KILL_US: std::sync::Mutex<Vec<u128>> = std::sync::Mutex::new(Vec::new());
const AC_NAME: &str = "selene.sr-extract.m0spike";
const EXIT_TIMEOUT_KILL: u32 = 0xE7E7;

// PROCESS_CREATION_MITIGATION_POLICY_* (winnt.h); defined here to avoid feature hunting.
const MIT_HEAP_TERMINATE: u64 = 1 << 12;
const MIT_BOTTOM_UP_ASLR: u64 = 1 << 16;
const MIT_STRICT_HANDLE_CHECKS: u64 = 1 << 24;
const MIT_WIN32K_DISABLE: u64 = 1 << 28;
const MIT_EXTENSION_POINT_DISABLE: u64 = 1 << 32;
const MIT_PROHIBIT_DYNAMIC_CODE: u64 = 1 << 36;
const MIT_IMAGE_NO_REMOTE: u64 = 1 << 52;
const MIT_IMAGE_NO_LOW_LABEL: u64 = 1 << 56;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    None,
    LowIl,
    AppContainer,
}

#[derive(Clone)]
struct Cfg {
    worker: PathBuf,
    pdfium: PathBuf,
    fixtures: PathBuf,
    mode: Mode,
    mem: usize,
    cpu_ms: u64,
    wall_ms: u64,
    mitig: bool,
    win32k_off: bool,
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn last_err() -> String {
    std::io::Error::last_os_error().to_string()
}

struct Worker {
    job: HANDLE,
    port: HANDLE,
    process: HANDLE,
    pid: u32,
    stdin: File,
    rx: Receiver<std::io::Result<Option<Vec<u8>>>>,
    ready: Ready,
    spawn_ms: u128,
    limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
}

impl Drop for Worker {
    fn drop(&mut self) {
        unsafe {
            TerminateJobObject(self.job, EXIT_TIMEOUT_KILL);
            CloseHandle(self.process);
            CloseHandle(self.job); // KILL_ON_JOB_CLOSE as a backstop
            CloseHandle(self.port);
        }
    }
}

#[derive(Debug)]
enum Outcome {
    Ok { chars: u64, pages: Option<u32>, truncated: bool, sample: String },
    Error { code: String, msg: String },
    ExtractTimeout,
    CpuLimit { exit: u32 },
    MemoryLimit { exit: u32 },
    WorkerCrashed { exit: u32, msgs: String },
    ProtocolError(String),
}

impl Outcome {
    fn label(&self) -> String {
        match self {
            Outcome::Ok { .. } => "Ok".into(),
            Outcome::Error { code, .. } => format!("Err:{code}"),
            Outcome::ExtractTimeout => "ExtractTimeout".into(),
            Outcome::CpuLimit { .. } => "CpuLimit (job killed)".into(),
            Outcome::MemoryLimit { .. } => "MemoryLimit".into(),
            Outcome::WorkerCrashed { .. } => "WorkerCrashed".into(),
            Outcome::ProtocolError(_) => "ProtocolError".into(),
        }
    }
    fn detail(&self) -> String {
        let d = match self {
            Outcome::Ok { chars, pages, truncated, sample } => {
                format!("{chars} chars{}{} \"{sample}\"", pages.map(|p| format!(", {p} pages")).unwrap_or_default(), if *truncated { ", truncated" } else { "" })
            }
            Outcome::Error { msg, .. } => msg.clone(),
            Outcome::ExtractTimeout => "wall-clock timeout; job terminated, worker restarted".into(),
            Outcome::CpuLimit { exit } => format!("JOB_OBJECT_MSG_END_OF_JOB_TIME, exit 0x{exit:X}"),
            Outcome::MemoryLimit { exit } => format!("JOB memory-limit msg, exit 0x{exit:X}"),
            Outcome::WorkerCrashed { exit, msgs } => format!("exit 0x{exit:X} {msgs}"),
            Outcome::ProtocolError(s) => s.clone(),
        };
        d.replace('|', "/").replace(['\n', '\r', '\u{c}'], " ").chars().take(110).collect()
    }
    fn worker_died(&self) -> bool {
        !matches!(self, Outcome::Ok { .. } | Outcome::Error { .. })
    }
}

// ---------------------------------------------------------------- spawn

unsafe fn make_job(cfg: &Cfg) -> Result<(HANDLE, HANDLE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION), String> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(format!("CreateJobObject: {}", last_err()));
        }
        let mut li: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        li.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_JOB_MEMORY
            | JOB_OBJECT_LIMIT_JOB_TIME;
        li.BasicLimitInformation.ActiveProcessLimit = 1;
        li.BasicLimitInformation.PerJobUserTimeLimit = (cfg.cpu_ms * 10_000) as i64;
        li.ProcessMemoryLimit = cfg.mem;
        li.JobMemoryLimit = cfg.mem;
        if SetInformationJobObject(job, JobObjectExtendedLimitInformation, (&li as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), size_of_val(&li) as u32) == 0 {
            return Err(format!("SetInformationJobObject(limits): {}", last_err()));
        }
        let ui_bits: u32 = std::env::var("SRH_UI").ok().and_then(|v| u32::from_str_radix(&v, 16).ok()).unwrap_or(0xFF);
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS { UIRestrictionsClass: ui_bits };
        if SetInformationJobObject(job, JobObjectBasicUIRestrictions, (&ui as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(), size_of_val(&ui) as u32) == 0 {
            return Err(format!("SetInformationJobObject(ui): {}", last_err()));
        }
        let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1);
        let acp = JOBOBJECT_ASSOCIATE_COMPLETION_PORT { CompletionKey: job, CompletionPort: port };
        if SetInformationJobObject(job, JobObjectAssociateCompletionPortInformation, (&acp as *const JOBOBJECT_ASSOCIATE_COMPLETION_PORT).cast(), size_of_val(&acp) as u32) == 0 {
            return Err(format!("SetInformationJobObject(port): {}", last_err()));
        }
        Ok((job, port, li))
    }
}

unsafe fn pipe() -> Result<(HANDLE, HANDLE), String> {
    unsafe {
        let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: std::ptr::null_mut(), bInheritHandle: 1 };
        let (mut r, mut wr) = (std::ptr::null_mut(), std::ptr::null_mut());
        if CreatePipe(&mut r, &mut wr, &sa, 1 << 20) == 0 {
            return Err(format!("CreatePipe: {}", last_err()));
        }
        Ok((r, wr))
    }
}

unsafe fn low_il_token() -> Result<HANDLE, String> {
    unsafe {
        let mut tok = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT | TOKEN_ASSIGN_PRIMARY, &mut tok) == 0 {
            return Err(format!("OpenProcessToken: {}", last_err()));
        }
        let mut rt = std::ptr::null_mut();
        // Strip every privilege except SeChangeNotify, then drop to Low IL.
        if CreateRestrictedToken(tok, DISABLE_MAX_PRIVILEGE, 0, std::ptr::null(), 0, std::ptr::null(), 0, std::ptr::null(), &mut rt) == 0 {
            return Err(format!("CreateRestrictedToken: {}", last_err()));
        }
        CloseHandle(tok);
        let mut sid = std::ptr::null_mut();
        if ConvertStringSidToSidW(w("S-1-16-4096").as_ptr(), &mut sid) == 0 {
            return Err(format!("ConvertStringSidToSid: {}", last_err()));
        }
        let tml = TOKEN_MANDATORY_LABEL { Label: SID_AND_ATTRIBUTES { Sid: sid, Attributes: SE_GROUP_INTEGRITY as u32 } };
        if SetTokenInformation(rt, TokenIntegrityLevel, (&tml as *const TOKEN_MANDATORY_LABEL).cast(), size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(sid)) == 0 {
            return Err(format!("SetTokenInformation(IL): {}", last_err()));
        }
        Ok(rt)
    }
}

unsafe fn app_container_sid() -> Result<PSID, String> {
    unsafe {
        let name = w(AC_NAME);
        let mut sid: PSID = std::ptr::null_mut();
        let hr = CreateAppContainerProfile(name.as_ptr(), name.as_ptr(), w("Selene sr-extract M0 spike").as_ptr(), std::ptr::null(), 0, &mut sid);
        if hr == 0 {
            return Ok(sid);
        }
        // 0x800700B7 = ERROR_ALREADY_EXISTS
        let hr2 = DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid);
        if hr2 == 0 { Ok(sid) } else { Err(format!("CreateAppContainerProfile 0x{hr:X}, Derive 0x{hr2:X}")) }
    }
}

/// Grant the AppContainer SID read+execute on one file (we own the file; no admin needed).
unsafe fn grant_ac(path: &Path, sid: PSID) -> Result<(), String> {
    unsafe {
        let mut p = w(&path.to_string_lossy());
        let mut old: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let e = GetNamedSecurityInfoW(p.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), &mut old, std::ptr::null_mut(), &mut sd);
        if e != 0 {
            return Err(format!("GetNamedSecurityInfo {e}"));
        }
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_READ | GENERIC_EXECUTE,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
                ptstrName: sid as *mut u16,
            },
        };
        let mut new: *mut ACL = std::ptr::null_mut();
        let e = SetEntriesInAclW(1, &ea, old, &mut new);
        if e != 0 {
            return Err(format!("SetEntriesInAcl {e}"));
        }
        let e = SetNamedSecurityInfoW(p.as_mut_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), new, std::ptr::null());
        if e != 0 {
            return Err(format!("SetNamedSecurityInfo {e}"));
        }
        Ok(())
    }
}

fn spawn(cfg: &Cfg) -> Result<Worker, String> {
    let t0 = Instant::now();
    unsafe {
        let (job, port, limits) = make_job(cfg)?;
        let (in_r, in_w) = pipe()?;
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        for h in [in_w, out_r, err_r] {
            SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
        }

        // Attribute list: handle allowlist, child-process ban, mitigations, [AppContainer].
        let handles = [in_r, out_w, err_w];
        let child_policy: u32 = if std::env::var("SRH_NO_CHILD").is_ok() { 0 } else { 1 }; // PROCESS_CREATION_CHILD_PROCESS_RESTRICTED
        let mut mitig: u64 = 0;
        if cfg.mitig {
            mitig = MIT_HEAP_TERMINATE | MIT_BOTTOM_UP_ASLR | MIT_STRICT_HANDLE_CHECKS | MIT_EXTENSION_POINT_DISABLE | MIT_PROHIBIT_DYNAMIC_CODE | MIT_IMAGE_NO_REMOTE | MIT_IMAGE_NO_LOW_LABEL;
        }
        if cfg.win32k_off {
            mitig |= MIT_WIN32K_DISABLE;
        }
        let mut sc: SECURITY_CAPABILITIES = std::mem::zeroed();
        let mut n_attr = 2;
        if mitig != 0 {
            n_attr += 1;
        }
        if cfg.mode == Mode::AppContainer {
            let sid = app_container_sid()?;
            grant_ac(&cfg.worker, sid)?;
            grant_ac(&cfg.pdfium, sid)?;
            sc.AppContainerSid = sid;
            n_attr += 1;
        }
        let mut size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), n_attr, 0, &mut size);
        let mut attr_buf = vec![0u8; size];
        let attrs: LPPROC_THREAD_ATTRIBUTE_LIST = attr_buf.as_mut_ptr().cast();
        if InitializeProcThreadAttributeList(attrs, n_attr, 0, &mut size) == 0 {
            return Err(format!("InitializeProcThreadAttributeList: {}", last_err()));
        }
        let upd = |a: u32, v: *const c_void, n: usize, what: &str| -> Result<(), String> {
            if UpdateProcThreadAttribute(attrs, 0, a as usize, v, n, std::ptr::null_mut(), std::ptr::null()) == 0 {
                Err(format!("UpdateProcThreadAttribute({what}): {}", last_err()))
            } else {
                Ok(())
            }
        };
        upd(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, handles.as_ptr().cast(), size_of_val(&handles), "handles")?;
        upd(PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, (&child_policy as *const u32).cast(), 4, "child")?;
        if mitig != 0 {
            upd(PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, (&mitig as *const u64).cast(), 8, "mitigation")?;
        }
        if cfg.mode == Mode::AppContainer {
            upd(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, (&sc as *const SECURITY_CAPABILITIES).cast(), size_of_val(&sc), "appcontainer")?;
        }

        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = in_r;
        si.StartupInfo.hStdOutput = out_w;
        si.StartupInfo.hStdError = err_w;
        si.lpAttributeList = attrs;

        let mut cmd = w(&format!("\"{}\" --pdfium \"{}\"", cfg.worker.display(), cfg.pdfium.display()));
        let app = w(&cfg.worker.to_string_lossy());
        // Minimal environment: nothing from the parent leaks into the worker.
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let mut envs = format!("SystemRoot={sysroot}\0SR_EXTRACT_PROBES=1\0");
        if std::env::var("SRH_W32K_SELF").is_ok() {
            envs.push_str("SR_EXTRACT_W32K_SELF=1\0");
        }
        if cfg.mode == Mode::AppContainer {
            // CreateProcess(AppContainer) fails with ERROR_ENVVAR_NOT_FOUND (203) unless the
            // block carries the profile vars it uses to build the container's redirected dirs.
            for k in ["USERPROFILE", "LOCALAPPDATA", "APPDATA", "TEMP", "TMP", "HOMEDRIVE", "HOMEPATH", "USERNAME"] {
                if let Ok(v) = std::env::var(k) {
                    envs.push_str(&format!("{k}={v}\0"));
                }
            }
        }
        envs.push('\0');
        let env: Vec<u16> = envs.encode_utf16().collect();
        let cwd = w(&format!(r"{sysroot}\System32"));
        // DETACHED_PROCESS, not CREATE_NO_WINDOW: a console app with CREATE_NO_WINDOW still gets a
        // conhost.exe spawned for it, which CHILD_PROCESS_RESTRICTED (and ActiveProcessLimit=1)
        // blocks -> kernelbase console init fails -> STATUS_DLL_INIT_FAILED (0xC0000142).
        // Override with SRH_NOWINDOW=1 to reproduce.
        let console = if std::env::var("SRH_NOWINDOW").is_ok() { CREATE_NO_WINDOW } else { DETACHED_PROCESS };
        let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | console | CREATE_UNICODE_ENVIRONMENT;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = match cfg.mode {
            Mode::LowIl => {
                let tok = low_il_token()?;
                let r = CreateProcessAsUserW(tok, app.as_ptr(), cmd.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 1, flags, env.as_ptr().cast(), cwd.as_ptr(), &si.StartupInfo, &mut pi);
                CloseHandle(tok);
                r
            }
            _ => CreateProcessW(app.as_ptr(), cmd.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 1, flags, env.as_ptr().cast(), cwd.as_ptr(), &si.StartupInfo, &mut pi),
        };
        DeleteProcThreadAttributeList(attrs);
        if ok == 0 {
            return Err(format!("CreateProcess({:?}): {}", cfg.mode, last_err()));
        }
        if AssignProcessToJobObject(job, pi.hProcess) == 0 {
            let e = last_err();
            TerminateProcess(pi.hProcess, 1);
            return Err(format!("AssignProcessToJobObject: {e}"));
        }
        ResumeThread(pi.hThread);
        CloseHandle(pi.hThread);
        for h in [in_r, out_w, err_w] {
            CloseHandle(h);
        }

        let stdin = File::from_raw_handle(in_w);
        let mut stdout = File::from_raw_handle(out_r);
        let mut stderr = File::from_raw_handle(err_r);
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            loop {
                let r = read_frame(&mut stdout, MAX_RESPONSE);
                let stop = !matches!(r, Ok(Some(_)));
                if tx.send(r).is_err() || stop {
                    break;
                }
            }
        });
        let pid = pi.dwProcessId;
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            let mut total = 0usize;
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                total += n;
                if total < 16 * 1024 {
                    eprint!("[worker {pid} stderr] {}", String::from_utf8_lossy(&buf[..n]));
                }
            }
        });
        let ready = match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(Some(f))) => serde_json::from_slice::<Ready>(&f).map_err(|e| format!("bad ready: {e}"))?,
            other => {
                let mut code = 0u32;
                WaitForSingleObject(pi.hProcess, 2000);
                GetExitCodeProcess(pi.hProcess, &mut code);
                return Err(format!("worker did not become ready: {other:?}, exit 0x{code:X}"));
            }
        };
        Ok(Worker { job, port, process: pi.hProcess, pid, stdin, rx, ready, spawn_ms: t0.elapsed().as_millis(), limits })
    }
}

impl Worker {
    fn job_msgs(&self) -> Vec<u32> {
        let mut v = vec![];
        unsafe {
            loop {
                let (mut msg, mut key, mut ov) = (0u32, 0usize, std::ptr::null_mut::<OVERLAPPED>());
                if GetQueuedCompletionStatus(self.port, &mut msg, &mut key, &mut ov, 0) == 0 {
                    break;
                }
                v.push(msg);
            }
        }
        v
    }

    fn peak_mb(&self) -> usize {
        unsafe {
            let mut li: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            QueryInformationJobObject(self.job, JobObjectExtendedLimitInformation, (&mut li as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), size_of_val(&li) as u32, std::ptr::null_mut());
            li.PeakProcessMemoryUsed >> 20
        }
    }

    fn cpu_ms(&self) -> u64 {
        unsafe {
            let mut a: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            QueryInformationJobObject(self.job, JobObjectBasicAccountingInformation, (&mut a as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(), size_of_val(&a) as u32, std::ptr::null_mut());
            (a.TotalUserTime / 10_000) as u64
        }
    }

    /// Re-arm the per-request CPU budget. Setting JOB_TIME again (without PRESERVE_JOB_TIME)
    /// makes the limit relative to the CPU already consumed by the job.
    fn arm_cpu_budget(&self) {
        unsafe {
            SetInformationJobObject(self.job, JobObjectExtendedLimitInformation, (&self.limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), size_of_val(&self.limits) as u32);
        }
    }

    fn request(&mut self, id: u64, kind: &str, payload: &[u8], wall: Duration) -> Outcome {
        if payload.len() > MAX_PAYLOAD as usize {
            return Outcome::Error { code: "FileTooLarge".into(), msg: format!("{} B > {} B cap; rejected by harness, worker untouched", payload.len(), MAX_PAYLOAD) };
        }
        self.arm_cpu_budget();
        let hdr = serde_json::to_vec(&Request { id, kind: kind.into() }).unwrap();
        if let Err(e) = write_frame(&mut self.stdin, &hdr).and_then(|_| write_frame(&mut self.stdin, payload)) {
            return self.classify_death(format!("write failed: {e}"));
        }
        match self.rx.recv_timeout(wall) {
            Ok(Ok(Some(f))) => match serde_json::from_slice::<Response>(&f) {
                Ok(r) if r.id != id => Outcome::ProtocolError(format!("id mismatch {} != {id}", r.id)),
                Ok(r) if r.ok => {
                    let text = r.text.unwrap_or_default();
                    Outcome::Ok { chars: r.chars, pages: r.pages, truncated: r.truncated, sample: text.chars().filter(|c| !c.is_control()).take(40).collect() }
                }
                Ok(r) => Outcome::Error { code: r.code.unwrap_or_default(), msg: r.msg.unwrap_or_default() },
                Err(e) => Outcome::ProtocolError(format!("bad response json: {e}")),
            },
            Ok(Ok(None)) => self.classify_death("EOF".into()),
            Ok(Err(e)) => {
                // Includes an over-cap length prefix from a compromised worker.
                if e.kind() == std::io::ErrorKind::InvalidData {
                    Outcome::ProtocolError(e.to_string())
                } else {
                    self.classify_death(format!("read: {e}"))
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let msgs = self.job_msgs();
                let tk = Instant::now();
                unsafe {
                    TerminateJobObject(self.job, EXIT_TIMEOUT_KILL);
                    WaitForSingleObject(self.process, 10_000);
                }
                LIVE_KILL_US.lock().unwrap().push(tk.elapsed().as_micros());
                if msgs.contains(&JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT) || msgs.contains(&JOB_OBJECT_MSG_JOB_MEMORY_LIMIT) {
                    // Hit the commit cap and then wedged (allocator retry / OOM loop): still a timeout
                    // for the catalog, but note the memory cause.
                    return Outcome::Error { code: "ExtractTimeout".into(), msg: "wall-clock timeout after job memory-limit msg (worker wedged at cap); job terminated".into() };
                }
                Outcome::ExtractTimeout
            }
            Err(RecvTimeoutError::Disconnected) => self.classify_death("reader gone".into()),
        }
    }

    fn classify_death(&self, why: String) -> Outcome {
        let mut code = 0u32;
        unsafe {
            WaitForSingleObject(self.process, 5000);
            GetExitCodeProcess(self.process, &mut code);
        }
        let msgs = self.job_msgs();
        if msgs.contains(&JOB_OBJECT_MSG_END_OF_JOB_TIME) || msgs.contains(&JOB_OBJECT_MSG_END_OF_PROCESS_TIME) {
            return Outcome::CpuLimit { exit: code };
        }
        if msgs.contains(&JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT) || msgs.contains(&JOB_OBJECT_MSG_JOB_MEMORY_LIMIT) {
            return Outcome::MemoryLimit { exit: code };
        }
        Outcome::WorkerCrashed { exit: code, msgs: format!("({why}; job msgs {msgs:?})") }
    }
}

// ---------------------------------------------------------------- suite

struct Supervisor {
    cfg: Cfg,
    w: Option<Worker>,
    restarts: u32,
    next_id: u64,
    spawn_times: Vec<u128>,
    kill_times: Vec<u128>,
}

impl Supervisor {
    fn worker(&mut self) -> &mut Worker {
        if self.w.is_none() {
            let wk = spawn(&self.cfg).unwrap_or_else(|e| panic!("spawn failed: {e}"));
            self.spawn_times.push(wk.spawn_ms);
            self.w = Some(wk);
        }
        self.w.as_mut().unwrap()
    }

    fn run(&mut self, kind: &str, payload: &[u8]) -> (Outcome, u128, u32, usize, u64) {
        self.next_id += 1;
        let id = self.next_id;
        let wall = Duration::from_millis(self.cfg.wall_ms);
        let wk = self.worker();
        let pid = wk.pid;
        let t = Instant::now();
        let out = wk.request(id, kind, payload, wall);
        let ms = t.elapsed().as_millis();
        let peak = wk.peak_mb();
        let cpu = wk.cpu_ms();
        let wedged = matches!(&out, Outcome::Error { code, .. } if code == "ExtractTimeout");
        if out.worker_died() || wedged {
            let wk = self.w.as_ref().unwrap();
            let t = Instant::now();
            unsafe {
                TerminateJobObject(wk.job, EXIT_TIMEOUT_KILL);
                WaitForSingleObject(wk.process, 10_000);
            }
            self.kill_times.push(t.elapsed().as_micros());
            self.w = None; // Drop terminates + closes the job; next call respawns.
            self.restarts += 1;
        }
        (out, ms, pid, peak, cpu)
    }

    fn legit_ok(&mut self) -> bool {
        let b = std::fs::read(self.cfg.fixtures.join("sample.pdf")).unwrap();
        matches!(self.run("pdf", &b).0, Outcome::Ok { ref sample, .. } if sample.contains("Selene retrieval spike page 1"))
    }
}

fn kind_of(name: &str) -> &'static str {
    match Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("") {
        "pdf" => "pdf",
        "docx" => "docx",
        "html" => "html",
        "md" => "md",
        _ => "txt",
    }
}

fn parse_args() -> (Cfg, bool) {
    let a: Vec<String> = std::env::args().collect();
    let orphan = a.iter().any(|x| x == "--orphan");
    let pos: Vec<&String> = a.iter().skip(1).filter(|x| !x.starts_with("--")).collect();
    let opt = |k: &str| a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned();
    let mode = match opt("--mode").as_deref() {
        Some("none") => Mode::None,
        Some("appcontainer") => Mode::AppContainer,
        _ => Mode::LowIl,
    };
    let abs = |p: &str| std::fs::canonicalize(p).map(|p| PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\"))).unwrap_or_else(|e| panic!("{p}: {e}"));
    // positional args exclude option values
    let opt_vals: Vec<String> = ["--mode", "--mem-mb", "--cpu-ms", "--wall-ms"].iter().filter_map(|k| opt(k)).collect();
    let pos: Vec<&String> = pos.into_iter().filter(|p| !opt_vals.contains(p)).collect();
    (
        Cfg {
            worker: abs(pos[0]),
            pdfium: abs(pos[1]),
            fixtures: abs(pos[2]),
            mode,
            mem: opt("--mem-mb").map(|v| v.parse().unwrap()).unwrap_or(512usize) << 20,
            cpu_ms: opt("--cpu-ms").map(|v| v.parse().unwrap()).unwrap_or(10_000),
            wall_ms: opt("--wall-ms").map(|v| v.parse().unwrap()).unwrap_or(15_000),
            mitig: !a.iter().any(|x| x == "--no-mitig"),
            win32k_off: a.iter().any(|x| x == "--win32k-off"),
        },
        orphan,
    )
}

fn main() {
    let (cfg, orphan) = parse_args();
    if orphan {
        // Child half of the KILL_ON_JOB_CLOSE test: start a worker, make it hang, print its
        // pid, then exit abruptly without any cleanup (no Drop runs).
        let mut wk = spawn(&cfg).expect("spawn");
        let hdr = serde_json::to_vec(&Request { id: 1, kind: "probe:hang".into() }).unwrap();
        write_frame(&mut wk.stdin, &hdr).unwrap();
        write_frame(&mut wk.stdin, b"").unwrap();
        println!("PID {}", wk.pid);
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        std::process::exit(0);
    }

    let mut sv = Supervisor { cfg: cfg.clone(), w: None, restarts: 0, next_id: 0, spawn_times: vec![], kill_times: vec![] };
    let r = sv.worker().ready.clone();
    println!("## mode={:?} mem={} MiB cpu={} ms wall={} ms mitigations={} win32k_off={}", cfg.mode, cfg.mem >> 20, cfg.cpu_ms, cfg.wall_ms, cfg.mitig, cfg.win32k_off);
    println!("worker ready: pid={} integrity={} app_container={} pdfium_load_ms={} spawn_to_ready_ms={}\n", r.pid, r.integrity, r.app_container, r.pdfium_load_ms, sv.spawn_times[0]);
    println!("| # | case | outcome | detail | ms | pid | peak MiB | job CPU ms | restarts | legit after |");
    println!("|---|---|---|---|---|---|---|---|---|---|");

    let mut rows = 0;
    let mut fails = 0;
    let mut row = |sv: &mut Supervisor, case: &str, kind: &str, payload: &[u8], expect: &dyn Fn(&Outcome) -> bool, check_after: bool| {
        rows += 1;
        let (o, ms, pid, peak, cpu) = sv.run(kind, payload);
        let legit = if check_after { if sv.legit_ok() { "yes" } else { "NO" } } else { "-" };
        let pass = expect(&o) && legit != "NO";
        if !pass {
            fails += 1;
        }
        println!("| {rows} | {case} | {}{} | {} | {ms} | {pid} | {peak} | {cpu} | {} | {legit} |", if pass { "" } else { "**FAIL** " }, o.label(), o.detail(), sv.restarts);
    };
    let ok = |o: &Outcome| matches!(o, Outcome::Ok { .. });
    let any = |_: &Outcome| true;
    let contained = |o: &Outcome| !matches!(o, Outcome::ProtocolError(_));

    // A. legit baseline
    for f in ["sample.pdf", "big-200p.pdf", "sample.docx", "sample.html", "sample.md"] {
        let b = std::fs::read(cfg.fixtures.join(f)).unwrap();
        row(&mut sv, f, kind_of(f), &b, &ok, false);
    }

    // B. sandbox probes (spike-only request kinds)
    let spike_dir = cfg.fixtures.parent().unwrap().to_path_buf();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let user_dir = PathBuf::from(std::env::var("USERPROFILE").unwrap());
    let probes: Vec<(&str, String, Box<dyn Fn(&Outcome) -> bool>)> = vec![
        ("probe:hang", String::new(), Box::new(|o: &Outcome| matches!(o, Outcome::ExtractTimeout))),
        ("probe:spin", String::new(), Box::new(|o: &Outcome| matches!(o, Outcome::CpuLimit { .. } | Outcome::ExtractTimeout))),
        ("probe:alloc", String::new(), Box::new(ok)),
        ("probe:alloc_abort", String::new(), Box::new(|o: &Outcome| o.worker_died())),
        ("probe:crash", String::new(), Box::new(|o: &Outcome| o.worker_died())),
        ("probe:stack", String::new(), Box::new(|o: &Outcome| o.worker_died())),
        ("probe:spawn", String::new(), Box::new(any)),
        ("probe:dyncode", String::new(), Box::new(any)),
        ("probe:write", spike_dir.join("target").join("probe-write.txt").display().to_string(), Box::new(any)),
        ("probe:write", user_dir.join("sr-probe-write.txt").display().to_string(), Box::new(any)),
        ("probe:write", spike_dir.join("target").join("worker-tmp-low").join("probe-write.txt").display().to_string(), Box::new(any)),
        ("probe:read", cfg.fixtures.join("sample.pdf").display().to_string(), Box::new(any)),
        ("probe:read", user_dir.join("NTUSER.DAT").display().to_string(), Box::new(any)),
        ("probe:net", addr.clone(), Box::new(any)),
    ];
    for (k, arg, exp) in &probes {
        let case = if arg.is_empty() { k.to_string() } else { format!("{k} `{}`", arg.replace('\\', "/").rsplit('/').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/")) };
        row(&mut sv, &case, k, arg.as_bytes(), exp.as_ref(), true);
    }
    drop(listener);

    // C. hostile fixtures (everything not used as a legit baseline)
    let mut names: Vec<String> = std::fs::read_dir(&cfg.fixtures).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
    names.sort();
    for f in names.iter().filter(|n| !n.starts_with("sample.") && *n != "big-200p.pdf") {
        let b = std::fs::read(cfg.fixtures.join(f)).unwrap();
        let case = format!("{f} ({} KiB)", b.len() / 1024);
        row(&mut sv, &case, kind_of(f), &b, &contained, true);
    }

    // D. warm throughput + respawn cost
    let b = std::fs::read(cfg.fixtures.join("sample.pdf")).unwrap();
    let t = Instant::now();
    let mut n_ok = 0;
    for _ in 0..100 {
        if matches!(sv.run("pdf", &b).0, Outcome::Ok { .. }) {
            n_ok += 1;
        }
    }
    let warm = t.elapsed().as_secs_f64() * 1000.0 / 100.0;
    let mut tput = vec![];
    for (f, n) in [("big-200p.pdf", 20u32), ("sample.docx", 200), ("sample.html", 200), ("bidi-zw.txt", 20)] {
        let b = std::fs::read(cfg.fixtures.join(f)).unwrap();
        let t = Instant::now();
        let mut okn = 0;
        for _ in 0..n {
            if matches!(sv.run(kind_of(f), &b).0, Outcome::Ok { .. }) {
                okn += 1;
            }
        }
        let s = t.elapsed().as_secs_f64();
        tput.push(format!("{f}: {:.2} ms/file, {:.0} files/s, {:.1} MiB/s ({okn}/{n} ok)", s * 1000.0 / n as f64, n as f64 / s, (b.len() as f64 * n as f64) / s / 1048576.0));
    }
    let idle_peak = sv.worker().peak_mb();

    // E. KILL_ON_JOB_CLOSE: a harness that dies abruptly must take its worker with it.
    let exe = std::env::current_exe().unwrap();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    args.push("--orphan".into());
    let out = std::process::Command::new(exe).args(&args).output().unwrap();
    let pid: u32 = String::from_utf8_lossy(&out.stdout).trim().trim_start_matches("PID ").parse().unwrap_or(0);
    let orphan_result = unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if pid == 0 {
            format!("FAIL (no pid; stderr {})", String::from_utf8_lossy(&out.stderr))
        } else if h.is_null() {
            "PASS (worker already gone)".to_string()
        } else {
            let r = WaitForSingleObject(h, 3000);
            CloseHandle(h);
            if r == WAIT_OBJECT_0 { "PASS (worker terminated with its harness)".into() } else { format!("FAIL (worker {pid} still alive)") }
        }
    };

    let st = &sv.spawn_times;
    println!("\nwarm sample.pdf round trip: {warm:.2} ms avg over 100 ({n_ok} ok)");
    println!("worker spawns: {} (restarts {}), spawn-to-ready ms min/avg/max: {}/{}/{}", st.len(), sv.restarts, st.iter().min().unwrap(), st.iter().sum::<u128>() / st.len() as u128, st.iter().max().unwrap());
    for l in &tput {
        println!("throughput {l}");
    }
    let kt = &sv.kill_times;
    if !kt.is_empty() {
        println!("kill (TerminateJobObject -> process signalled) us min/avg/max: {}/{}/{} over {}", kt.iter().min().unwrap(), kt.iter().sum::<u128>() / kt.len() as u128, kt.iter().max().unwrap(), kt.len());
    }
    let lk = LIVE_KILL_US.lock().unwrap().clone();
    if !lk.is_empty() {
        println!("live kill on timeout (TerminateJobObject -> hung worker exited) us: {lk:?}");
    }
    println!("final worker lifetime peak commit: {idle_peak} MiB");
    println!("KILL_ON_JOB_CLOSE orphan test: {orphan_result}");
    println!("harness survived: yes; rows={rows} fails={fails}");
}
