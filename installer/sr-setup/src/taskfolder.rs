//! The `\SR` Task Scheduler folder. `schtasks /delete` removes the task but leaves its (now
//! empty) folder behind, and schtasks cannot delete folders; the Task Scheduler COM API can, with
//! no admin rights (verified on Windows 11 26200, docs/research/m2-i-installer.md §5).
//!
//! The folder is machine-wide and shared by every user's `\SR\daemon-<sid8>` task, so it is
//! deleted only when it holds no task and no subfolder that we can see, and `DeleteFolder` itself
//! refuses a folder that still holds another user's task. Then we leave it alone.
#![allow(unsafe_code)]

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::TaskScheduler::{ITaskService, TASK_ENUM_HIDDEN, TaskScheduler};
use windows::Win32::System::Variant::VARIANT;
use windows::core::BSTR;

/// What happened to the folder.
#[derive(Debug, PartialEq, Eq)]
pub enum Folder {
    Deleted,
    Absent,
    /// Still holds a task or subfolder (ours or another user's); left in place.
    InUse,
}

struct Com;

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: paired with the successful CoInitializeEx below on this thread.
        unsafe { CoUninitialize() };
    }
}

/// Deletes the top-level task folder `name` (no leading backslash) if it is empty.
pub fn delete_if_empty(name: &str) -> Result<Folder, String> {
    // SAFETY: COM init on this thread; uninitialised by `Com` on every return path.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    hr.ok().map_err(|e| format!("COM init: {e}"))?;
    let _com = Com;
    // SAFETY: standard in-proc activation of the Task Scheduler service object.
    let svc: ITaskService = unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| format!("Task Scheduler: {e}"))?;
    let none = VARIANT::default();
    // SAFETY: valid interface; empty VARIANTs = local machine, current user.
    unsafe { svc.Connect(&none, &none, &none, &none) }
        .map_err(|e| format!("Task Scheduler connect: {e}"))?;
    // SAFETY: valid interface and BSTR arguments for every call below.
    let folder = match unsafe { svc.GetFolder(&BSTR::from(format!("\\{name}"))) } {
        Ok(f) => f,
        Err(_) => return Ok(Folder::Absent),
    };
    let tasks = unsafe { folder.GetTasks(TASK_ENUM_HIDDEN.0) }
        .and_then(|c| unsafe { c.Count() })
        .map_err(|e| format!("list tasks: {e}"))?;
    let subs = unsafe { folder.GetFolders(0) }
        .and_then(|c| unsafe { c.Count() })
        .map_err(|e| format!("list folders: {e}"))?;
    if tasks > 0 || subs > 0 {
        return Ok(Folder::InUse);
    }
    let root =
        unsafe { svc.GetFolder(&BSTR::from("\\")) }.map_err(|e| format!("root folder: {e}"))?;
    match unsafe { root.DeleteFolder(&BSTR::from(name), 0) } {
        Ok(()) => Ok(Folder::Deleted),
        // Another user's task we cannot see, or no right on the folder: theirs to keep.
        Err(_) => Ok(Folder::InUse),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn schtasks(args: &[&str]) -> bool {
        let sys = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        Command::new(
            std::path::Path::new(&sys)
                .join("System32")
                .join("schtasks.exe"),
        )
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn folder_is_kept_while_used_and_removed_when_empty() {
        let folder = format!("SR-M2I-test-{}", std::process::id());
        let task = format!(r"\{folder}\probe");
        let sid = sr_client::win::user_sid_string().unwrap_or_else(|e| panic!("{e}"));
        // Disabled, never-running task (same XML generator the installer uses).
        let mut xml = sr_client::launch::task_xml(
            &sid,
            std::path::Path::new(r"C:\Windows\System32\cmd.exe"),
            "/c exit 0",
        );
        xml = xml.replace("<Enabled>true</Enabled>", "<Enabled>false</Enabled>");
        let path = std::env::temp_dir().join(format!("{folder}.xml"));
        let mut bytes = vec![0xFF, 0xFE];
        for u in xml.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("{e}"));
        let p = path.to_string_lossy().into_owned();
        assert!(schtasks(&["/create", "/tn", &task, "/xml", &p, "/f"]));
        let _ = std::fs::remove_file(&path);

        let in_use = delete_if_empty(&folder);
        let deleted_task = schtasks(&["/delete", "/tn", &task, "/f"]);
        let emptied = delete_if_empty(&folder);
        let again = delete_if_empty(&folder);
        assert_eq!(in_use, Ok(Folder::InUse));
        assert!(deleted_task);
        assert_eq!(emptied, Ok(Folder::Deleted));
        assert_eq!(again, Ok(Folder::Absent));
    }
}
