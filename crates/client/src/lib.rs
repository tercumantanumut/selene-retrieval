//! sr-client: the pipe client shared by sr-bridge and sr-app, plus the `sr-launch` logic.
//!
//! * [`attach()`]: connect → `spawn.lock` → re-check → `sr-launch` → poll with backoff (≤ 10 s)
//!   → handshake (with drain-restart for version skew).
//! * [`rpc::AppClient`]: app-RPC requests and event subscription with seq resume;
//!   [`rpc::Subscription`] survives daemon restarts (resume or re-snapshot).
//! * [`launch`]: verified-breakaway → scheduled task → in-job launcher (`sr-launch` bin).
//! * [`naming`]: data dir, pipe name `\.\pipe\sr-<sidhash>-<datadirhash>`, lock and task names.
//! * `win`: the reviewed Win32 FFI (the only `unsafe` in this crate on Windows), also used by the
//!   daemon's pipe server (`sr_engine::pipe`) so both sides share one pipe-name and one DACL
//!   definition.
//! * `unix` (macOS port): the same role over a Unix domain socket in a private per-user folder,
//!   with peer-credential checks on both sides.
//! * [`sys`]: whichever of the two this platform uses. Code outside this crate uses `sys`.
//!
//! No engine dependencies: the bridge must stay free of the native stack.

pub mod attach;
pub mod default_folder;
pub mod launch;
pub mod naming;
pub mod rpc;
#[cfg(unix)]
#[allow(unsafe_code)]
pub mod unix;
#[cfg(windows)]
#[allow(unsafe_code)]
pub mod win;

/// The platform module: `win` on Windows, `unix` on macOS and other Unix systems. Both export
/// the names the clients, the launcher and the daemon need (`Stream`, `ServerStream`,
/// `user_sid_string`, `lock_file`, `process_image_path`, …).
#[cfg(unix)]
pub use unix as sys;
#[cfg(windows)]
pub use win as sys;

pub use attach::{ATTACH_TIMEOUT, AttachOptions, AttachPath, Connection, attach};
pub use naming::pipe_name;
pub use rpc::{AppClient, EventStream, StreamItem, SubItem, Subscription};
/// M5-E S-08: first statement of every `main` (see `win::harden_dll_search`; a no-op on Unix).
pub use sys::harden_dll_search;
