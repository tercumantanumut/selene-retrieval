//! sr-app: Selene Retrieval's desktop app (Tauri 2). A **client** of the background service
//! only (02 §2.1): it never hosts the engine. Every page lives in `ui/`.
//!
//! Plugins: single-instance is registered **first** (a second launch focuses this window), then
//! autostart (`--background` = tray only), dialog, notification, updater.
//!
//! Command line: `--background` (autostart: tray only), `--approve <id>` (R-UX30: the service
//! launches the app for a folder request made while it was closed; the window opens with that
//! request's card on top and focused. Nothing is ever approved without the user's click). A second
//! launch with `--approve` hands the id to the running instance.
//!
//! Debug-only dev hooks (ignored in release builds, like sr-client's): `SR_DATA_DIR` (service data
//! dir, via sr-client), `SR_LAUNCH_EXE` / `SR_DAEMON_EXE` (via sr-client), and
//! `SR_CLAUDE_ROOTS_DIR` (fake `Roaming` / `Local` / `Home` roots for every Connect writer, so a
//! dev run never reads or writes the real Claude, Cursor or VS Code config), and `SR_DOCUMENTS_DIR`
//! (a fake Documents for "Create a folder for me", Amendment 13).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod activity;
mod apps;
mod cli_signin;
mod commands;
mod connect;
mod connected;
mod daemon;
mod default_folder;
mod diagnostics;
mod dto;
mod estimate;
mod folders;
mod index_move;
mod opener;
mod policy;
mod prefs;
mod providers;
mod search;
mod state;
mod tray;
mod updater;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, Manager, WindowEvent};
use tauri_plugin_notification::NotificationExt;
use tauri_specta::{Event, collect_commands, collect_events};
use tokio::sync::watch;

use crate::apps::{ChangeTracker, ConnectCtx};
use crate::commands::{AppCtx, cmd};
use crate::connect::ClientRoots;
use crate::daemon::{DaemonLink, Shared};
use crate::dto::{AppView, ClientApp, ClientsView, ConnectStatus, Overall, ServiceSignal};

/// Pushed whenever the view changes (snapshot, event, re-attach). Never polled.
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct StateChanged(pub AppView);

/// Pushed when Claude's config file changes on disk (ours or someone else's write).
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct ClaudeConfigChanged(pub ConnectStatus);

/// Forwards M4 service signals to the UI as `ServicePush` events (push only).
async fn signal_loop(app: AppHandle, mut rx: tokio::sync::broadcast::Receiver<ServiceSignal>) {
    loop {
        match rx.recv().await {
            Ok(s) => {
                let _ = ServicePush(s).emit(&app);
            }
            // Missed some under a burst: one "re-read everything" is enough.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                for s in [
                    ServiceSignal::GroupsChanged,
                    ServiceSignal::ClientsChanged,
                    ServiceSignal::ActivityAdded,
                    ServiceSignal::EgressRecorded,
                ] {
                    let _ = ServicePush(s).emit(&app);
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Pushed when any connected app's config changes on disk (page 29 cards; never polled).
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct ClientsChanged(pub ClientsView);

/// M4 service pushes: "re-read X" signals and one-off results (`ServiceSignal`).
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct ServicePush(pub ServiceSignal);

/// Where `cargo test -p sr-app` writes / checks the committed TypeScript bindings.
pub const BINDINGS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/src/bindings.ts");

pub fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            cmd::get_state,
            cmd::retry_connection,
            cmd::estimate_folder,
            cmd::add_folder,
            cmd::remove_folder,
            cmd::sync_folder,
            cmd::set_folder_paused,
            cmd::decide_approval,
            cmd::dismiss_notices,
            cmd::use_recommended_model,
            cmd::cancel_model_change,
            cmd::get_advanced_settings,
            cmd::set_advanced_settings,
            cmd::choose_search_tier,
            cmd::choose_multilingual_search,
            cmd::get_claude_status,
            cmd::connect_claude,
            cmd::disconnect_claude,
            cmd::get_diagnostics,
            cmd::get_about,
            cmd::set_start_with_windows,
            cmd::check_for_update,
            cmd::install_update,
            cmd::get_providers,
            cmd::get_provider_status,
            cmd::save_provider,
            cmd::check_provider,
            cmd::set_provider_model,
            cmd::remove_provider,
            cmd::get_consents,
            cmd::set_consent,
            cmd::dismiss_consent_request,
            cmd::get_answers_settings,
            cmd::set_answers_settings,
            cmd::restart_service,
            cmd::get_clients,
            cmd::connect_client,
            cmd::disconnect_client,
            cmd::repair_client,
            cmd::get_licenses,
            cmd::save_diagnostics,
            cmd::open_link,
            cmd::show_in_folder,
            cmd::get_folder_detail,
            cmd::get_skipped_files,
            cmd::retry_files,
            cmd::folder_change_impact,
            cmd::check_folder_settings,
            cmd::update_folder_settings,
            cmd::stop_folder,
            cmd::get_groups,
            cmd::create_group,
            cmd::rename_group,
            cmd::delete_group,
            cmd::move_folder_to_group,
            cmd::search,
            cmd::ask,
            cmd::cancel_request,
            cmd::enhance_prompt,
            cmd::get_activity,
            cmd::get_egress,
            cmd::get_service_clients,
            cmd::set_client_scope,
            cmd::set_http_enabled,
            cmd::create_tool_token,
            cmd::rotate_tool_token,
            cmd::revoke_tool,
            cmd::get_index_location,
            cmd::check_index_target,
            cmd::move_index,
            cmd::cancel_index_move,
            cmd::recover_index,
            cmd::forget_client,
            cmd::cli_signin_status,
            cmd::cli_signin_start,
            cmd::get_default_folder,
            cmd::create_default_folder,
            cmd::open_folder,
        ])
        .events(collect_events![
            StateChanged,
            ClaudeConfigChanged,
            ClientsChanged,
            ServicePush
        ])
}

