//! sr-setup: the install/uninstall actions behind `crates/app/nsis/hooks.nsh` (M2-I). Never run by
//! a user, never shown: NSIS calls it with `nsExec`, so it has no window and prints plain lines
//! that land in the installer's details log.
//!
//! ```text
//! sr-setup install   --install-dir <dir>                 ACE + data dir + per-user task
//! sr-setup stop      --install-dir <dir> --mode <uninstall|update>
//! sr-setup uninstall --install-dir <dir> [--delete-data] task, \SR folder, our entries in
//!                    [--notice-file <f>] [--app-id <id>] Claude / Cursor / VS Code configs,
//!                                                        AppContainer profile, AI provider
//!                                                        keys and HTTP tool tokens, autostart
//!                                                        values (+ index; + the app's own
//!                                                        WebView2/UI folders with --app-id)
//! sr-setup status    [--install-dir <dir>]               what exists (verification, support)
//! ```
//!
//! Every action is idempotent, and a failed step never stops the remaining ones (exit 1 lists
//! them). Names and paths come from the same code the daemon and sr-launch use:
//! `sr_client::naming::{default_data_dir, task_name}`, `sr_client::launch::{register_task,
//! delete_task}`, `sr_extract_client::{ensure_app_packages_read, delete_app_container_profile,
//! APP_CONTAINER_NAME}`. Amendments 3.17, 4.5, 4.6.
mod autostart;
mod clients;
mod creds;
mod proc;
mod taskfolder;

/// The app's Connect writers, compiled verbatim (one implementation; see clients.rs). Only
/// detection and `jsonfile::disconnect` are used here.
#[allow(dead_code)]
#[path = "../../../crates/app/src/connect/mod.rs"]
mod connect;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use sr_client::{AppClient, AttachOptions, launch, naming, win};
use sr_common::protocol::ConnKind;

const USAGE: &str = "usage: sr-setup (install --install-dir <d> | stop --install-dir <d> --mode <uninstall|update> | uninstall --install-dir <d> [--delete-data] [--notice-file <f>] [--app-id <id>] | status [--install-dir <d>])";
const DAEMON_EXE: &str = "sr-daemon.exe";
/// Top-level Task Scheduler folder of `\SR\daemon-<sid8>` (sr_client::naming::task_name).
const TASK_FOLDER: &str = "SR";
/// Image names stopped hard when a drain did not end them (engine-side processes).
const ENGINE_EXES: &[&str] = &["sr-daemon.exe", "sr-extract.exe", "sr-launch.exe"];
/// Suffix of an in-use exe moved aside during an update (deleted by the next install/uninstall).
const OLD_SUFFIX: &str = ".sr-old";
/// Daemon drain target is ≤ 10 s (Amendment 4.25); allow headroom before a hard stop.
const DRAIN_WAIT: Duration = Duration::from_secs(20);

#[derive(Debug, PartialEq)]
enum Mode {
    Uninstall,
    Update,
}

#[derive(Debug, PartialEq)]
enum Cmd {
    Install(PathBuf),
    Stop(PathBuf, Mode),
    Uninstall {
        install_dir: PathBuf,
        delete_data: bool,
        /// R-UX43: where to write the plain-language summary of what the user must finish by
        /// hand (written only when there is something; the installer shows it, UTF-16LE).
        notice_file: Option<PathBuf>,
        /// tauri.conf.json identifier: also delete `%LOCALAPPDATA%\<id>` and `%APPDATA%\<id>`
        /// (the app's WebView2 cache and UI state, never the index). The MSI passes it; NSIS
        /// removes those folders itself.
        app_id: Option<String>,
    },
    Status(Option<PathBuf>),
}

