//! sr-launch: short-lived launcher run by clients (sr-bridge, sr-app) when the daemon is absent.
//!
//! `sr-launch --data-dir <dir>`: starts `sr-daemon.exe` (next to this exe) with a *verified*
//! Job breakaway, else through the per-user scheduled task, else in-job (degraded), then exits.
//! Logic and exit codes: `sr_client::launch`. Being a separate, short-lived process means the
//! daemon's parent is dead by the time a client's tree is killed (`taskkill /T`).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;

use sr_client::launch::{self, LaunchArgs, code};

fn main() -> ExitCode {
    sr_client::harden_dll_search(); // S-08 (M5-E export): before anything can load a DLL by name
    let mut data_dir = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--version" => {
                println!("sr-launch {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--data-dir" => data_dir = it.next().map(PathBuf::from),
            _ => return ExitCode::from(code::USAGE as u8),
        }
    }
    let Some(data_dir) = data_dir else {
        return ExitCode::from(code::USAGE as u8);
    };
    let args = match LaunchArgs::new(data_dir.clone()) {
        Ok(a) => a,
        Err(e) => {
            launch::log(&data_dir, &format!("launch usage error: {e}"));
            return ExitCode::from(code::USAGE as u8);
        }
    };
    let outcome = launch::run(&args);
    ExitCode::from(u8::try_from(outcome.exit_code()).unwrap_or(u8::MAX))
}
