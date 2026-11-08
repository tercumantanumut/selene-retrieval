//! Pipe access control (Amendment 3.10): protected DACL with only the user's SID, Medium
//! mandatory label with no-read-up/no-write-up, and remote clients rejected.
#![allow(unsafe_code)]
// Test code: panicking on unexpected errors is the point. Each #[tokio::test] owns its runtime,
// so holding the cross-test serial lock across awaits cannot deadlock.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]

mod common;

use std::ffi::c_void;
use std::time::{Duration, Instant};

use common::*;
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{
    CreateRestrictedToken, DuplicateTokenEx, GetTokenInformation, SID_AND_ATTRIBUTES,
    SecurityImpersonation, SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY,
    TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER,
    TokenImpersonation, TokenIntegrityLevel, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING, READ_CONTROL};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken, SetThreadToken};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// CreateFileW on the pipe; retries ERROR_PIPE_BUSY so every probe gets a real access decision.
fn open_raw(name: &str, access: u32) -> Result<HANDLE, u32> {
    let w = wide(name);
    let dl = Instant::now() + Duration::from_secs(3);
    loop {
        // SAFETY: plain CreateFileW.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                access,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            return Ok(h);
        }
        // SAFETY: no preconditions.
        let e = unsafe { GetLastError() };
        if e != 231 || Instant::now() >= dl {
            return Err(e);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn result(r: Result<HANDLE, u32>) -> Result<(), u32> {
    r.map(|h| {
        // SAFETY: handle from CreateFileW.
        unsafe { CloseHandle(h) };
    })
}

/// Runs `f` on a fresh thread impersonating `token` (so no other test thread is affected).
fn impersonating<R: Send + 'static>(token: isize, f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::spawn(move || {
        // SAFETY: the token is an impersonation token; reverted before the thread ends.
        unsafe {
            assert_ne!(SetThreadToken(std::ptr::null(), token as HANDLE), 0);
            let r = f();
            SetThreadToken(std::ptr::null(), std::ptr::null_mut());
            CloseHandle(token as HANDLE);
            r
        }
    })
    .join()
    .unwrap()
}

/// Impersonation token of this process with the user SID turned deny-only ("another user":
/// the same DACL check a real second account hits; creating one needs admin).
fn other_principal_token() -> isize {
    // SAFETY: token APIs with checked results; buffers outlive the calls.
    unsafe {
        let mut tok: HANDLE = std::ptr::null_mut();
        assert_ne!(
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_IMPERSONATE,
                &mut tok
            ),
            0
        );
        let mut buf = vec![0u64; 64];
        let mut len = 0;
        GetTokenInformation(tok, TokenUser, buf.as_mut_ptr().cast(), 512, &mut len);
        let sid = (*(buf.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        let dis = SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: 0,
        };
        let mut rt: HANDLE = std::ptr::null_mut();
        assert_ne!(
            CreateRestrictedToken(
                tok,
                0,
                1,
                &dis,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                &mut rt
            ),
            0
        );
        let mut imp: HANDLE = std::ptr::null_mut();
        DuplicateTokenEx(
            rt,
            TOKEN_IMPERSONATE | TOKEN_QUERY,
            std::ptr::null(),
            SecurityImpersonation,
            TokenImpersonation,
            &mut imp,
        );
        CloseHandle(rt);
        CloseHandle(tok);
        imp as isize
    }
}

/// Impersonation token of this process at integrity level `il` (e.g. Low = S-1-16-4096).
fn integrity_token(il: &str) -> isize {
    // SAFETY: token APIs with checked results.
    unsafe {
        let mut t: HANDLE = std::ptr::null_mut();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_ADJUST_DEFAULT,
            &mut t,
        );
        let mut dup: HANDLE = std::ptr::null_mut();
        DuplicateTokenEx(
            t,
            TOKEN_IMPERSONATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT,
            std::ptr::null(),
            SecurityImpersonation,
            TokenImpersonation,
            &mut dup,
        );
        CloseHandle(t);
        let w = wide(il);
        let mut psid: *mut c_void = std::ptr::null_mut();
        assert_ne!(ConvertStringSidToSidW(w.as_ptr(), &mut psid), 0);
        let tml = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: psid,
                Attributes: 0x20, // SE_GROUP_INTEGRITY
            },
        };
        let ok = SetTokenInformation(
            dup,
            TokenIntegrityLevel,
            (&tml as *const TOKEN_MANDATORY_LABEL).cast(),
            std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + 16,
        );
        LocalFree(psid);
        assert_ne!(ok, 0, "SetTokenInformation(IL)");
        dup as isize
    }
}