fn parse(args: &[String]) -> Result<Cmd, String> {
    let mut install_dir = None;
    let mut mode = None;
    let mut delete_data = false;
    let mut notice_file = None;
    let mut app_id = None;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--install-dir" => install_dir = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
            "--mode" => {
                mode = Some(match it.next().map(String::as_str) {
                    Some("uninstall") => Mode::Uninstall,
                    Some("update") => Mode::Update,
                    _ => return Err(USAGE.into()),
                })
            }
            "--delete-data" => delete_data = true,
            "--notice-file" => notice_file = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
            "--app-id" => {
                let id = it.next().ok_or(USAGE)?;
                if !valid_app_id(id) {
                    return Err(format!("--app-id must be a plain reverse-DNS name: {id}"));
                }
                app_id = Some(id.clone());
            }
            _ => return Err(USAGE.into()),
        }
    }
    let need_dir = |d: Option<PathBuf>| -> Result<PathBuf, String> {
        let d = normalise_dir(&d.ok_or(USAGE)?);
        if d.is_absolute() {
            Ok(d)
        } else {
            Err(format!("--install-dir must be absolute: {}", d.display()))
        }
    };
    match args.first().map(String::as_str) {
        Some("install") => Ok(Cmd::Install(need_dir(install_dir)?)),
        Some("stop") => Ok(Cmd::Stop(need_dir(install_dir)?, mode.ok_or(USAGE)?)),
        Some("uninstall") => Ok(Cmd::Uninstall {
            install_dir: need_dir(install_dir)?,
            delete_data,
            notice_file,
            app_id,
        }),
        Some("status") => Ok(Cmd::Status(install_dir)),
        _ => Err(USAGE.into()),
    }
}

/// The MSI passes `"[INSTALLFOLDER]\"`, i.e. `C:\…\Selene Retrieval\` with a trailing separator
/// (the doubled backslash keeps the closing quote intact). Trailing separators and a trailing
/// `\.` are dropped so prefix checks (`proc::is_inside`) see the same folder as NSIS's `$INSTDIR`.
fn normalise_dir(d: &Path) -> PathBuf {
    let mut s = d.as_os_str().to_string_lossy().into_owned();
    loop {
        let before = s.len();
        if s.len() > 4 && (s.ends_with("\\.") || s.ends_with("/.")) {
            s.truncate(s.len() - 2);
        }
        // Keep a drive root like `C:\`.
        while s.len() > 3 && (s.ends_with('\\') || s.ends_with('/')) {
            s.pop();
        }
        if s.len() == before {
            return PathBuf::from(s);
        }
    }
}

/// tauri.conf.json `identifier` shape (used inside a path): letters, digits, `.`, `-`; no `..`.
fn valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && !id.starts_with('.')
        && !id.contains("..")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// Collects step failures; the exit code reports them all at the end.
#[derive(Default)]
struct Report {
    failed: Vec<String>,
}

impl Report {
    fn ok(&self, msg: &str) {
        println!("sr-setup: {msg}");
    }
    fn fail(&mut self, msg: String) {
        println!("sr-setup: FAILED {msg}");
        self.failed.push(msg);
    }
    fn exit(self) -> ExitCode {
        if self.failed.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        }
    }
}

fn data_dir() -> Result<PathBuf, String> {
    let d = naming::default_data_dir().map_err(|e| e.user_message().agent_text())?;
    naming::validate_data_dir(&d).map_err(|e| e.user_message().agent_text())?;
    Ok(d)
}

fn task_for(data_dir: &Path) -> Result<String, String> {
    let sid = win::user_sid_string().map_err(|e| format!("user SID: {e}"))?;
    Ok(naming::task_name(&sid, data_dir))
}

/// `schtasks /query /tn <name>` exit 0 (System32 schtasks by absolute path, no window).
fn task_exists(name: &str) -> Result<bool, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = win::system_dir()
        .map_err(|e| format!("System32: {e}"))?
        .join("schtasks.exe");
    let st = Command::new(exe)
        .args(["/query", "/tn", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("schtasks: {e}"))?;
    Ok(st.success())
}

/// `%LOCALAPPDATA%\Packages\<moniker>`: present while the AppContainer profile exists.
fn profile_dir() -> Option<PathBuf> {
    win::local_appdata().ok().map(|l| {
        l.join("Packages")
            .join(sr_extract_client::APP_CONTAINER_NAME.to_lowercase())
    })
}

/// Deletes `*.sr-old` files left by an update's move-aside (best effort; one may still run).
fn sweep_old(dir: &Path, r: &Report) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.to_string_lossy().ends_with(OLD_SUFFIX) {
            match std::fs::remove_file(&p) {
                Ok(()) => r.ok(&format!("removed {}", p.display())),
                Err(e) => r.ok(&format!("kept {} for now ({e})", p.display())),
            }
        }
    }
}

