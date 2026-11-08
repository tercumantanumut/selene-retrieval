//! sr-daemon: the only engine process (docs/02 §2.2). Windows subsystem: launched by sr-launch or
//! the per-user scheduled task, it must never open a console window.
//!
//! `sr-daemon [--data-dir <dir>] [--launched-by <breakaway|task|injob|direct>]` runs the daemon;
//! `sr-daemon admin …` is the hidden support CLI (see `sr_mcp_server::admin`).
#![windows_subsystem = "windows"]

use std::process::ExitCode;

fn main() -> ExitCode {
    // Amendment 4.8: before anything can load a DLL by name (with the delay-load flags from
    // build.rs, DirectML.dll / d3d12.dll / dxgi.dll are not mapped before this runs).
    sr_providers::dml::harden_dll_search();
    // Amendment 3.13: one rustls crypto provider for the whole process.
    sr_mcp_server::install_rustls_provider();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("admin") {
        sr_engine::win::attach_parent_console();
        let parsed = match sr_mcp_server::admin::parse(&args[1..]) {
            Ok(a) => a,
            Err(u) => {
                eprintln!("{u}");
                return ExitCode::from(sr_mcp_server::daemon::EXIT_USAGE);
            }
        };
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(1);
            }
        };
        return match rt.block_on(sr_mcp_server::admin::run(parsed)) {
            Ok(out) => {
                println!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", e.user_message().agent_text());
                ExitCode::from(1)
            }
        };
    }
    match sr_mcp_server::daemon::parse_args(&args) {
        Ok(a) => {
            if a.version {
                sr_engine::win::attach_parent_console();
            }
            ExitCode::from(sr_mcp_server::daemon::run(a))
        }
        Err(e) => {
            sr_engine::win::attach_parent_console();
            eprintln!("{e}");
            ExitCode::from(sr_mcp_server::daemon::EXIT_USAGE)
        }
    }
}
