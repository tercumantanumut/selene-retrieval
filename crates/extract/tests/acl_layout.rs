//! The pdfium-DLL ACL question from M0-C ("an AppContainer ACE on pdfium.dll broke low-IL
//! loading"): which install-directory layout lets both sandbox modes load the worker and pdfium.
//!
//! `one_install_dir_serves_both_modes` is the regression test for the chosen layout (an
//! inheritable "ALL APPLICATION PACKAGES" RX ACE on the install directory, exactly what
//! Program Files has). The `#[ignore]` experiments reproduce the M0 per-file grant and print the
//! ACLs; run them with `--ignored --nocapture`.
// Windows ACLs and AppContainer only.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

mod common;

use std::path::Path;

use common::*;
use sr_common::protocol::extract::DocKind;
use sr_extract_client::sandbox::AppContainer;
use sr_extract_client::{ActiveSandbox, ExtractPool, ExtractSource, SandboxMode};

fn fresh_stage(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = tmp_root().join("stage").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    stage(name)
}

fn icacls(p: &Path) -> String {
    let out = std::process::Command::new("icacls")
        .arg(p)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Returns (pdf ok, sandbox, pdf_engine error) for one pool run.
fn pdf_probe(
    exe: &Path,
    dll: &Path,
    mode: SandboxMode,
    profile: &str,
    acl: bool,
) -> (bool, Option<ActiveSandbox>, String) {
    let mut cfg = config(exe, dll, mode, profile);
    cfg.ensure_install_acl = acl;
    cfg.workers = 1;
    let pool = ExtractPool::start(cfg).unwrap();
    let r = rt().block_on(pool.extract(ExtractSource::Bytes(sample_pdf(1)), DocKind::Pdf));
    let stats = pool.stats();
    pool.shutdown();
    match r {
        Ok(x) => (x.text.contains("page 1"), stats.sandbox, String::new()),
        Err(e) => (
            false,
            stats.sandbox,
            format!(
                "{e:?} / fallback {:?} / pdfium {:?}",
                stats.fallback_reason, stats.pdf_engine_error
            ),
        ),
    }
}

#[test]
fn one_install_dir_serves_both_modes() {
    let (exe, dll) = fresh_stage("acl-both");
    let prof = Profile::new("acl-both");
    // AppContainer first (this grants the directory ACE), then low IL on the very same files,
    // then AppContainer again.
    let (ok, mode, err) = pdf_probe(&exe, &dll, SandboxMode::AppContainer, &prof.0, true);
    assert!(ok && mode == Some(ActiveSandbox::AppContainer), "AC: {err}");
    let (ok, mode, err) = pdf_probe(&exe, &dll, SandboxMode::LowIntegrity, &prof.0, true);
    assert!(
        ok && mode == Some(ActiveSandbox::LowIntegrity),
        "low IL after AC: {err}"
    );
    let (ok, _, err) = pdf_probe(&exe, &dll, SandboxMode::AppContainer, &prof.0, true);
    assert!(ok, "AC again: {err}");
    // The grant is the inherited directory ACE only: no per-file explicit entry.
    let acl = icacls(&dll);
    assert!(acl.contains("ALL APPLICATION PACKAGES:(I)(RX)"), "{acl}");
}

#[test]
fn without_the_install_acl_auto_falls_back_to_low_il_and_still_works() {
    let (exe, dll) = fresh_stage("acl-none");
    let prof = Profile::new("acl-none");
    let (ok, mode, err) = pdf_probe(&exe, &dll, SandboxMode::Auto, &prof.0, false);
    assert!(ok, "{err}");
    assert_eq!(mode, Some(ActiveSandbox::LowIntegrity));
}

// ------------------------------------------------------------------ M0 reproduction

/// The M0 harness's `grant_ac`: GetNamedSecurityInfo → SetEntriesInAcl(GENERIC_READ |
/// GENERIC_EXECUTE, NO_INHERITANCE, the AppContainer SID) → SetNamedSecurityInfo.
fn grant_spike_style(path: &Path, sid: *mut core::ffi::c_void) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GENERIC_EXECUTE, GENERIC_READ};
    use windows_sys::Win32::Security::Authorization::*;
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE, PSECURITY_DESCRIPTOR,
    };
    let mut p: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let mut old: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        assert_eq!(
            GetNamedSecurityInfoW(
                p.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut old,
                std::ptr::null_mut(),
                &mut sd
            ),
            0
        );
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
        assert_eq!(SetEntriesInAclW(1, &ea, old, &mut new), 0);
        assert_eq!(
            SetNamedSecurityInfoW(
                p.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                new,
                std::ptr::null()
            ),
            0
        );
    }
}