fn install(dir: &Path) -> ExitCode {
    let mut r = Report::default();
    if !dir.join(DAEMON_EXE).is_file() {
        r.fail(format!("{} not found in {}", DAEMON_EXE, dir.display()));
        return r.exit();
    }
    // Amendment 4.5 / m1-b §4: one inheritable ALL APPLICATION PACKAGES RX ACE on the install dir.
    // The daemon re-checks and self-heals it at every start, so a failure here is not fatal.
    match sr_extract_client::ensure_app_packages_read(dir) {
        Ok(true) => r.ok("install folder: reader access for the sandboxed file reader added"),
        Ok(false) => r.ok("install folder: reader access already present"),
        Err(e) => r.fail(format!("install folder ACE: {e}")),
    }
    sweep_old(dir, &r);
    // Per-user on-demand task `\SR\daemon-<sid8>` (Amendment 4.6), XML from sr_client::launch.
    // sr-launch re-registers it if it is ever missing, but packaged clients (MSIX Claude) can
    // only start the daemon through it, so it is registered here up front.
    match data_dir() {
        Ok(dd) => {
            if let Err(e) = std::fs::create_dir_all(&dd) {
                r.fail(format!("data folder {}: {e}", dd.display()));
            } else {
                match task_for(&dd) {
                    Ok(task) => match launch::register_task(&task, &dd, &dir.join(DAEMON_EXE)) {
                        Ok(()) => r.ok(&format!("background service task {task} registered")),
                        Err(e) => r.fail(format!("task {task}: {e}")),
                    },
                    Err(e) => r.fail(e),
                }
            }
        }
        Err(e) => r.fail(format!("data folder: {e}")),
    }
    r.exit()
}

/// Asks the daemon on the default data dir to drain and exit (connect-only: never launches one).
fn drain_daemon(r: &Report) -> Option<u32> {
    let dd = data_dir().ok()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let mut o = AttachOptions::new(ConnKind::App, "installer");
        o.data_dir = Some(dd);
        o.launch = false;
        o.drain_restart = false;
        o.timeout = Duration::from_secs(3);
        let c = match AppClient::connect(&o).await {
            Ok(c) => c,
            Err(_) => {
                r.ok("background service not running");
                return None;
            }
        };
        let pid = c.daemon_pid;
        match c.shutdown(true).await {
            Ok(()) => r.ok(&format!(
                "background service (pid {pid}) asked to finish and stop"
            )),
            // The daemon may close the pipe before replying; the wait below is what counts.
            Err(e) => r.ok(&format!(
                "shutdown request: {}",
                e.user_message().agent_text()
            )),
        }
        Some(pid)
    })
}

/// Moves an in-use exe aside so the update can write the new file (Windows allows renaming a
/// running image on the same volume). The old process keeps running from the renamed file.
fn move_aside(p: &Path) -> std::io::Result<PathBuf> {
    for n in 0..100u32 {
        let suffix = if n == 0 {
            OLD_SUFFIX.to_string()
        } else {
            format!(".{n}{OLD_SUFFIX}")
        };
        let mut s = p.as_os_str().to_owned();
        s.push(suffix);
        let dst = PathBuf::from(s);
        if !dst.exists() {
            std::fs::rename(p, &dst)?;
            return Ok(dst);
        }
    }
    Err(std::io::Error::other("too many old copies"))
}

fn stop(dir: &Path, mode: &Mode) -> ExitCode {
    let mut r = Report::default();
    if let Some(pid) = drain_daemon(&r) {
        if proc::wait_exit(pid, DRAIN_WAIT) {
            r.ok("background service stopped");
        } else {
            r.ok("background service did not stop in time; stopping it now");
        }
    }
    let procs = match proc::running_under(dir) {
        Ok(p) => p,
        Err(e) => {
            r.fail(format!("process list: {e}"));
            return r.exit();
        }
    };
    for p in procs {
        let name = p.file_name();
        let hard = *mode == Mode::Uninstall || ENGINE_EXES.contains(&name.as_str());
        if hard {
            match proc::terminate(p.pid, Duration::from_secs(10)) {
                Ok(true) => r.ok(&format!("stopped {name} (pid {})", p.pid)),
                Ok(false) => r.fail(format!("{name} (pid {}) did not exit", p.pid)),
                Err(e) => r.fail(format!("stop {name} (pid {}): {e}", p.pid)),
            }
        } else {
            // Update: an MCP client (Claude, VS Code…) still runs sr-bridge.exe. Keep it alive;
            // it reports "Restart Claude to finish." on its next request (version skew, 4.6).
            match move_aside(&p.image) {
                Ok(to) => r.ok(&format!(
                    "{name} in use (pid {}); moved to {}",
                    p.pid,
                    to.display()
                )),
                Err(e) if !p.image.exists() => r.ok(&format!("{name} already moved ({e})")),
                Err(e) => r.fail(format!(
                    "{name} in use (pid {}) and not movable: {e}",
                    p.pid
                )),
            }
        }
    }
    r.exit()
}

