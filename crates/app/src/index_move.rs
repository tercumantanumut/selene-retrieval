//! "Move the search index" (page 32, OQ-49; contract §9). Advanced only. The service checks the
//! target (rule 3.12: a short local path, not inside an added folder or the app), copies,
//! verifies, switches and only then deletes the old copy; the app shows the checks in plain words,
//! the progress pushed as `index.move.progress`, and the result.
#![allow(clippy::result_large_err)] // `UiError` is the Tauri IPC error (see commands.rs).

use serde::{Deserialize, Serialize};
use serde_json::json;
use specta::Type;
use sr_common::api::{IndexLocationStatus, IndexMoveCheck, MoveProblem};
use sr_common::error::{Error, ErrorCode};

use crate::daemon::{Rpc, method};
use crate::dto::UiError;

/// "1.4 GB", "820 MB".
pub fn size_text(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // display only
    let b = bytes as f64;
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", b / 1e9)
    } else {
        format!("{} MB", bytes.div_ceil(1_000_000).max(1))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct IndexLocationView {
    pub path: String,
    pub is_default: bool,
    /// "Now: {path} · {size}".
    pub size: String,
    pub free: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct MoveCheckView {
    /// [Move] enabled.
    pub ok: bool,
    /// Where the index will live ("<target>\SeleneRetrieval Index").
    pub index_path: String,
    /// "New: {path} · {free} free".
    pub free: String,
    /// The one problem in plain words, or `None` ("Enough space").
    pub problem: Option<String>,
}

pub fn problem_text(p: MoveProblem) -> &'static str {
    match p {
        MoveProblem::NotLocal | MoveProblem::TooLong => {
            "Choose a folder on a drive in this computer, with a short path."
        }
        MoveProblem::InsideFolder => {
            "This folder is one of your searched folders. Choose another place."
        }
        // [New] (no brief copy): the app's own folder or the current index.
        MoveProblem::InsideApp | MoveProblem::SameAsCurrent => {
            "Choose a folder outside Selene Retrieval's own folders."
        }
        MoveProblem::NotEmpty => {
            "Choose an empty folder, or one without a “SeleneRetrieval Index” folder in it."
        }
        MoveProblem::NotWritable if cfg!(target_os = "macos") => {
            "macOS didn't allow Selene Retrieval to write there. Choose another place."
        }
        MoveProblem::NotWritable => {
            "Windows didn't allow Selene Retrieval to write there. Choose another place."
        }
    }
}

pub fn check_view(c: &IndexMoveCheck) -> MoveCheckView {
    let problem = match (c.problem, c.enough_space) {
        (Some(p), _) => Some(problem_text(p).to_owned()),
        (None, false) => Some(format!(
            "Not enough space on {}. About {} is needed.",
            c.drive,
            size_text(c.need_bytes)
        )),
        (None, true) => None,
    };
    MoveCheckView {
        ok: c.ok && c.enough_space && problem.is_none(),
        index_path: c.index_path.clone(),
        free: size_text(c.free_bytes),
        problem,
    }
}

fn parse<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T, UiError> {
    serde_json::from_value(v).map_err(|e| {
        UiError::from(Error::new(ErrorCode::Internal).with_detail(format!("unexpected reply: {e}")))
    })
}

pub async fn status<R: Rpc>(rpc: &R) -> Result<IndexLocationView, UiError> {
    let v = rpc
        .call(method::INDEX_LOCATION_STATUS, json!({}))
        .await
        .map_err(UiError::from)?;
    let s: IndexLocationStatus = parse(v)?;
    Ok(IndexLocationView {
        path: s.path,
        is_default: s.is_default,
        size: size_text(s.size_bytes),
        free: size_text(s.free_bytes),
    })
}

/// Live check after the folder picker (never on a timer).
pub async fn check<R: Rpc>(rpc: &R, target: &str) -> Result<MoveCheckView, UiError> {
    let t = target.trim();
    if t.is_empty() || !std::path::Path::new(t).is_absolute() {
        return Ok(MoveCheckView {
            ok: false,
            index_path: String::new(),
            free: String::new(),
            problem: Some(problem_text(MoveProblem::NotLocal).to_owned()),
        });
    }
    let v = rpc
        .call(method::INDEX_LOCATION_CHECK, json!({ "target": t }))
        .await
        .map_err(UiError::from)?;
    Ok(check_view(&parse::<IndexMoveCheck>(v)?))
}

/// [Move]: the service saves the plan, drains and restarts; progress is pushed.
pub async fn start<R: Rpc>(rpc: &R, target: &str) -> Result<(), UiError> {
    rpc.call(
        method::INDEX_LOCATION_MOVE,
        json!({ "target": target.trim() }),
    )
    .await
    .map(|_| ())
    .map_err(UiError::from)
}

pub async fn cancel<R: Rpc>(rpc: &R) -> Result<bool, UiError> {
    let v = rpc
        .call(method::INDEX_LOCATION_CANCEL, json!({}))
        .await
        .map_err(UiError::from)?;
    Ok(v.get("cancelled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake::FakeRpc;

    fn chk(problem: Option<MoveProblem>, enough: bool) -> IndexMoveCheck {
        IndexMoveCheck {
            ok: problem.is_none() && enough,
            problem,
            index_path: r"D:\SeleneRetrieval Index".into(),
            drive: "D:".into(),
            need_bytes: 2_500_000_000,
            free_bytes: 10_000_000_000,
            enough_space: enough,
        }
    }

    #[test]
    fn checks_read_as_the_brief_says() {
        assert_eq!(check_view(&chk(None, true)).problem, None);
        assert!(check_view(&chk(None, true)).ok);
        assert_eq!(
            check_view(&chk(None, false)).problem.as_deref(),
            Some("Not enough space on D:. About 2.5 GB is needed.")
        );
        assert_eq!(
            check_view(&chk(Some(MoveProblem::InsideFolder), true))
                .problem
                .as_deref(),
            Some("This folder is one of your searched folders. Choose another place.")
        );
        assert_eq!(
            check_view(&chk(Some(MoveProblem::TooLong), true))
                .problem
                .as_deref(),
            Some("Choose a folder on a drive in this computer, with a short path.")
        );
        assert!(!check_view(&chk(Some(MoveProblem::NotEmpty), true)).ok);
        assert_eq!(size_text(820_000_000), "820 MB");
    }

    #[tokio::test]
    async fn relative_targets_never_reach_the_service() {
        let rpc = FakeRpc::default();
        let c = check(&rpc, r"relative\dir").await.unwrap();
        assert!(!c.ok && c.problem.is_some());
        assert!(rpc.calls().is_empty());
        start(&rpc, r"D:\Data").await.unwrap();
        assert_eq!(
            rpc.calls()[0],
            (
                "index_location.move".to_owned(),
                json!({"target": r"D:\Data"})
            )
        );
    }
}