pub fn typescript() -> specta_typescript::Typescript {
    specta_typescript::Typescript::default().header(
        "// Generated by tauri-specta from crates/app (`cargo test -p sr-app`). Do not edit by hand.\n",
    )
}

fn client_roots(app: &AppHandle) -> tauri::Result<ClientRoots> {
    if let Some(d) = sr_client::naming::dev_env("SR_CLAUDE_ROOTS_DIR") {
        let d = PathBuf::from(d);
        return Ok(ClientRoots {
            roaming: d.join("Roaming"),
            local: d.join("Local"),
            home: d.join("Home"),
        });
    }
    // Known-folder API (FOLDERID_RoamingAppData / LocalAppData / Profile), not env vars.
    Ok(ClientRoots {
        roaming: app.path().data_dir()?,
        local: app.path().local_data_dir()?,
        home: app.path().home_dir()?,
    })
}

fn window_visible(app: &AppHandle) -> bool {
    app.get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Page 29 cards (with "last used" from the service once the contract provides it).
pub fn clients_view(ctx: &AppCtx) -> ClientsView {
    apps::clients_view(&ctx.connect, &ctx.tracker(), &HashMap::new())
}

/// Re-reads every app's config, pushes the cards and Claude's block, and tells the user once
/// when Claude removed our entry by itself while the window is hidden (R-UX23).
pub fn push_clients(app: &AppHandle) {
    let ctx = app.state::<AppCtx>();
    let claude = ctx.connect.state(ClientApp::ClaudeDesktop);
    let lost = ctx
        .tracker()
        .observe(ClientApp::ClaudeDesktop, claude, Instant::now());
    let _ = ClaudeConfigChanged(ctx.claude_status()).emit(app);
    let _ = ClientsChanged(clients_view(&ctx)).emit(app);
    if lost && !window_visible(app) {
        // Page 16 [New, M4]. The notification plugin has no action buttons on Windows desktop;
        // the card in the app carries [Reconnect Claude Desktop].
        notify(
            app,
            "Claude was disconnected",
            "Claude's settings no longer include Selene Retrieval.",
        );
    }
}

/// (Re)arm the watcher on the folders holding the apps' config files. Pushes the new states so
/// "Connected" never goes stale (R-UX3: apps may rewrite their files).
pub fn watch_client_configs(app: &AppHandle) {
    let ctx = app.state::<AppCtx>();
    let dirs = ctx.connect.watch_dirs();
    let handle = app.clone();
    let w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        let ours = ev.paths.iter().any(|p| {
            p.file_name().is_some_and(|n| {
                n == connect::claude::CONFIG_FILE
                    || n == connect::cursor::CONFIG_FILE
                    || n == connect::vscode::CONFIG_FILE
            })
        });
        if ours {
            push_clients(&handle);
        }
    });
    let mut w = match w {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(err = %e, "config watcher unavailable");
            return;
        }
    };
    for d in &dirs {
        let _ = w.watch(d, RecursiveMode::NonRecursive);
    }
    *ctx.watcher
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(w);
}