/// Removes the data dir (the index, catalog, settings and logs). Only the default data dir.
fn delete_data(dd: &Path, r: &mut Report) {
    if !dd.exists() {
        r.ok("search index folder already gone");
        return;
    }
    let started = Instant::now();
    loop {
        match std::fs::remove_dir_all(dd) {
            Ok(()) => {
                r.ok(&format!("search index folder {} deleted", dd.display()));
                return;
            }
            // A just-stopped daemon can hold a handle for a moment.
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => {
                r.fail(format!("delete {}: {e}", dd.display()));
                return;
            }
        }
    }
}

/// R-UX43: the summary the uninstaller shows when the user has to finish something by hand
/// (today: a client settings file we may not edit, e.g. a VS Code `mcp.json` with comments).
/// Plain words and one action; written as UTF-16LE without BOM (NSIS `FileReadUTF16LE`, the MSI
/// custom action). Nothing is written when there is nothing to do; a stale file is removed.
pub fn notice_text(manual: &[(String, PathBuf)]) -> Option<String> {
    if manual.is_empty() {
        return None;
    }
    let (lead, action) = if manual.len() == 1 {
        (
            "Selene Retrieval was removed, but one app's settings file still lists it. The file has \
             comments or couldn't be changed, so it was left exactly as it was:",
            "To finish, open the file and delete the \"selene-retrieval\" entry.",
        )
    } else {
        (
            "Selene Retrieval was removed, but these apps' settings files still list it. The files \
             have comments or couldn't be changed, so they were left exactly as they were:",
            "To finish, open each file and delete the \"selene-retrieval\" entry.",
        )
    };
    let mut s = format!("{lead}\r\n");
    for (app, p) in manual {
        s.push_str(&format!("\r\n{app}: {}", p.display()));
    }
    s.push_str(&format!("\r\n\r\n{action}"));
    Some(s)
}

fn write_notice(file: &Path, manual: &[(String, PathBuf)], r: &mut Report) {
    let _ = std::fs::remove_file(file);
    let Some(text) = notice_text(manual) else {
        return;
    };
    let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    match std::fs::write(file, bytes) {
        Ok(()) => r.ok(&format!("uninstall summary written to {}", file.display())),
        Err(e) => r.fail(format!("uninstall summary {}: {e}", file.display())),
    }
}

/// Deletes a folder tree we created (retries while a just-stopped process lets go of it).
fn remove_tree(dir: &Path, what: &str, r: &mut Report) {
    if !dir.exists() {
        return;
    }
    let started = Instant::now();
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => {
                r.ok(&format!("{what} {} deleted", dir.display()));
                return;
            }
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => {
                r.fail(format!("delete {}: {e}", dir.display()));
                return;
            }
        }
    }
}

