//! Updater wiring (tauri-plugin-updater). Order on install (02 §2.2):
//! check → download (minisign verified against the pubkey in `tauri.conf.json`) →
//! **`host.shutdown{drain}`** so the service finishes its batch, commits and exits → install → restart.
//!
//! M2-A: the endpoint and pubkey in `tauri.conf.json` are placeholders (`updates.invalid` never
//! resolves; the placeholder key verifies nothing), so a check fails with a plain "couldn't
//! check" message and an install can never apply an unsigned file. Real values arrive with
//! signing (M2 installer work).
#![allow(clippy::result_large_err)] // `UiError` is the Tauri IPC error (see commands.rs).

use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

use crate::daemon::DaemonLink;
use crate::dto::{UiAction, UiError, UpdateInfo};

fn check_failed() -> UiError {
    UiError::new(
        "Couldn't check for updates",
        "You may be offline, or the update service isn't reachable right now.",
        UiAction::Retry,
        "Retry",
        "UpdateCheck",
    )
}

fn install_failed(detail: &str) -> UiError {
    UiError::new(
        "The update didn't install",
        "Nothing was changed. You can keep using Selene Retrieval and try again later.",
        UiAction::Retry,
        "Retry",
        detail,
    )
}

pub async fn check(app: &AppHandle) -> Result<Option<UpdateInfo>, UiError> {
    let u = app.updater().map_err(|e| {
        tracing::warn!(err = %e, "updater not configured");
        check_failed()
    })?;
    match u.check().await {
        Ok(Some(up)) => Ok(Some(UpdateInfo {
            version: up.version.clone(),
            notes: up.body.clone(),
        })),
        Ok(None) => Ok(None),
        Err(e) => {
            tracing::info!(err = %e, "update check failed");
            Err(check_failed())
        }
    }
}

pub async fn install(app: &AppHandle, link: &DaemonLink) -> Result<(), UiError> {
    let u = app.updater().map_err(|_| check_failed())?;
    let up = u
        .check()
        .await
        .map_err(|_| check_failed())?
        .ok_or_else(|| install_failed("NoUpdate"))?;
    // Signature is verified here; a bad or placeholder key fails before anything is stopped.
    let bytes = up.download(|_, _| {}, || {}).await.map_err(|e| {
        tracing::warn!(err = %e, "update download/verify failed");
        install_failed("UpdateDownload")
    })?;
    if let Err(e) = link.shutdown_drain().await {
        tracing::warn!(code = ?e.code, "service drain before update failed");
        return Err(install_failed("UpdateDrain"));
    }
    up.install(bytes).map_err(|e| {
        tracing::warn!(err = %e, "update install failed");
        install_failed("UpdateInstall")
    })?;
    app.restart();
}
