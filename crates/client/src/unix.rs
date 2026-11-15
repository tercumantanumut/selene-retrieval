//! Unix (macOS first) counterpart of `win`: the socket transport, locks and process queries used
//! by the clients, the launcher and the daemon's socket server.
//!
//! The functions keep the names of their `win` twins, so callers go through [`crate::sys`]. This
//! is the only module of sr-client on Unix that may use `unsafe`: every call is one libc function
//! with a caller-owned, correctly sized buffer.
//!
//! Transport security (replaces the pipe DACL, `FIRST_PIPE_INSTANCE` and S-01). Every check fails
//! closed:
//! * The socket lives in a private folder: `<per-user temp dir>/sr` on macOS (the temp dir comes
//!   from `confstr(_CS_DARWIN_USER_TEMP_DIR)`, never `$TMPDIR`), `/tmp/sr-<uid>` elsewhere. The
//!   folder is created mode 0700 and must be a real folder (`lstat`, never a link) owned by
//!   `getuid()` with no group or other bits and (macOS) no extended ACL, on every bind and every
//!   connect. A folder of ours that fails this is made private again ([`make_private_dir`]).
//! * The socket name is `sr-<hash(uid)>-<hash(data dir key)>.sock` ([`crate::naming::pipe_name`]),
//!   so the path is short enough for `sun_path` and never contains the data dir itself.
//! * Server ([`SocketListener`]): binds only under the host lock, sets mode 0600 and verifies it.
//!   A stale socket of ours is replaced; a live one is `AddrInUse`; anything else is refused.
//!   Every accepted peer must run as `getuid()` (`getpeereid`), else it is closed unread.
//! * Client ([`connect`], [`pipe_server_identity`]): before the first byte is sent, the folder and
//!   the socket file must be ours and private, and the peer must run as `getuid()`.
//! * macOS caller trust (R50, replaces the pid → image check): [`PeerToken`] (`LOCAL_PEERTOKEN`)
//!   read around a nonce, see "Peer audit token" below.

use std::ffi::{CStr, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tokio::net::{UnixListener, UnixStream};

/// Client end of a daemon connection (a named pipe in `win`).
pub type Stream = UnixStream;
/// Server end of one daemon session.
pub type ServerStream = UnixStream;

/// Longest socket path: `sun_path` is 104 bytes on macOS (108 on Linux), minus the NUL.
const MAX_SOCKET_PATH: usize = 103;

/// Real user id of this process.
pub fn uid() -> u32 {
    // SAFETY: no arguments; getuid cannot fail.
    unsafe { libc::getuid() }
}

/// Unix stand-in for the Windows user SID: the decimal user id. It feeds the socket name and the
/// server-identity check, exactly where `win` uses the SID.
pub fn user_sid_string() -> io::Result<String> {
    Ok(uid().to_string())
}

/// Amendment 13: whether this process runs as root (the Unix form of "Run as administrator").
pub fn process_elevated() -> io::Result<bool> {
    // SAFETY: no arguments; geteuid cannot fail.
    Ok(unsafe { libc::geteuid() } == 0)
}

/// M5-E S-08 has no Unix twin: dyld does not search the current directory for the libraries a
/// binary links. Kept so every `main` stays the same. Always true.
/// macOS phase 2: hardened runtime and library validation come with code signing.
pub fn harden_dll_search() -> bool {
    true
}

/// Home folder of the current user from the user database (`getpwuid_r`), never `$HOME`
/// (MCP clients pass a filtered environment).
fn home_dir() -> io::Result<PathBuf> {
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: an all-zero `passwd` is a valid out value (null pointers, zero ids).
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut res: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `pw`, `buf` and `res` outlive the call and `buf.len()` is the buffer size; the
        // strings in `pw` point into `buf` and are read before `buf` changes.
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut pw,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut res,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        if res.is_null() || pw.pw_dir.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "this account has no home folder",
            ));
        }
        // SAFETY: on success `pw_dir` is a NUL-terminated string inside `buf`.
        let dir = unsafe { CStr::from_ptr(pw.pw_dir) }.to_bytes().to_vec();
        let dir = PathBuf::from(OsString::from_vec(dir));
        if !dir.is_absolute() {
            return Err(io::Error::other("home folder is not an absolute path"));
        }
        return Ok(dir);
    }
}

/// Unix twin of `%LOCALAPPDATA%`: `~/Library/Application Support` on macOS,
/// `~/.local/share` elsewhere. The home folder comes from the user database, never `$HOME`.
pub fn local_appdata() -> io::Result<PathBuf> {
    let home = home_dir()?;
    if cfg!(target_os = "macos") {
        Ok(home.join("Library").join("Application Support"))
    } else {
        Ok(home.join(".local").join("share"))
    }
}

/// Folder of the trusted system tools: `/usr/bin` (read-only system volume on macOS). The Unix
/// twin of `GetSystemDirectoryW`; callers join a tool name and never search `PATH`.
pub fn system_dir() -> io::Result<PathBuf> {
    Ok(PathBuf::from("/usr/bin"))
}