#[test]
fn pipe_denies_other_user_low_integrity_and_remote() {
    let _s = serial();
    let env = TestEnv::new("acl");
    env.start_daemon(&[]);
    let name = env.pipe_name();
    let sid = sr_client::win::user_sid_string().unwrap();
    let rw = GENERIC_READ | GENERIC_WRITE;

    // Effective descriptor, read through a client handle.
    let h = open_raw(&name, READ_CONTROL | GENERIC_READ).unwrap();
    let sddl = sr_client::win::handle_sddl(h.cast()).unwrap();
    // SAFETY: handle from CreateFileW.
    unsafe { CloseHandle(h) };
    let dacl = sddl
        .split("D:")
        .nth(1)
        .unwrap_or("")
        .split("S:")
        .next()
        .unwrap_or("");
    let aces: Vec<&str> = dacl.split('(').skip(1).collect();
    record(&format!("pipe effective SDDL: {sddl}"));
    assert!(dacl.starts_with('P'), "protected DACL: {sddl}");
    assert_eq!(aces.len(), 1, "one ACE: {sddl}");
    assert!(
        aces[0].starts_with("A;;FA;;;") && aces[0].contains(&sid),
        "{sddl}"
    );
    assert!(
        sddl.contains("(ML;;NWNR;;;ME)"),
        "medium label NW+NR: {sddl}"
    );

    // Same user, medium IL (control): allowed.
    assert_eq!(result(open_raw(&name, rw)), Ok(()));

    // Another principal: denied.
    let n2 = name.clone();
    let r = impersonating(other_principal_token(), move || result(open_raw(&n2, rw)));
    record(&format!(
        "other principal (user SID deny-only) read+write: {r:?}"
    ));
    assert_eq!(r, Err(5));

    // Same user at Low IL: denied read+write AND read-only (the NR label bit).
    let n3 = name.clone();
    let (lrw, lro) = impersonating(integrity_token("S-1-16-4096"), move || {
        (
            result(open_raw(&n3, rw)),
            result(open_raw(&n3, GENERIC_READ)),
        )
    });
    record(&format!(
        "same user, Low IL: read+write {lrw:?}, read-only {lro:?}"
    ));
    assert_eq!(lrw, Err(5));
    assert_eq!(lro, Err(5));

    // Remote path (SMB loopback = remote client): rejected by PIPE_REJECT_REMOTE_CLIENTS.
    let remote = name.replacen(r"\\.\", r"\\localhost\", 1);
    let rr = result(open_raw(&remote, rw));
    record(&format!("remote (\\\\localhost\\pipe) read+write: {rr:?}"));
    assert!(rr.is_err(), "remote client must not connect");
    if rr != Err(5) {
        record(
            "NOTE remote probe failed with a non-ACCESS_DENIED error (SMB server service unavailable?)",
        );
    }

    // Control pipe (default descriptor, no reject-remote flag): shows whether the remote probe
    // itself can reach a pipe on this machine (informational).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ctrl = format!(r"\\.\pipe\sr-m1c-control-{}", std::process::id());
    let _guard = rt.enter();
    let _srv = tokio::net::windows::named_pipe::ServerOptions::new()
        .reject_remote_clients(false)
        .create(&ctrl);
    let ctrl_remote = result(open_raw(&ctrl.replacen(r"\\.\", r"\\localhost\", 1), rw));
    record(&format!(
        "control pipe (no reject-remote) via \\\\localhost: {ctrl_remote:?}"
    ));
}

/// Handshake hardening: garbage, unknown kind and oversize lines are refused with `invalid`, and
/// a silent probe connection doesn't disturb the daemon.
#[test]
fn malformed_handshakes_are_refused() {
    use std::io::{BufRead, BufReader, Read, Write};
    let _s = serial();
    let env = TestEnv::new("badhs");
    let pid = env.start_daemon(&[]);
    let name = env.pipe_name();
    let send = |payload: &[u8]| -> String {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
            .unwrap();
        f.write_all(payload).unwrap();
        let mut line = String::new();
        let _ = BufReader::new(&mut f).read_line(&mut line);
        line
    };
    let garbage = send(b"hello\n");
    let kind = send(b"{\"proto\":1,\"build\":\"x\",\"kind\":\"root\",\"client_id\":\"a\"}\n");
    let mut big = vec![b'a'; 5000];
    big.push(b'\n');
    let oversize = send(&big);
    record(&format!(
        "bad handshakes: garbage={garbage:?} kind={kind:?} oversize={oversize:?}"
    ));
    assert!(garbage.contains(r#""status":"invalid""#));
    assert!(oversize.contains(r#""status":"invalid""#));
    assert!(kind.contains(r#""status":"invalid""#));
    // A connection that sends nothing: dropped after the handshake timeout, daemon unaffected.
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&name)
        .unwrap();
    let mut b = [0u8; 1];
    let t = Instant::now();
    let n = f.read(&mut b).unwrap_or(0);
    record(&format!(
        "silent connection closed by daemon after {} ms",
        t.elapsed().as_millis()
    ));
    assert_eq!(n, 0);
    assert!(alive(pid));
}
