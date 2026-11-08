//! Win32 helpers shared by sr-daemon and sr-harness (spike only).
#![allow(dead_code)]

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    IsProcessInJob, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    QueryInformationJobObject,
};

pub const JOB_FLAGS: &[(u32, &str)] = &[
    (0x0000_0008, "ACTIVE_PROCESS"),
    (0x0000_0100, "PROCESS_MEMORY"),
    (0x0000_0200, "JOB_MEMORY"),
    (0x0000_0400, "DIE_ON_UNHANDLED_EXCEPTION"),
    (0x0000_0800, "BREAKAWAY_OK"),
    (0x0000_1000, "SILENT_BREAKAWAY_OK"),
    (0x0000_2000, "KILL_ON_JOB_CLOSE"),
];

pub fn job_flag_names(f: u32) -> String {
    let v: Vec<&str> = JOB_FLAGS.iter().filter(|(b, _)| f & b != 0).map(|(_, n)| *n).collect();
    if v.is_empty() { format!("0x{f:x} (none)") } else { format!("0x{f:x} {}", v.join("|")) }
}

/// IsProcessInJob(h, NULL): in *any* job?
pub fn in_any_job(h: HANDLE) -> Option<bool> {
    let mut r = 0;
    unsafe { if IsProcessInJob(h, std::ptr::null_mut(), &mut r) == 0 { None } else { Some(r != 0) } }
}

pub fn in_job(h: HANDLE, job: HANDLE) -> Option<bool> {
    let mut r = 0;
    unsafe { if IsProcessInJob(h, job, &mut r) == 0 { None } else { Some(r != 0) } }
}

/// LimitFlags of `job` (NULL = the job of the calling process).
pub fn job_limit_flags(job: HANDLE) -> Option<u32> {
    unsafe {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        let ok = QueryInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        );
        if ok == 0 { None } else { Some(info.BasicLimitInformation.LimitFlags) }
    }
}

/// Some(full name) when running with package identity (MSIX), None otherwise.
pub fn package_full_name() -> Option<String> {
    unsafe {
        let mut len = 0u32;
        let rc = GetCurrentPackageFullName(&mut len, std::ptr::null_mut());
        if rc == 15700 {
            return None; // APPMODEL_ERROR_NO_PACKAGE
        }
        let mut buf = vec![0u16; len as usize + 1];
        if GetCurrentPackageFullName(&mut len, buf.as_mut_ptr()) != 0 {
            return Some(format!("<error {rc}>"));
        }
        Some(String::from_utf16_lossy(&buf[..len.saturating_sub(1) as usize]))
    }
}

/// Builds a self-relative SD from SDDL. The returned pointer is leaked (process lifetime).
pub fn sd_from_sddl(sddl: &str) -> std::io::Result<PSECURITY_DESCRIPTOR> {
    let w: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(sd)
}

pub fn security_attributes(sd: PSECURITY_DESCRIPTOR) -> Box<SECURITY_ATTRIBUTES> {
    Box::new(SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    })
}

/// Effective owner + DACL of a kernel object handle, as SDDL (GetSecurityInfo).
pub fn handle_sddl(h: HANDLE) -> std::io::Result<String> {
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let rc = GetSecurityInfo(
            h,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | 0x10 /* LABEL */,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        let mut s: *mut u16 = std::ptr::null_mut();
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | 0x10 /* LABEL */,
            &mut s,
            std::ptr::null_mut(),
        );
        LocalFree(sd as *mut c_void);
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut n = 0;
        while *s.add(n) != 0 {
            n += 1;
        }
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
        LocalFree(s.cast());
        Ok(out)
    }
}

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
}

pub fn processes() -> Vec<Proc> {
    let mut v = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return v;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut e) != 0 {
            loop {
                let n = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
                v.push(Proc { pid: e.th32ProcessID, ppid: e.th32ParentProcessID, name: String::from_utf16_lossy(&e.szExeFile[..n]) });
                if Process32NextW(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    v
}

pub fn count_named(name: &str) -> Vec<u32> {
    processes().into_iter().filter(|p| p.name.eq_ignore_ascii_case(name)).map(|p| p.pid).collect()
}

pub fn parent_of(pid: u32) -> Option<Proc> {
    let ps = processes();
    let me = ps.iter().find(|p| p.pid == pid)?;
    ps.iter().find(|p| p.pid == me.ppid).cloned().or(Some(Proc { pid: me.ppid, ppid: 0, name: "<exited>".into() }))
}