/// Opens (creating, mode 0600, never through a symlink) `path` and takes an exclusive `flock`
/// (std `File::lock`). `wait=false` returns `Ok(None)` when another open file holds it.
/// The lock lives as long as the returned `File`.
pub fn lock_file(path: &Path, wait: bool) -> io::Result<Option<File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if wait {
        f.lock()?;
        return Ok(Some(f));
    }
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// No package identity on Unix (MSIX only).
pub fn package_full_name() -> Option<String> {
    None
}

/// No Job Objects on Unix.
pub fn current_process_in_job() -> Option<bool> {
    None
}

/// Whether process `pid` is still running (`kill(pid, 0)`; `EPERM` means it exists).
pub fn pid_alive(pid: u32) -> bool {
    // pid 0 and negative values address process groups: never.
    let Some(p) = libc::pid_t::try_from(pid).ok().filter(|p| *p > 0) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists and may be signalled.
    if unsafe { libc::kill(p, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Full image path of process `pid` (`proc_pidpath` on macOS, `/proc/<pid>/exe` on Linux),
/// `None` when the process is gone or can't be queried. R31: the daemon's caller check.
pub fn process_image_path(pid: u32) -> Option<PathBuf> {
    let p = libc::pid_t::try_from(pid).ok().filter(|p| *p > 0)?;
    image_path(p)
}

#[cfg(target_os = "macos")]
fn image_path(pid: libc::pid_t) -> Option<PathBuf> {
    let mut buf = vec![0u8; usize::try_from(libc::PROC_PIDPATHINFO_MAXSIZE).ok()?];
    let cap = u32::try_from(buf.len()).ok()?;
    // SAFETY: `buf` holds `cap` writable bytes and outlives the call; the call writes at most
    // `cap` bytes and returns the length written.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), cap) };
    let n = usize::try_from(n)
        .ok()
        .filter(|n| *n > 0 && *n <= buf.len())?;
    buf.truncate(n);
    Some(PathBuf::from(OsString::from_vec(buf)))
}

#[cfg(target_os = "linux")]
fn image_path(pid: libc::pid_t) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn image_path(_: libc::pid_t) -> Option<PathBuf> {
    None
}

/// Most descriptors [`close_inherited_fds`] looks at, whatever the soft limit says.
const MAX_INHERITED_FD: libc::rlim_t = 65_536;

/// One past the highest descriptor a child may have inherited: the soft `RLIMIT_NOFILE`, capped
/// at [`MAX_INHERITED_FD`] (also when the limit is unlimited or can't be read).
fn fd_limit() -> libc::c_int {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rl` is a valid out value and outlives the call.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
        rl.rlim_cur.min(MAX_INHERITED_FD)
    } else {
        MAX_INHERITED_FD
    };
    libc::c_int::try_from(soft).unwrap_or(libc::c_int::MAX)
}

/// The child of `cmd` closes, right before exec, every descriptor above stderr that is not
/// close-on-exec. std and tokio open their own descriptors close-on-exec, but ones this process
/// inherited without it (from the MCP client) would pass on to sr-launch and the daemon and stay
/// open for the daemon's lifetime, so the client might never see EOF. The Unix form of
/// `bInheritHandles = FALSE` in `win`. Close-on-exec descriptors are left alone: they close at
/// exec anyway, and one of them is std's pipe that reports a failed exec.
pub fn close_inherited_fds(cmd: &mut Command) {
    // Read before fork: getrlimit in the child is not needed and the value can't change.
    let limit = fd_limit();
    // SAFETY: the hook runs in the child between fork and exec, allocates nothing and calls only
    // fcntl and close, which are async-signal-safe. A descriptor that isn't open fails F_GETFD
    // (EBADF) and is skipped.
    unsafe {
        cmd.pre_exec(move || {
            for fd in 3..limit {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags != -1 && flags & libc::FD_CLOEXEC == 0 {
                    libc::close(fd);
                }
            }
            Ok(())
        });
    }
}

/// Starts `exe args` as a detached daemon and returns its pid: its own session (`setsid`, so a
/// terminal hang-up or a Ctrl+C sent to the client's group never reaches it), stdio on
/// `/dev/null`, and no other inherited descriptors ([`close_inherited_fds`]).
/// Not waited: the launcher exits at once and launchd adopts the child. This is the whole macOS
/// launch path; why no LaunchAgent is needed is in [`crate::launch`].
#[allow(clippy::zombie_processes)] // see above: the launcher exits right after
pub fn spawn_detached(exe: &Path, args: &[&OsStr], cwd: Option<&Path>) -> io::Result<u32> {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    // SAFETY: the hook runs in the child between fork and exec and calls only setsid, which is
    // async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    close_inherited_fds(&mut cmd);
    let child = cmd.spawn()?;
    Ok(child.id())
}

// ------------------------------------------------------------------------------------------------
// Socket folder and files

fn not_private(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} {why}", path.display()),
    )
}

/// The per-user temp dir of macOS (`confstr(_CS_DARWIN_USER_TEMP_DIR)`, e.g.
/// `/var/folders/xx/…/T/`). Not `$TMPDIR`: the environment is the caller's.
#[cfg(target_os = "macos")]
fn darwin_user_temp_dir() -> io::Result<PathBuf> {
    // SAFETY: size query: a null buffer of length 0 only returns the size needed (with the NUL).
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
    if n == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u8; n];
    // SAFETY: `buf` holds `n` writable bytes and outlives the call.
    let m = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), n) };
    if m == 0 || m > n {
        return Err(io::Error::other("per-user temp folder is unavailable"));
    }
    buf.truncate(m - 1);
    let dir = PathBuf::from(OsString::from_vec(buf));
    if !dir.is_absolute() {
        return Err(io::Error::other(
            "per-user temp folder is not an absolute path",
        ));
    }
    Ok(dir)
}

/// Where the socket folder lives: `<per-user temp dir>/sr` on macOS (the temp dir itself must
/// be ours and not writable by others), `/tmp/sr-<uid>` elsewhere.
fn socket_dir_path() -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let base = darwin_user_temp_dir()?;
        let m = std::fs::symlink_metadata(&base)?;
        if !m.file_type().is_dir() || m.uid() != uid() || m.mode() & 0o022 != 0 {
            return Err(not_private(
                &base,
                "is not this account's private temp folder",
            ));
        }
        // An allow entry could let others replace our folder (deny-only entries are harmless).
        match extended_acl_entries(&base) {
            Ok((_, 0)) => {}
            _ => {
                return Err(not_private(
                    &base,
                    "is not this account's private temp folder (access list)",
                ));
            }
        }
        Ok(base.join("sr"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(PathBuf::from(format!("/tmp/sr-{}", uid())))
    }
}

/// macOS `<sys/acl.h>` (not in the libc crate). The enums are C `int`s.
#[cfg(target_os = "macos")]
mod acl {
    use std::ffi::{c_char, c_int, c_void};

    pub const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
    pub const ACL_FIRST_ENTRY: c_int = 0;
    pub const ACL_NEXT_ENTRY: c_int = -1;
    pub const ACL_EXTENDED_ALLOW: c_int = 1;

    unsafe extern "C" {
        pub fn acl_init(count: c_int) -> *mut c_void;
        pub fn acl_free(obj: *mut c_void) -> c_int;
        pub fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        pub fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
        pub fn acl_get_link_np(path: *const c_char, ty: c_int) -> *mut c_void;
        pub fn acl_set_fd_np(fd: c_int, acl: *mut c_void, ty: c_int) -> c_int;
    }
}

/// macOS: the entries in the extended ACL of `path` itself (a link is read, never followed), as
/// `(all, allow)`. `(0, 0)` when it has none or its file system keeps no ACLs. An entry whose
/// kind can't be read counts as an allow entry.
#[cfg(target_os = "macos")]
pub fn extended_acl_entries(path: &Path) -> io::Result<(usize, usize)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: `c` is NUL-terminated and outlives the call; a non-null result is freed below.
    let list = unsafe { acl::acl_get_link_np(c.as_ptr(), acl::ACL_TYPE_EXTENDED) };
    if list.is_null() {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            // ENOENT: no ACL on it (callers `lstat` first). ENOTSUP: none possible here.
            Some(libc::ENOENT | libc::ENOTSUP) => Ok((0, 0)),
            _ => Err(e),
        };
    }
    let (mut all, mut allow) = (0usize, 0usize);
    let mut id = acl::ACL_FIRST_ENTRY;
    loop {
        let mut entry = std::ptr::null_mut();
        // SAFETY: `list` is a live ACL and `entry` a valid out pointer. 0 = an entry, -1 = no more.
        if unsafe { acl::acl_get_entry(list, id, &mut entry) } != 0 {
            break;
        }
        id = acl::ACL_NEXT_ENTRY;
        all += 1;
        let mut tag = 0;
        // SAFETY: `entry` points into `list`, which is still live; `tag` is a valid out value.
        if unsafe { acl::acl_get_tag_type(entry, &mut tag) } != 0 || tag == acl::ACL_EXTENDED_ALLOW
        {
            allow += 1;
        }
    }
    // SAFETY: `list` came from acl_get_link_np and is freed once.
    unsafe { acl::acl_free(list) };
    Ok((all, allow))
}

/// macOS: removes every entry of the extended ACL of the open folder `f` (`chmod -N` through the
/// handle). A file system without ACLs has nothing to remove.
#[cfg(target_os = "macos")]
fn strip_extended_acl(f: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: allocates an empty ACL, freed below.
    let empty = unsafe { acl::acl_init(0) };
    if empty.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `f` is open and `empty` is a live ACL for the duration of the call.
    let rc = unsafe { acl::acl_set_fd_np(f.as_raw_fd(), empty, acl::ACL_TYPE_EXTENDED) };
    let e = io::Error::last_os_error();
    // SAFETY: `empty` came from acl_init and is freed once.
    unsafe { acl::acl_free(empty) };
    match rc {
        0 => Ok(()),
        _ if e.raw_os_error() == Some(libc::ENOTSUP) => Ok(()),
        _ => Err(e),
    }
}

/// Checks (with `lstat`, so a link is judged itself) that `dir` is a real folder owned by `uid`
/// with no group or other bits and, on macOS, no extended ACL (an inherited `everyone allow`
/// entry lets others in whatever the mode says). `NotFound` passes through; every other failure
/// is `PermissionDenied`.
pub fn verify_private_dir(dir: &Path, uid: u32) -> io::Result<()> {
    let m = std::fs::symlink_metadata(dir)?;
    if !m.file_type().is_dir() {
        return Err(not_private(dir, "is not a plain folder"));
    }
    if m.uid() != uid {
        return Err(not_private(dir, "belongs to another account"));
    }
    if m.mode() & 0o077 != 0 {
        return Err(not_private(dir, "is open to other accounts"));
    }
    #[cfg(target_os = "macos")]
    match extended_acl_entries(dir) {
        Ok((0, _)) => {}
        Ok(_) => return Err(not_private(dir, "has an access list (ACL)")),
        Err(e) => return Err(not_private(dir, &format!("access list unreadable: {e}"))),
    }
    Ok(())
}

/// Makes `dir`, a real folder owned by `uid`, private: mode 0700 and, on macOS, no extended ACL.
/// Works through a handle opened with `O_NOFOLLOW` (a link is refused, never followed; no
/// path-based chmod), then checks the result with [`verify_private_dir`]. Idempotent.
/// `NotFound` passes through; every other failure is `PermissionDenied`.
pub fn make_private_dir(dir: &Path, uid: u32) -> io::Result<()> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => not_private(dir, "is not a plain folder"),
            _ if e.kind() == io::ErrorKind::NotFound => e,
            _ => not_private(dir, &format!("could not be opened: {e}")),
        })?;
    let m = f
        .metadata()
        .map_err(|e| not_private(dir, &format!("could not be read: {e}")))?;
    if !m.is_dir() {
        return Err(not_private(dir, "is not a plain folder"));
    }
    if m.uid() != uid {
        return Err(not_private(dir, "belongs to another account"));
    }
    if m.mode() & 0o777 != 0o700 {
        f.set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|e| not_private(dir, &format!("mode could not be set: {e}")))?;
    }
    #[cfg(target_os = "macos")]
    strip_extended_acl(&f)
        .map_err(|e| not_private(dir, &format!("access list could not be removed: {e}")))?;
    drop(f);
    verify_private_dir(dir, uid)
}

/// Checks (with `lstat`) that `path` is a socket owned by `uid` with no group or other bits.
/// `NotFound` passes through; every other failure is `PermissionDenied`.
pub fn verify_private_socket(path: &Path, uid: u32) -> io::Result<()> {
    let m = std::fs::symlink_metadata(path)?;
    if !m.file_type().is_socket() {
        return Err(not_private(path, "is not a socket"));
    }
    if m.uid() != uid {
        return Err(not_private(path, "belongs to another account"));
    }
    if m.mode() & 0o077 != 0 {
        return Err(not_private(path, "is open to other accounts"));
    }
    Ok(())
}

/// The verified socket folder. With `create`, a missing folder is made (mode 0700) first.
/// Without it, a missing folder is `NotFound` (no daemon has run yet). A folder of ours that is
/// open to others (mode bits, or an ACL inherited at creation) is made private again; anything
/// else that fails the check is refused.
pub fn socket_dir(create: bool) -> io::Result<PathBuf> {
    let dir = socket_dir_path()?;
    if create {
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    match verify_private_dir(&dir, uid()) {
        Ok(()) => {}
        // make_private_dir refuses links, non-folders and other owners itself.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => make_private_dir(&dir, uid())?,
        Err(e) => return Err(e),
    }
    Ok(dir)
}

/// `dir/name`, refused when `name` is not one plain file name or the path does not fit
/// `sun_path`.
pub fn socket_path(dir: &Path, name: &str) -> io::Result<PathBuf> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad socket name",
        ));
    }
    let p = dir.join(name);
    if p.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("socket path too long: {}", p.display()),
        ));
    }
    Ok(p)
}

// ------------------------------------------------------------------------------------------------
// Client

/// Connects to the daemon's socket `name` (from [`crate::naming::pipe_name`]). The folder and
/// the socket file are checked first (ours, private, not links). `NotFound` / `ConnectionRefused`
/// mean no daemon is serving ([`is_absent`]); `PermissionDenied` means the checks failed.
pub async fn connect(name: &str) -> io::Result<UnixStream> {
    let me = uid();
    let dir = socket_dir(false)?;
    let path = socket_path(&dir, name)?;
    verify_private_socket(&path, me)?;
    UnixStream::connect(&path).await
}

/// No daemon is serving: no folder or socket file yet, or a stale socket nobody listens on.
pub fn is_absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// The daemon is serving but its accept queue is full for a moment (`EAGAIN`).
pub fn is_busy(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WouldBlock
}

/// Why a socket server was refused (diagnostics only; the user sees one plain sentence). Same
/// shape as `win::ServerIdentity`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerIdentity {
    /// The server process runs as the expected user.
    Ours,
    /// The socket's owner is someone else (normally caught by [`connect`] already).
    ForeignOwner(String),
    /// The server process runs as someone else.
    ForeignProcess(String),
    /// The peer credentials could not be read. Fail closed.
    Unverifiable(String),
}

/// The pure decision of [`pipe_server_identity`]: the peer's uid must be `expected`.
pub fn decide_server_identity(expected: &str, peer_uid: u32) -> ServerIdentity {
    if peer_uid.to_string() == expected {
        ServerIdentity::Ours
    } else {
        ServerIdentity::ForeignProcess(format!("uid {peer_uid}"))
    }
}

/// S-01 on Unix: who serves this connection? The kernel's peer credentials (`getpeereid`) must
/// show `expected_sid` (the uid from [`user_sid_string`]); [`connect`] already checked the folder
/// and the socket file. Call before any byte is sent.
pub fn pipe_server_identity(stream: &UnixStream, expected_sid: &str) -> ServerIdentity {
    match stream.peer_cred() {
        Ok(c) => decide_server_identity(expected_sid, c.uid()),
        Err(e) => ServerIdentity::Unverifiable(format!("peer credentials: {e}")),
    }
}

/// [`pipe_server_identity`] against the current user.
pub fn pipe_server_is_ours(stream: &UnixStream) -> io::Result<ServerIdentity> {
    Ok(pipe_server_identity(stream, &user_sid_string()?))
}

// ------------------------------------------------------------------------------------------------
// Server

/// The daemon's listening socket. The socket file is removed on drop (only if it is still ours).
#[derive(Debug)]
pub struct SocketListener {
    listener: UnixListener,
    path: PathBuf,
    /// (device, inode) of the socket file we bound.
    id: (u64, u64),
}

impl SocketListener {
    /// Binds `<socket folder>/<name>` with mode 0600 and verifies it. Call it only while holding
    /// the host lock. An old socket file of ours is replaced when nobody answers on it; one that
    /// answers fails with `AddrInUse` (the `FIRST_PIPE_INSTANCE` rule); anything that is not our
    /// socket is refused and left alone.
    pub fn bind(name: &str) -> io::Result<Self> {
        let me = uid();
        let dir = socket_dir(true)?;
        let path = socket_path(&dir, name)?;
        match std::fs::symlink_metadata(&path) {
            Ok(m) => {
                if !m.file_type().is_socket() || m.uid() != me {
                    return Err(not_private(&path, "is not our socket; left in place"));
                }
                if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "another service is serving this data folder",
                    ));
                }
                std::fs::remove_file(&path)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&path)?;
        // The folder (0700) already keeps others out; the file mode is a second wall.
        let checked = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .and_then(|()| verify_private_socket(&path, me))
            .and_then(|()| std::fs::symlink_metadata(&path));
        match checked {
            Ok(m) => Ok(Self {
                listener,
                path,
                id: (m.dev(), m.ino()),
            }),
            Err(e) => {
                drop(listener);
                let _ = std::fs::remove_file(&path);
                Err(e)
            }
        }
    }

    /// Full path of the socket file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Owner and mode of the socket and its folder (diagnostics; the Unix "effective SDDL").
    pub fn describe(&self) -> String {
        let one = |p: &Path| match std::fs::symlink_metadata(p) {
            Ok(m) => format!("uid={} mode={:o}", m.uid(), m.mode() & 0o7777),
            Err(e) => format!("<lstat error {e}>"),
        };
        let dir = self.path.parent().unwrap_or(Path::new("/"));
        format!(
            "socket {} {}; folder {}",
            self.path.display(),
            one(&self.path),
            one(dir)
        )
    }

    /// The next connection from a process of this user, with its pid. Connections from other
    /// users (or with unreadable credentials) are closed before a byte is read. Cancel-safe.
    pub async fn accept(&self) -> io::Result<(UnixStream, Option<u32>)> {
        let me = uid();
        loop {
            let (s, _) = self.listener.accept().await?;
            match s.peer_cred() {
                Ok(c) if c.uid() == me => {
                    return Ok((s, c.pid().and_then(|p| u32::try_from(p).ok())));
                }
                _ => drop(s),
            }
        }
    }
}

impl Drop for SocketListener {
    fn drop(&mut self) {
        // Only our own file: a successor may already have bound a new one.
        if let Ok(m) = std::fs::symlink_metadata(&self.path)
            && (m.dev(), m.ino()) == self.id
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Peer audit token (macOS, R50)
//
// The kernel reads `LOCAL_PEERTOKEN` when asked, not at connect: it is the audit token of the
// process that last used the client's end of the socket. Measured on macOS 26: after the peer
// `exec`s, the token's pid version changes; after another process writes through a shared
// descriptor, the token is that process's. So one token proves nothing about who connected, and
// the daemon pairs two of them around a nonce (`host.challenge` / `host.attest` in
// `sr_engine::pipe`): the process that answers must be the very process (same pid and pid
// version) that was on the socket when the nonce went out, and that process's image must be
// ours. A process that writes and then `exec`s our app has a new pid version by the time it
// could answer; a helper that answers through a shared descriptor has another pid.

/// `audit_token_t` (`<mach/message.h>`) and the libbsm / libproc calls that read it.
#[cfg(target_os = "macos")]
mod bsm {
    use std::ffi::{c_int, c_void};

    /// Eight 32-bit words; only libbsm's accessors interpret them.
    #[repr(C)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct AuditToken {
        pub val: [u32; 8],
    }

    #[link(name = "bsm")]
    unsafe extern "C" {
        pub fn audit_token_to_pid(t: AuditToken) -> libc::pid_t;
        pub fn audit_token_to_pidversion(t: AuditToken) -> c_int;
        pub fn audit_token_to_euid(t: AuditToken) -> libc::uid_t;
    }

    // libproc is part of libSystem (macOS 11+).
    unsafe extern "C" {
        pub fn proc_pidpath_audittoken(t: *mut AuditToken, buf: *mut c_void, size: u32) -> c_int;
    }
}

/// macOS (R50): the audit token of the process that last used the peer's end of a socket.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PeerToken(bsm::AuditToken);

#[cfg(target_os = "macos")]
impl std::fmt::Debug for PeerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PeerToken(pid {} v{})", self.pid(), self.pid_version())
    }
}

#[cfg(target_os = "macos")]
impl PeerToken {
    /// Reads `LOCAL_PEERTOKEN` of the connected socket `sock` now.
    pub fn of(sock: &impl std::os::fd::AsFd) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        let mut t = bsm::AuditToken { val: [0; 8] };
        let mut len = libc::socklen_t::try_from(size_of::<bsm::AuditToken>())
            .map_err(|_| io::Error::other("audit token size"))?;
        // SAFETY: `t` is a writable audit_token_t of `len` bytes and outlives the call; the
        // descriptor is borrowed for the call.
        let rc = unsafe {
            libc::getsockopt(
                sock.as_fd().as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERTOKEN,
                (&mut t as *mut bsm::AuditToken).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        if usize::try_from(len).ok() != Some(size_of::<bsm::AuditToken>()) {
            return Err(io::Error::other("short audit token"));
        }
        Ok(Self(t))
    }

    /// Process id.
    pub fn pid(&self) -> u32 {
        // SAFETY: a pure accessor on a token passed by value.
        u32::try_from(unsafe { bsm::audit_token_to_pid(self.0) }).unwrap_or(0)
    }

    /// Pid version: changes when the process `exec`s, and a reused pid gets a new one.
    pub fn pid_version(&self) -> i32 {
        // SAFETY: a pure accessor on a token passed by value.
        unsafe { bsm::audit_token_to_pidversion(self.0) }
    }

    /// Effective user id.
    pub fn uid(&self) -> u32 {
        // SAFETY: a pure accessor on a token passed by value.
        unsafe { bsm::audit_token_to_euid(self.0) }
    }

    /// Same process and same program image: equal pid and pid version.
    pub fn same_process(&self, other: &Self) -> bool {
        self.pid() == other.pid() && self.pid_version() == other.pid_version()
    }

    /// Image path of exactly this process (`proc_pidpath_audittoken`). `None` once it has
    /// exited or `exec`ed (the kernel compares the pid version), so a path is never the one of
    /// a later program under the same pid.
    pub fn image_path(&self) -> Option<PathBuf> {
        let mut t = self.0;
        let mut buf = vec![0u8; usize::try_from(libc::PROC_PIDPATHINFO_MAXSIZE).ok()?];
        let cap = u32::try_from(buf.len()).ok()?;
        // SAFETY: `t` is a writable copy of the token and `buf` holds `cap` writable bytes; both
        // outlive the call, which writes at most `cap` bytes and returns the length.
        let n = unsafe { bsm::proc_pidpath_audittoken(&mut t, buf.as_mut_ptr().cast(), cap) };
        let n = usize::try_from(n)
            .ok()
            .filter(|n| *n > 0 && *n <= buf.len())?;
        buf.truncate(n);
        Some(PathBuf::from(OsString::from_vec(buf)))
    }
}

/// macOS (R50): the `host.attest` rule. `challenged` was read when the nonce went out,
/// `answered` when the answer came in. Returns the image path of the answering process when it is
/// the same process (pid and pid version), of this user, and still running that image; the caller
/// then decides whether the image is one of ours.
#[cfg(target_os = "macos")]
pub fn attested_image(challenged: &PeerToken, answered: &PeerToken) -> Option<PathBuf> {
    if !challenged.same_process(answered) || answered.uid() != uid() {
        return None;
    }
    answered.image_path()
}

/// macOS (R50): reads the peer token of one accepted connection after its stream was split
/// (a close-on-exec duplicate of the server's descriptor, closed with the session).
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct PeerProbe(std::os::fd::OwnedFd);

#[cfg(target_os = "macos")]
impl PeerProbe {
    pub fn new(stream: &UnixStream) -> io::Result<Self> {
        use std::os::fd::AsFd;
        Ok(Self(stream.as_fd().try_clone_to_owned()?))
    }

    /// The peer token now ([`PeerToken::of`]).
    pub fn token(&self) -> io::Result<PeerToken> {
        PeerToken::of(&self.0)
    }
}

/// 32 random bytes from the kernel (`getentropy`) as hex: the `host.challenge` nonce.
#[cfg(target_os = "macos")]
pub fn random_nonce() -> io::Result<String> {
    let mut b = [0u8; 32];
    // SAFETY: `b` holds 32 writable bytes (getentropy allows up to 256).
    if unsafe { libc::getentropy(b.as_mut_ptr().cast(), b.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sr-unix-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn private_dir_rules() {
        let d = tmp("dir");
        let me = uid();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(verify_private_dir(&d, me).is_ok());
        for mode in [0o750, 0o705, 0o770, 0o777] {
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).unwrap();
            let e = verify_private_dir(&d, me).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{mode:o}");
        }
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Another owner is refused.
        let e = verify_private_dir(&d, me.wrapping_add(1)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        // A link to a private folder is judged itself, never followed.
        let link = d.with_extension("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&d, &link).unwrap();
        let e = verify_private_dir(&link, me).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        // Missing passes through as NotFound (no daemon yet).
        let e = verify_private_dir(&d.join("none"), me).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn inherited_descriptors_are_closed_before_exec() {
        use std::os::fd::AsRawFd;
        // A descriptor without close-on-exec, as an MCP client may leave one (dup clears it).
        let f = File::open("/dev/null").unwrap();
        // SAFETY: `f` is open; the copy is closed at the end of the test.
        let fd = unsafe { libc::dup(f.as_raw_fd()) };
        assert!(fd > 2);
        let script = format!("test ! -e /dev/fd/{fd}");
        // Without the hook the child sees it (so the check below means something).
        let plain = Command::new("/bin/sh").args(["-c", &script]).status();
        assert!(!plain.unwrap().success());
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &script]);
        close_inherited_fds(&mut cmd);
        assert!(cmd.status().unwrap().success());
        // A failed exec is still reported (std's close-on-exec error pipe is left alone).
        let mut bad = Command::new("/nonexistent/sr-test-exe");
        close_inherited_fds(&mut bad);
        assert_eq!(bad.spawn().unwrap_err().kind(), io::ErrorKind::NotFound);
        // SAFETY: `fd` is ours and closed once.
        unsafe { libc::close(fd) };
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extended_acl_makes_a_folder_not_private_until_removed() {
        let d = tmp("acl");
        let me = uid();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(extended_acl_entries(&d).unwrap(), (0, 0));
        let ok = Command::new("/bin/chmod")
            .arg("+a")
            .arg("everyone allow list,search")
            .arg(&d)
            .status()
            .unwrap();
        assert!(ok.success());
        assert_eq!(extended_acl_entries(&d).unwrap(), (1, 1));
        // Mode 0700 alone is not private while the ACL lets everyone in.
        let e = verify_private_dir(&d, me).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        make_private_dir(&d, me).unwrap();
        make_private_dir(&d, me).unwrap(); // idempotent
        assert_eq!(extended_acl_entries(&d).unwrap(), (0, 0));
        let m = std::fs::symlink_metadata(&d).unwrap();
        assert_eq!(m.mode() & 0o777, 0o700);
        verify_private_dir(&d, me).unwrap();
        // A link is refused, never followed; another owner is refused.
        let link = d.with_extension("acl-link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&d, &link).unwrap();
        let e = make_private_dir(&link, me).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        let e = make_private_dir(&d, me.wrapping_add(1)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn socket_paths_fit_and_stay_plain() {
        let d = Path::new("/tmp/x");
        assert!(socket_path(d, "sr-0123456789abcdef-0123456789abcdef.sock").is_ok());
        assert!(socket_path(d, "../evil").is_err());
        assert!(socket_path(d, "a/b").is_err());
        assert!(socket_path(d, "").is_err());
        let long = "a".repeat(MAX_SOCKET_PATH);
        assert!(socket_path(d, &long).is_err());
    }

    #[test]
    fn the_real_socket_dir_is_private() {
        let d = socket_dir(true).unwrap();
        let m = std::fs::symlink_metadata(&d).unwrap();
        assert!(m.file_type().is_dir());
        assert_eq!(m.uid(), uid());
        assert_eq!(m.mode() & 0o077, 0);
        let name = crate::naming::pipe_name("501", Path::new("/Users/u/Library/x"));
        assert!(socket_path(&d, &name).is_ok(), "{}", d.display());
    }

    #[tokio::test]
    async fn socket_is_private_and_peer_checked() {
        let name = format!("sr-unix-test-{}.sock", std::process::id());
        let l = SocketListener::bind(&name).unwrap();
        let m = std::fs::symlink_metadata(l.path()).unwrap();
        assert!(m.file_type().is_socket());
        assert_eq!(m.uid(), uid());
        assert_eq!(m.mode() & 0o777, 0o600);
        // A second server for the same name is refused while the first answers.
        assert_eq!(
            SocketListener::bind(&name).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
        let (client, served) = tokio::join!(connect(&name), l.accept());
        let client = client.unwrap();
        let (_srv, pid) = served.unwrap();
        assert_eq!(pid, Some(std::process::id()));
        assert_eq!(pipe_server_is_ours(&client).unwrap(), ServerIdentity::Ours);
        assert!(matches!(
            pipe_server_identity(&client, &(uid() + 1).to_string()),
            ServerIdentity::ForeignProcess(_)
        ));
        // A socket opened up to others is refused by the client before connecting.
        std::fs::set_permissions(l.path(), std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            connect(&name).await.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let path = l.path().to_path_buf();
        drop(l);
        assert!(!path.exists(), "socket removed on drop");
        assert!(is_absent(&connect(&name).await.unwrap_err()));
    }

    #[test]
    fn stale_socket_is_replaced_and_foreign_files_are_refused() {
        let name = format!("sr-unix-stale-{}.sock", std::process::id());
        let dir = socket_dir(true).unwrap();
        let path = socket_path(&dir, &name).unwrap();
        // A socket nobody listens on (std listener dropped without unlinking).
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let l = SocketListener::bind(&name).unwrap();
            drop(l);
        });
        // A plain file with the name is not ours to remove.
        std::fs::write(&path, "x").unwrap();
        rt.block_on(async {
            let e = SocketListener::bind(&name).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        });
        assert!(path.is_file());
        let _ = std::fs::remove_file(&path);
    }

    /// R50: a peer that writes and then `exec`s gets a new pid version and image, so a token
    /// read before the exec never matches one read after it, and the old one names no process.
    #[cfg(target_os = "macos")]
    #[test]
    fn peer_token_changes_on_exec() {
        use std::ffi::CString;
        use std::io::{Read, Write};
        use std::os::unix::ffi::OsStrExt;
        let d = tmp("tok");
        let path = d.join("s");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        // Everything the child needs is built before fork: it calls only async-signal-safe
        // functions (socket, connect, write, read, execv, _exit).
        // SAFETY: an all-zero sockaddr_un is valid; the path fits (checked below).
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX).unwrap();
        let bytes = path.as_os_str().as_bytes();
        assert!(bytes.len() < addr.sun_path.len());
        for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
            *d = libc::c_char::from_ne_bytes([*s]);
        }
        let alen = libc::socklen_t::try_from(size_of::<libc::sockaddr_un>()).unwrap();
        let exe = CString::new("/bin/sleep").unwrap();
        let arg1 = CString::new("5").unwrap();
        let argv = [exe.as_ptr(), arg1.as_ptr(), std::ptr::null()];
        // SAFETY: the child runs only the async-signal-safe calls listed above and never returns.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: see above; every pointer was built before fork and is still valid.
            unsafe {
                let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
                let sa = (&raw const addr).cast::<libc::sockaddr>();
                if fd < 0 || libc::connect(fd, sa, alen) != 0 {
                    libc::_exit(1);
                }
                let b = 1u8;
                libc::write(fd, (&raw const b).cast(), 1);
                let mut go = 0u8;
                libc::read(fd, (&raw mut go).cast(), 1);
                libc::execv(exe.as_ptr(), argv.as_ptr());
                libc::_exit(1);
            }
        }
        let (mut s, _) = l.accept().unwrap();
        let mut b = [0u8; 1];
        s.read_exact(&mut b).unwrap();
        let before = PeerToken::of(&s).unwrap();
        assert_eq!(before.pid(), u32::try_from(pid).unwrap());
        assert_eq!(before.uid(), uid());
        assert!(before.same_process(&PeerToken::of(&s).unwrap()));
        let canon = |p: PathBuf| std::fs::canonicalize(p).unwrap();
        assert_eq!(
            canon(before.image_path().unwrap()),
            canon(std::env::current_exe().unwrap())
        );
        assert!(attested_image(&before, &before).is_some());
        s.write_all(b"g").unwrap();
        let mut after = before;
        for _ in 0..500 {
            after = PeerToken::of(&s).unwrap();
            if !after.same_process(&before) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(after.pid(), before.pid());
        assert!(!after.same_process(&before), "exec changes the pid version");
        assert_eq!(after.image_path().unwrap(), Path::new("/bin/sleep"));
        assert!(before.image_path().is_none(), "the pre-exec image is gone");
        assert!(attested_image(&before, &after).is_none());
        // SAFETY: `pid` is our child; it is killed and reaped once.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        assert_eq!(random_nonce().unwrap().len(), 64);
        assert_ne!(random_nonce().unwrap(), random_nonce().unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn locks_pids_and_images() {
        let d = tmp("lock");
        let p = d.join("x.lock");
        let a = lock_file(&p, false).unwrap();
        assert!(a.is_some());
        assert!(lock_file(&p, false).unwrap().is_none());
        drop(a);
        // A child forked by a parallel test holds a copy of every descriptor until its exec,
        // so the lock can stay taken for a moment after the drop.
        let again = (0..200).any(|_| {
            let got = lock_file(&p, false).unwrap().is_some();
            if !got {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            got
        });
        assert!(again);
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        let me = process_image_path(std::process::id()).unwrap();
        assert_eq!(
            std::fs::canonicalize(me).unwrap(),
            std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap()
        );
        assert!(local_appdata().unwrap().is_absolute());
        assert!(harden_dll_search());
        let _ = std::fs::remove_dir_all(&d);
    }
}