#[test]
#[ignore = "experiment: run with --ignored --nocapture"]
fn experiment_m0_per_file_grant() {
    let (exe, dll) = fresh_stage("acl-m0");
    let prof = Profile::new("acl-m0");
    println!("before:\n{}\n{}", icacls(&exe), icacls(&dll));
    let (ok, _, err) = pdf_probe(&exe, &dll, SandboxMode::LowIntegrity, &prof.0, false);
    println!("low IL before any grant: pdf ok={ok} {err}");
    let ac = AppContainer::ensure(&prof.0, "test").unwrap();
    println!("AC SID {}", ac.sid_string().unwrap());
    grant_spike_style(&exe, ac.sid());
    grant_spike_style(&dll, ac.sid());
    println!(
        "after spike-style grant:\n{}\n{}",
        icacls(&exe),
        icacls(&dll)
    );
    let (ok, mode, err) = pdf_probe(&exe, &dll, SandboxMode::AppContainer, &prof.0, false);
    println!("AC after grant: pdf ok={ok} mode={mode:?} {err}");
    let (ok, mode, err) = pdf_probe(&exe, &dll, SandboxMode::LowIntegrity, &prof.0, false);
    println!("low IL after grant: pdf ok={ok} mode={mode:?} {err}");
}

fn sid_from(s: &str) -> *mut core::ffi::c_void {
    use std::os::windows::ffi::OsStrExt;
    let w: Vec<u16> = std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut sid = std::ptr::null_mut();
    unsafe {
        assert_ne!(
            windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW(
                w.as_ptr(),
                &mut sid
            ),
            0
        );
    }
    sid
}

#[test]
#[ignore = "experiment: run with --ignored --nocapture"]
fn experiment_isolate_variables() {
    let prof = Profile::new("acl-iso");
    let ac = AppContainer::ensure(&prof.0, "test").unwrap();
    let ac_sid = ac.sid_string().unwrap();
    let cases: Vec<(&str, String, bool)> = vec![
        // (label, SID granted per file on pdfium.dll only, run AC between grant and low IL)
        ("Everyone explicit", "S-1-1-0".into(), false),
        ("Users explicit", "S-1-5-32-545".into(), false),
        (
            "ALL APPLICATION PACKAGES explicit",
            "S-1-15-2-1".into(),
            false,
        ),
        (
            "ALL RESTRICTED APPLICATION PACKAGES explicit",
            "S-1-15-2-2".into(),
            false,
        ),
        ("AC SID explicit, no AC run", ac_sid.clone(), false),
        ("AC SID explicit, AC run first", ac_sid.clone(), true),
        (
            "unrelated package SID explicit",
            "S-1-15-2-1-2-3-4-5-6-7".into(),
            false,
        ),
        (
            "capability SID explicit (internetClient)",
            "S-1-15-3-1".into(),
            false,
        ),
    ];
    for (i, (label, sid, run_ac)) in cases.into_iter().enumerate() {
        let name = format!("acl-iso-{i}");
        let (exe, dll) = fresh_stage(&name);
        grant_spike_style(&dll, sid_from(&sid));
        if run_ac {
            grant_spike_style(&exe, ac.sid());
            let (ok, _, err) = pdf_probe(&exe, &dll, SandboxMode::AppContainer, &prof.0, false);
            println!("   (AC run: ok={ok} {err})");
        }
        let (ok, _, err) = pdf_probe(&exe, &dll, SandboxMode::LowIntegrity, &prof.0, false);
        println!("{label:50} -> low-IL pdfium load ok={ok} {err}");
    }
}

#[cfg(feature = "test-probes")]
#[test]
#[ignore = "experiment: run with --features test-probes --ignored --nocapture"]
fn experiment_low_il_can_read_but_not_map() {
    let prof = Profile::new("acl-read");
    let (exe, dll) = fresh_stage("acl-read");
    grant_spike_style(&dll, sid_from("S-1-15-2-1-2-3-4-5-6-7"));
    let mut cfg = config(&exe, &dll, SandboxMode::LowIntegrity, &prof.0);
    cfg.ensure_install_acl = false;
    let pool = ExtractPool::start(cfg).unwrap();
    let probe = format!("SR-PROBE:read:{}", dll.display()).into_bytes();
    let r = rt()
        .block_on(pool.extract(ExtractSource::Bytes(probe), DocKind::Text))
        .unwrap();
    println!("low-IL read of pdfium.dll: {}", r.text);
    println!(
        "low-IL pdfium load error: {:?}",
        pool.stats().pdf_engine_error
    );
}

#[cfg(feature = "test-probes")]
#[test]
#[ignore = "experiment: run with --features test-probes --ignored --nocapture"]
fn experiment_label_vs_package_ace() {
    let prof = Profile::new("acl-lbl");
    for (i, (sid, label)) in [
        ("S-1-1-0", None),
        ("S-1-15-2-1-2-3-4-5-6-7", None),
        ("S-1-15-2-1-2-3-4-5-6-7", Some("medium")),
        ("S-1-15-2-1-2-3-4-5-6-7", Some("low")),
    ]
    .into_iter()
    .enumerate()
    {
        let (exe, dll) = fresh_stage(&format!("acl-lbl-{i}"));
        grant_spike_style(&dll, sid_from(sid));
        if let Some(l) = label {
            let o = std::process::Command::new("icacls")
                .arg(&dll)
                .args(["/setintegritylevel", l])
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
        }
        let mut cfg = config(&exe, &dll, SandboxMode::LowIntegrity, &prof.0);
        cfg.ensure_install_acl = false;
        let pool = ExtractPool::start(cfg).unwrap();
        let probe = format!("SR-PROBE:read:{}", dll.display()).into_bytes();
        let r = rt()
            .block_on(pool.extract(ExtractSource::Bytes(probe), DocKind::Text))
            .unwrap();
        println!(
            "{sid:28} label={label:?}: {} | load: {:?}",
            r.text,
            pool.stats().pdf_engine_error
        );
    }
}