fn uninstall(
    dir: &Path,
    delete: bool,
    notice_file: Option<&Path>,
    app_id: Option<&str>,
) -> ExitCode {
    let mut r = Report::default();
    match data_dir() {
        Ok(dd) => {
            match task_for(&dd) {
                Ok(task) => match task_exists(&task) {
                    Ok(false) => r.ok(&format!("task {task} already gone")),
                    Ok(true) => match launch::delete_task(&task) {
                        Ok(()) => r.ok(&format!("task {task} deleted")),
                        Err(e) => r.fail(format!("task {task}: {e}")),
                    },
                    Err(e) => r.fail(e),
                },
                Err(e) => r.fail(e),
            }
            // schtasks leaves the empty `\SR` folder behind; remove it unless still shared.
            match taskfolder::delete_if_empty(TASK_FOLDER) {
                Ok(taskfolder::Folder::Deleted) => r.ok(r"empty task folder \SR removed"),
                Ok(taskfolder::Folder::Absent) => {}
                Ok(taskfolder::Folder::InUse) => r.ok(r"task folder \SR still in use; kept"),
                Err(e) => r.fail(format!("task folder: {e}")),
            }
            if delete {
                delete_data(&dd, &mut r);
            } else {
                r.ok(&format!("search index kept in {}", dd.display()));
            }
        }
        Err(e) => r.fail(format!("data folder: {e}")),
    }
    // Our entry in MCP client configs (Claude Desktop Squirrel + MSIX, Cursor, VS Code + Insiders).
    // The bridge it points at is being removed, so the entry goes whether or not the index is kept.
    let mut manual: Vec<(String, PathBuf)> = Vec::new();
    let roots = clients::user_roots();
    match &roots {
        Some(roots) => {
            for l in clients::disconnect_all(roots) {
                if let Some(p) = l.manual {
                    // User-facing app name ("Msix Claude", "VS Code (Insiders)" are log labels).
                    let label = l.msg.split(':').next().unwrap_or_default();
                    let app = if label.contains("Claude") {
                        "Claude"
                    } else if label.starts_with("VS Code") {
                        "VS Code"
                    } else if label.starts_with("Cursor") {
                        "Cursor"
                    } else {
                        "An app"
                    }
                    .to_string();
                    manual.push((app, p));
                }
                if l.ok {
                    r.ok(&l.msg);
                } else {
                    r.fail(l.msg);
                }
            }
        }
        None => r.fail("user folders not found; client settings not checked".into()),
    }
    if let Some(f) = notice_file {
        write_notice(f, &manual, &mut r);
    }
    // Autostart values (tauri-plugin-autostart + Explorer's StartupApproved copy, R-UX25).
    for key in autostart::KEYS {
        match autostart::delete(key) {
            Ok(true) => r.ok(&format!("start-with-Windows entry removed ({key})")),
            Ok(false) => {}
            Err(e) => r.fail(format!("start-with-Windows entry {key}: {e}")),
        }
    }
    // The app's WebView2 cache and UI state (not the index). Only with --app-id (MSI).
    if let (Some(id), Some(roots)) = (app_id, &roots) {
        for base in [&roots.local, &roots.roaming] {
            remove_tree(&base.join(id), "app window cache", &mut r);
        }
    }
    // Amendment 3.17: the extractor's AppContainer profile (folder, registry mapping, SID).
    match sr_extract_client::delete_app_container_profile(sr_extract_client::APP_CONTAINER_NAME) {
        Ok(()) => r.ok("file reader sandbox profile deleted"),
        Err(e) => r.fail(format!("sandbox profile: {e}")),
    }
    // M3: AI provider keys, M4: HTTP tool tokens, M5: Claude (sign in) tokens. All live in
    // Credential Manager under our `SeleneRetrieval/` prefix and go whether or not the index is
    // kept (U22).
    match creds::delete_all() {
        Ok(0) => r.ok("no saved AI provider keys, sign-ins or tool passwords"),
        Ok(n) => r.ok(&format!(
            "{n} saved AI provider key(s), sign-in(s) and tool password(s) removed"
        )),
        Err(e) => r.fail(format!("saved keys: {e}")),
    }
    sweep_old(dir, &r);
    r.exit()
}

fn status(dir: Option<&Path>) -> ExitCode {
    let mut r = Report::default();
    match data_dir() {
        Ok(dd) => {
            println!("data_dir={}", dd.display());
            println!("data_dir_exists={}", dd.exists());
            match task_for(&dd) {
                Ok(t) => {
                    println!("task={t}");
                    match task_exists(&t) {
                        Ok(b) => println!("task_exists={b}"),
                        Err(e) => r.fail(e),
                    }
                }
                Err(e) => r.fail(e),
            }
        }
        Err(e) => r.fail(e),
    }
    if let Some(p) = profile_dir() {
        println!("profile_dir={}", p.display());
        println!("profile_exists={}", p.exists());
    }
    match creds::count() {
        Ok(n) => println!("provider_keys={n}"),
        Err(e) => r.fail(e),
    }
    if let Some(d) = dir {
        match proc::running_under(d) {
            Ok(ps) => {
                for p in ps {
                    println!("running={} pid={}", p.image.display(), p.pid);
                }
            }
            Err(e) => r.fail(format!("process list: {e}")),
        }
    }
    r.exit()
}