fn notify_problem(app: &AppHandle, v: &AppView) {
    if window_visible(app) {
        return; // the in-app toast covers it
    }
    let body = match &v.problem {
        Some(p) => format!("{} Open Selene Retrieval to fix it.", p.body),
        None => "A folder needs your attention. Open Selene Retrieval to see what happened.".into(),
    };
    let title = v
        .problem
        .as_ref()
        .map_or_else(|| "Selene Retrieval".to_owned(), |p| p.title.clone());
    notify(app, &title, &body);
}

/// The first time the window is closed (R-UX22): the app is still next to the clock.
fn first_close_note(app: &AppHandle) {
    let Some(ctx) = app.try_state::<AppCtx>() else {
        return;
    };
    if prefs::first_time(ctx.prefs_dir.as_deref(), |p| &mut p.close_note_shown) {
        notify(
            app,
            "Selene Retrieval is still running",
            "It's next to the clock and keeps your folders up to date.",
        );
    }
}

/// Tray "Quit" (OQ-10 c): the first time, say when folders update again, then exit.
fn quit_app(app: &AppHandle) {
    if let Some(ctx) = app.try_state::<AppCtx>()
        && prefs::first_time(ctx.prefs_dir.as_deref(), |p| &mut p.quit_note_shown)
    {
        notify(
            app,
            "Selene Retrieval",
            "Your folders update again the next time Selene Retrieval starts.",
        );
    }
    app.exit(0);
}