/// Opens `path` for read while impersonating variants of our own token.
#[test]
#[ignore = "experiment: run with --ignored --nocapture"]
fn experiment_which_token_property() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::*;
    use windows_sys::Win32::Storage::FileSystem::*;
    use windows_sys::Win32::System::SystemServices::SE_GROUP_INTEGRITY;
    use windows_sys::Win32::System::Threading::*;
    let (_exe, dll) = fresh_stage("acl-tok");
    grant_spike_style(&dll, sid_from("S-1-15-2-1-2-3-4-5-6-7"));
    let (_e2, ctl) = fresh_stage("acl-tok-ctl");
    let paths = [("package-SID ACE", dll), ("no extra ACE", ctl)];
    unsafe {
        let mut me = std::ptr::null_mut();
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut me),
            0
        );
        for (label, restricted, low) in [
            ("medium, plain", false, false),
            ("low IL, plain", false, true),
            ("medium, DISABLE_MAX_PRIVILEGE", true, false),
            ("low IL, DISABLE_MAX_PRIVILEGE (worker)", true, true),
        ] {
            let mut base = me;
            if restricted {
                let mut rt = std::ptr::null_mut();
                assert_ne!(
                    CreateRestrictedToken(
                        me,
                        DISABLE_MAX_PRIVILEGE,
                        0,
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        &mut rt
                    ),
                    0
                );
                base = rt;
            }
            let mut imp = std::ptr::null_mut();
            assert_ne!(
                DuplicateTokenEx(
                    base,
                    TOKEN_ALL_ACCESS,
                    std::ptr::null(),
                    SecurityImpersonation,
                    TokenImpersonation,
                    &mut imp
                ),
                0
            );
            if low {
                let sid = sid_from("S-1-16-4096");
                let tml = TOKEN_MANDATORY_LABEL {
                    Label: SID_AND_ATTRIBUTES {
                        Sid: sid,
                        Attributes: SE_GROUP_INTEGRITY as u32,
                    },
                };
                assert_ne!(
                    SetTokenInformation(
                        imp,
                        TokenIntegrityLevel,
                        (&tml as *const TOKEN_MANDATORY_LABEL).cast(),
                        size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(sid)
                    ),
                    0
                );
            }
            for (what, p) in &paths {
                let w: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
                assert_ne!(SetThreadToken(std::ptr::null(), imp), 0);
                let h = CreateFileW(
                    w.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                );
                let err = std::io::Error::last_os_error();
                RevertToSelf();
                let ok = h != INVALID_HANDLE_VALUE;
                if ok {
                    CloseHandle(h);
                }
                println!(
                    "{label:42} {what:16}: {}",
                    if ok {
                        "READ OK".to_string()
                    } else {
                        format!("DENIED ({err})")
                    }
                );
            }
        }
    }
}

/// Root cause (see m1-b-extract.md): when a DACL names any specific package SID (S-1-15-2-x-…),
/// Windows checks a Low-IL / sandboxed non-AppContainer token as if it were an AppContainer, so
/// only "ALL APPLICATION PACKAGES" / "ALL RESTRICTED APPLICATION PACKAGES" ACEs can grant it.
/// The chosen layout carries the ALL APPLICATION PACKAGES ACE, so even a stray package ACE (left
/// by the M0 harness or other software, electron/electron#54382) breaks neither mode.
#[test]
fn layout_survives_a_stray_package_sid_ace() {
    let (exe, dll) = fresh_stage("acl-stray");
    let prof = Profile::new("acl-stray");
    grant_spike_style(&dll, sid_from("S-1-15-2-1-2-3-4-5-6-7"));
    grant_spike_style(&exe, sid_from("S-1-15-2-1-2-3-4-5-6-7"));
    // Without the layout the low-IL worker cannot load pdfium (the M0 symptom) ...
    let (ok, _, _) = pdf_probe(&exe, &dll, SandboxMode::LowIntegrity, &prof.0, false);
    assert!(
        !ok,
        "expected the M0 failure without the ALL APPLICATION PACKAGES ACE"
    );
    // ... with it, both modes work.
    let (ok, _, err) = pdf_probe(&exe, &dll, SandboxMode::LowIntegrity, &prof.0, true);
    assert!(ok, "low IL with layout: {err}");
    let (ok, mode, err) = pdf_probe(&exe, &dll, SandboxMode::AppContainer, &prof.0, true);
    assert!(
        ok && mode == Some(ActiveSandbox::AppContainer),
        "AC with layout: {err}"
    );
}