fn main() -> ExitCode {
    sr_client::harden_dll_search(); // S-08 (M5-E export): before anything can load a DLL by name
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args) {
        Ok(Cmd::Install(d)) => install(&d),
        Ok(Cmd::Stop(d, m)) => stop(&d, &m),
        Ok(Cmd::Uninstall {
            install_dir,
            delete_data,
            notice_file,
            app_id,
        }) => uninstall(
            &install_dir,
            delete_data,
            notice_file.as_deref(),
            app_id.as_deref(),
        ),
        Ok(Cmd::Status(d)) => status(d.as_deref()),
        Err(e) => {
            println!("{e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_every_command() {
        let d = r"C:\Users\u\AppData\Local\Selene Retrieval";
        assert_eq!(
            parse(&a(&["install", "--install-dir", d])),
            Ok(Cmd::Install(d.into()))
        );
        assert_eq!(
            parse(&a(&["stop", "--install-dir", d, "--mode", "update"])),
            Ok(Cmd::Stop(d.into(), Mode::Update))
        );
        assert_eq!(
            parse(&a(&["uninstall", "--install-dir", d, "--delete-data"])),
            Ok(Cmd::Uninstall {
                install_dir: d.into(),
                delete_data: true,
                notice_file: None,
                app_id: None,
            })
        );
        let with_sep = format!("{d}\\");
        assert_eq!(
            parse(&a(&[
                "uninstall",
                "--install-dir",
                &with_sep,
                "--notice-file",
                r"C:\t\n.txt",
                "--app-id",
                "ai.selene.retrieval"
            ])),
            Ok(Cmd::Uninstall {
                install_dir: d.into(),
                delete_data: false,
                notice_file: Some(r"C:\t\n.txt".into()),
                app_id: Some("ai.selene.retrieval".into()),
            }),
            "MSI form: trailing separator dropped"
        );
        assert_eq!(parse(&a(&["status"])), Ok(Cmd::Status(None)));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse(&a(&[])).is_err());
        assert!(parse(&a(&["install"])).is_err(), "install dir required");
        assert!(parse(&a(&["install", "--install-dir", "rel\\dir"])).is_err());
        assert!(
            parse(&a(&["stop", "--install-dir", r"C:\x"])).is_err(),
            "mode required"
        );
        assert!(parse(&a(&["stop", "--install-dir", r"C:\x", "--mode", "x"])).is_err());
        assert!(parse(&a(&["uninstall", "--install-dir", r"C:\x", "--bogus"])).is_err());
        for bad in ["..", "../x", r"a\b", "a/b", ".hidden", "", "a b"] {
            assert!(
                parse(&a(&[
                    "uninstall",
                    "--install-dir",
                    r"C:\x",
                    "--app-id",
                    bad
                ]))
                .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn install_dir_forms_are_normalised() {
        let want = r"C:\Users\u\AppData\Local\Selene Retrieval";
        for raw in [
            r"C:\Users\u\AppData\Local\Selene Retrieval\",
            r"C:\Users\u\AppData\Local\Selene Retrieval\.",
            r"C:\Users\u\AppData\Local\Selene Retrieval\\",
            r"C:\Users\u\AppData\Local\Selene Retrieval",
        ] {
            assert_eq!(normalise_dir(Path::new(raw)), PathBuf::from(want), "{raw}");
        }
        assert_eq!(normalise_dir(Path::new(r"C:\")), PathBuf::from(r"C:\"));
    }

    #[test]
    fn notice_names_the_file_and_one_action() {
        assert_eq!(notice_text(&[]), None);
        let p = PathBuf::from(r"C:\Users\u\AppData\Roaming\Code\User\mcp.json");
        let one = notice_text(&[("VS Code".into(), p.clone())]).unwrap_or_default();
        assert!(
            one.contains(r"VS Code: C:\Users\u\AppData\Roaming\Code\User\mcp.json"),
            "{one}"
        );
        assert!(
            one.contains("delete the \"selene-retrieval\" entry"),
            "{one}"
        );
        assert!(!one.contains("first start"), "{one}");
        let two =
            notice_text(&[("VS Code".into(), p.clone()), ("Cursor".into(), p)]).unwrap_or_default();
        assert!(two.contains("open each file"), "{two}");
    }

    #[test]
    fn move_aside_picks_a_free_name() {
        let d = std::env::temp_dir().join(format!("sr-setup-aside-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap_or_else(|e| panic!("{e}"));
        let f = d.join("sr-bridge.exe");
        std::fs::write(&f, b"a").unwrap_or_else(|e| panic!("{e}"));
        let first = move_aside(&f).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&f, b"b").unwrap_or_else(|e| panic!("{e}"));
        let second = move_aside(&f).unwrap_or_else(|e| panic!("{e}"));
        assert!(first.to_string_lossy().ends_with("sr-bridge.exe.sr-old"));
        assert!(second.to_string_lossy().ends_with("sr-bridge.exe.1.sr-old"));
        let r = Report::default();
        sweep_old(&d, &r);
        let left: Vec<_> = std::fs::read_dir(&d)
            .map(|rd| rd.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        let _ = std::fs::remove_dir_all(&d);
        assert!(left.is_empty(), "{left:?}");
    }
}