/// Push loop: every state change goes to the UI (event), the tray, and, on a new problem while
/// the window is hidden, a Windows notification. The short pause after each push coalesces
/// bursts (≤ ~7 pushes/s); it is not polling: the loop only wakes on a change.
async fn push_loop(app: AppHandle, mut rx: watch::Receiver<AppView>) {
    let mut last = Overall::Starting;
    let mut last_focus: Option<String> = None;
    while rx.changed().await.is_ok() {
        let v = rx.borrow_and_update().clone();
        let _ = StateChanged(v.clone()).emit(&app);
        tray::update(&app, &v);
        if v.overall == Overall::Problem && last != Overall::Problem {
            notify_problem(&app, &v);
        }
        // A new folder request opens the window with its card on top (OQ-25 a, R-UX30). Only the
        // card's heading gets focus; deciding always needs a deliberate click.
        if v.focus_approval.is_some() && v.focus_approval != last_focus {
            tray::show_main(&app);
        }
        last_focus.clone_from(&v.focus_approval);
        last = v.overall;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// `--approve <id>` / `--approve=<id>`. Only a well-formed request id is accepted; it only
/// selects which card to bring into view (the decision is always the user's click).
pub fn approve_arg(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let v = if a == "--approve" {
            it.next().map(String::as_str)
        } else {
            a.strip_prefix("--approve=")
        };
        if let Some(v) = v {
            return v
                .trim()
                .parse::<sr_common::ids::ApprovalId>()
                .ok()
                .map(|id| id.to_string());
        }
    }
    None
}

/// Open the window for a folder request: show it on top and refresh the pending list.
fn open_for_approval(app: &AppHandle, id: String) {
    let Some(ctx) = app.try_state::<AppCtx>() else {
        // Still starting up: just show the window; the first attach lists pending requests.
        tray::show_main(app);
        return;
    };
    ctx.shared.update(|s| s.set_focus_approval(id));
    let (link, shared) = (ctx.link.clone(), ctx.shared.clone());
    tauri::async_runtime::spawn(async move { daemon::refresh_approvals(&*link, &shared).await });
    tray::show_main(app);
}

fn main() {
    sr_client::harden_dll_search(); // S-08 (M5-E export): before anything can load a DLL by name
    let builder = specta_builder();
    let args: Vec<String> = std::env::args().collect();
    let approve = approve_arg(&args);
    // A folder request always opens the window, even from an autostart launch.
    let background = approve.is_none() && args.iter().any(|a| a == "--background");

    let result = tauri::Builder::default()
        // Must be first: a second launch hands over to this instance and exits.
        .plugin(tauri_plugin_single_instance::init(
            |app, argv, _cwd| match approve_arg(&argv) {
                Some(id) => open_for_approval(app, id),
                None => tray::show_main(app),
            },
        ))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            let (shared, rx) = Shared::new();
            let link = Arc::new(DaemonLink::new());
            let bridge_exe = sr_client::naming::sibling_exe("sr-bridge.exe")
                .map_err(|e| std::io::Error::other(format!("{:?}", e.code)))?;
            let connect = ConnectCtx {
                roots: client_roots(app.handle())?,
                bridge_exe,
            };
            // Remember Claude's state at start, so a later removal by Claude is noticed.
            let mut tracker = ChangeTracker::default();
            tracker.observe(
                ClientApp::ClaudeDesktop,
                connect.state(ClientApp::ClaudeDesktop),
                Instant::now(),
            );
            app.manage(AppCtx {
                link: link.clone(),
                shared: shared.clone(),
                connect,
                tracker: Mutex::new(tracker),
                watcher: Mutex::new(None),
                prefs_dir: app.path().app_config_dir().ok(),
            });
            if let Some(id) = approve.clone() {
                // Before the first attach: the snapshot's approvals refresh then lists it.
                shared.update(|s| s.set_focus_approval(id));
            }
            tray::build(app.handle(), quit_app)?;
            tauri::async_runtime::spawn(signal_loop(
                app.handle().clone(),
                shared.signals.subscribe(),
            ));
            tauri::async_runtime::spawn(daemon::run_events(link, shared));
            tauri::async_runtime::spawn(push_loop(app.handle().clone(), rx));
            watch_client_configs(app.handle());
            if !background {
                tray::show_main(app.handle());
            }
            Ok(())
        })
        .on_window_event(|w, e| {
            // Closing the window keeps the app in the tray; "Quit" is in the tray menu.
            if let WindowEvent::CloseRequested { api, .. } = e {
                api.prevent_close();
                let _ = w.hide();
                first_close_note(w.app_handle());
            }
        })
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("Selene Retrieval couldn't start: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalize(s: &str) -> String {
        s.replace("\r\n", "\n")
    }

    #[test]
    fn approve_argument_accepts_only_request_ids() {
        let id = sr_common::ids::ApprovalId::new().to_string();
        let a = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            approve_arg(&a(&["sr-app", "--approve", &id])),
            Some(id.clone())
        );
        assert_eq!(
            approve_arg(&a(&["sr-app", &format!("--approve={id}")])),
            Some(id.clone())
        );
        assert_eq!(approve_arg(&a(&["sr-app", "--background"])), None);
        assert_eq!(approve_arg(&a(&["sr-app", "--approve"])), None);
        assert_eq!(approve_arg(&a(&["sr-app", "--approve", "../../etc"])), None);
        assert_eq!(approve_arg(&a(&["sr-app", "--approve", "allow-all"])), None);
    }

    /// The committed `ui/src/bindings.ts` must match the Rust commands and types. Set
    /// `SR_UPDATE_BINDINGS=1` to rewrite it.
    #[test]
    fn bindings_are_up_to_date() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("bindings.ts");
        specta_builder().export(typescript(), &fresh).unwrap();
        let fresh = std::fs::read_to_string(&fresh).unwrap();
        if std::env::var_os("SR_UPDATE_BINDINGS").is_some() {
            std::fs::write(BINDINGS_PATH, &fresh).unwrap();
        }
        let committed = std::fs::read_to_string(BINDINGS_PATH).unwrap_or_default();
        assert!(
            normalize(&committed) == normalize(&fresh),
            "ui/src/bindings.ts is stale: run `SR_UPDATE_BINDINGS=1 cargo test -p sr-app bindings`"
        );
    }
}
