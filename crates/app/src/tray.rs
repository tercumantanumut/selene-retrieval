//! Tray icon: status at a glance (starting / indexing / up to date / needs attention), the plain
//! summary in the tooltip and menu, "Open" and "Quit". Updated only from pushed state.

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Runtime};

use crate::dto::{AppView, Overall};

pub const TRAY_ID: &str = "main";

/// The disabled first menu line showing the summary.
pub struct StatusItem<R: Runtime>(pub MenuItem<R>);

fn icon_bytes(o: Overall) -> &'static [u8] {
    match o {
        Overall::Starting | Overall::NoFolders => include_bytes!("../icons/tray-idle.png"),
        Overall::Indexing => include_bytes!("../icons/tray-indexing.png"),
        Overall::Synced => include_bytes!("../icons/tray-synced.png"),
        Overall::Problem => include_bytes!("../icons/tray-problem.png"),
    }
}

/// Short words for the state, used before the summary in the tooltip.
pub fn state_word(o: Overall) -> &'static str {
    match o {
        Overall::Starting => "Starting",
        Overall::NoFolders => "No folders yet",
        Overall::Indexing => "Indexing",
        Overall::Synced => "Up to date",
        Overall::Problem => "Needs attention",
    }
}

pub fn tooltip(v: &AppView) -> String {
    let mut t = format!("Selene Retrieval: {}", v.summary);
    // Windows truncates tray tooltips at 127 characters (macOS shows the same text).
    if t.chars().count() > 120 {
        t = t.chars().take(119).collect::<String>() + "…";
    }
    t
}

/// macOS: a Dock icon (and the app menu) only while the window is shown, like the taskbar button
/// on Windows, which comes and goes with the window by itself. Nothing to do elsewhere.
pub fn set_dock_visible<R: Runtime>(app: &AppHandle<R>, visible: bool) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(if visible {
        tauri::ActivationPolicy::Regular
    } else {
        tauri::ActivationPolicy::Accessory
    });
    #[cfg(not(target_os = "macos"))]
    let _ = (app, visible);
}

pub fn show_main<R: Runtime>(app: &AppHandle<R>) {
    if let Some(w) = app.get_webview_window("main") {
        set_dock_visible(app, true);
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// `on_quit` runs for the menu's "Quit" (the app shows a one-time note, then exits).
pub fn build<R: Runtime>(app: &AppHandle<R>, on_quit: fn(&AppHandle<R>)) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open Selene Retrieval", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&status, &sep, &open, &quit])?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(icon_bytes(Overall::Starting))?)
        .tooltip("Selene Retrieval: Starting…")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, ev| match ev.id().as_ref() {
            "open" => show_main(app),
            "quit" => on_quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    app.manage(StatusItem(status));
    Ok(())
}

pub fn update<R: Runtime>(app: &AppHandle<R>, v: &AppView) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(tooltip(v)));
        if let Ok(img) = Image::from_bytes(icon_bytes(v.overall)) {
            let _ = tray.set_icon(Some(img));
        }
    }
    if let Some(item) = app.try_state::<StatusItem<R>>() {
        let _ = item.0.set_text(status_line(v));
    }
}

/// The disabled first menu line: the summary only (OQ-27; "{state} · {summary}" read "Up to date
/// · Up to date"). Falls back to the state word when there is no summary.
pub fn status_line(v: &AppView) -> String {
    if v.summary.trim().is_empty() {
        state_word(v.overall).to_owned()
    } else {
        v.summary.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_is_short_and_plain() {
        let mut v = AppView {
            summary: "Indexing 340 of 2,100 files".into(),
            ..AppView::default()
        };
        assert_eq!(tooltip(&v), "Selene Retrieval: Indexing 340 of 2,100 files");
        v.summary = "x".repeat(300);
        assert!(tooltip(&v).chars().count() <= 120);
    }

    #[test]
    fn status_line_never_repeats_the_state_word() {
        let mut v = AppView {
            overall: Overall::Synced,
            summary: "Up to date".into(),
            ..AppView::default()
        };
        assert_eq!(status_line(&v), "Up to date");
        v.summary.clear();
        assert_eq!(status_line(&v), "Up to date");
        v.overall = Overall::NoFolders;
        v.summary = "No folders yet".into();
        assert_eq!(status_line(&v), "No folders yet");
    }

    #[test]
    fn every_state_has_an_icon_that_decodes() {
        for o in [
            Overall::Starting,
            Overall::NoFolders,
            Overall::Indexing,
            Overall::Synced,
            Overall::Problem,
        ] {
            assert!(Image::from_bytes(icon_bytes(o)).is_ok(), "{o:?}");
            assert!(!state_word(o).is_empty());
        }
    }
}
