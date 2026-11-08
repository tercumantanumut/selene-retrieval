//! "Create a folder for me" (Amendment 13, `docs/07-default-folder-plan.md`).
//!
//! One click makes `Documents\Selene Retrieval` (path rules in [`sr_client::default_folder`]),
//! adds it with the **existing** privileged `folders.add` (the service validates it like any folder
//! the user picks: canonical root, dangerous roots, policy roots, overlap, network probe), and the
//! command then opens it in Explorer. Security:
//! * the path is computed here, in the app, from the known-folder API; the UI never sends one;
//! * the app makes the folder as the user and refuses when it runs elevated; the background
//!   service never writes into user folders;
//! * an organisation that manages the allowed folder roots gets no offer and a refusal (fail
//!   closed: the app only has the roots as display text);
//! * no new pipe method: the R31 privileged set is unchanged.
//!
//! Plain logic over [`Rpc`] with an injectable Documents folder, unit-tested with the fake service;
//! `commands.rs` only resolves Documents, collects the added folders and opens Explorer.
#![allow(clippy::result_large_err)] // `UiError` is the Tauri IPC error (see commands.rs).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use specta::Type;
use sr_client::default_folder::{self as df, Made, MakeError};
use sr_common::error::{Error, ErrorCode};

use crate::daemon::{Rpc, method};
use crate::dto::{AppView, FolderState, UiAction, UiError};

/// Longest wait for the file system (a Documents folder on a dead network share can hang); the
/// same limit as the service's network probe.
pub const FS_TIMEOUT: Duration = Duration::from_secs(5);

/// One catch-up check of a newly added default folder, this long after the add. The live watcher
/// arms a new folder within its 2 s probe; a file the user drops into the just-opened folder
/// before that (after the first, empty scan) would otherwise wait for the hourly full check
/// (R45). The service's `folders.sync` is cheap for a new folder.
pub const CATCH_UP_AFTER: Duration = Duration::from_secs(3);

/// Asks the service to check `folder_id` once more (see [`CATCH_UP_AFTER`]).
pub async fn catch_up<R: Rpc>(rpc: &R, folder_id: &str) {
    let _ = rpc
        .call(method::FOLDERS_SYNC, json!({ "folder_id": folder_id }))
        .await;
}

/// One folder already in Selene Retrieval (from the pushed view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub id: String,
    pub path: PathBuf,
}

/// The added folders with a known path (a folder being removed no longer counts).
pub fn added_folders(view: &AppView) -> Vec<Added> {
    view.folders
        .iter()
        .filter(|f| f.state != FolderState::Removing)
        .filter_map(|f| {
            f.path.as_deref().map(|p| Added {
                id: f.id.clone(),
                path: PathBuf::from(p),
            })
        })
        .collect()
}

/// Whether the organisation manages the allowed folder roots (then the offer is hidden).
pub fn roots_managed(view: &AppView) -> bool {
    view.policy
        .is_managed(sr_common::config::policy::paths::FOLDER_ROOTS)
}

/// Whether to show [Create a folder for me], and which folder it already is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DefaultFolderOffer {
    /// Show [Create a folder for me]: Documents was found, the folder isn't in Selene Retrieval
    /// yet (nor inside an added folder), and no organisation policy limits where folders go.
    pub available: bool,
    /// The default folder is already in Selene Retrieval: its id (its details page says "Anything
    /// you put here becomes searchable automatically").
    pub folder_id: Option<String>,
    /// The folder's name ("Selene Retrieval"), for the button's description.
    pub name: String,
}

/// What [Create a folder for me] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DefaultFolderResult {
    /// The folder whose details to show: the new folder, or the added folder that already
    /// contains it (e.g. Documents itself).
    pub folder_id: String,
    pub name: String,
    pub path: String,
    /// The folder was made now (`false`: it was already there and is used as it is).
    pub created: bool,
    /// It was already in Selene Retrieval; nothing was added.
    pub already_added: bool,
    /// File Explorer was opened on it.
    pub opened: bool,
    /// It is inside OneDrive (files kept only online there are skipped, with the usual note).
    pub in_onedrive: bool,
}

/// The added folder that is the default folder, or that contains it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing<'a> {
    Default(&'a Added),
    Covered(&'a Added),
}

pub fn existing<'a>(docs: &Path, added: &'a [Added]) -> Option<Existing<'a>> {
    if let Some(a) = added.iter().find(|a| df::is_default_folder(docs, &a.path)) {
        return Some(Existing::Default(a));
    }
    let target = docs.join(df::FOLDER_NAME);
    added
        .iter()
        .find(|a| df::is_within(&target, &a.path))
        .map(Existing::Covered)
}

pub fn offer(docs: Option<&Path>, added: &[Added], roots_managed: bool) -> DefaultFolderOffer {
    let found = docs.and_then(|d| existing(d, added));
    DefaultFolderOffer {
        available: docs.is_some() && found.is_none() && !roots_managed,
        folder_id: match found {
            Some(Existing::Default(a)) => Some(a.id.clone()),
            _ => None,
        },
        name: df::FOLDER_NAME.to_owned(),
    }
}

/// Whether `path` is inside one of the OneDrive folders (`roots`: the `OneDrive*` variables;
/// display only, never a security decision).
pub fn in_onedrive(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| df::is_within(path, r))
}

/// The user's OneDrive folders as Windows announces them.
pub fn onedrive_roots() -> Vec<PathBuf> {
    ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"]
        .iter()
        .filter_map(std::env::var_os)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Plain errors: what happened and the one thing that always works (choose a folder yourself).

const CHOOSE: &str = "Choose a folder";

fn choose_error(title: &str, body: &str, details: &str) -> UiError {
    UiError::new(title, body, UiAction::ChooseFolder, CHOOSE, details)
}

pub fn err_documents_missing() -> UiError {
    choose_error(
        "Your Documents folder can't be found",
        "Choose a folder you already have instead.",
        "DefaultFolder · DocumentsMissing",
    )
}

pub fn err_access_denied() -> UiError {
    choose_error(
        "Couldn't create the folder",
        "Windows didn't let Selene Retrieval create a folder in Documents. If Windows Security showed \
         “Unauthorized changes blocked”, its ransomware protection stopped it. Choose a folder you \
         already have instead.",
        "DefaultFolder · AccessDenied",
    )
}

pub fn err_disk_full() -> UiError {
    choose_error(
        "Not enough disk space",
        "There isn't enough space to create a folder in Documents. Free up some space, or choose a \
         folder you already have.",
        "DefaultFolder · DiskFull",
    )
}

pub fn err_not_responding() -> UiError {
    choose_error(
        "Your Documents folder isn't responding",
        "It may be on a network drive that can't be reached right now. Choose a folder you already \
         have instead.",
        "DefaultFolder · Timeout",
    )
}

pub fn err_elevated() -> UiError {
    choose_error(
        "Selene Retrieval is running as administrator",
        "Folders it makes now would belong to the administrator account. Open Selene Retrieval \
         normally, or choose a folder you already have.",
        "DefaultFolder · Elevated",
    )
}

pub fn err_other() -> UiError {
    choose_error(
        "Couldn't create the folder",
        "Something went wrong while making the folder in Documents. Choose a folder you already \
         have instead.",
        "DefaultFolder · Other",
    )
}

/// The policy refusal (the service's own renderer text), with the fallback action.
pub fn err_policy() -> UiError {
    let mut u = UiError::from(Error::new(ErrorCode::ManagedByPolicy));
    u.action = UiAction::ChooseFolder;
    u.action_label = CHOOSE.to_owned();
    u
}

fn make_error(e: MakeError) -> UiError {
    match e {
        MakeError::DocumentsMissing => err_documents_missing(),
        MakeError::AccessDenied => err_access_denied(),
        MakeError::DiskFull => err_disk_full(),
        MakeError::NoFreeName | MakeError::Other => err_other(),
    }
}

/// A service refusal keeps its words; without an action of its own (or with "Choose a subfolder"
/// of a folder we chose, not the user) it offers [Choose a folder].
fn add_error(e: Error) -> UiError {
    let mut u = UiError::from(e);
    if matches!(u.action, UiAction::None | UiAction::ChooseSubfolder { .. }) {
        u.action = UiAction::ChooseFolder;
        u.action_label = CHOOSE.to_owned();
    }
    u
}

/// Runs `f` off the async threads with a time limit (the thread is left to finish on its own).
async fn blocking<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, UiError> {
    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(f)).await {
        Err(_) => Err(err_not_responding()),
        Ok(Err(_)) => Err(err_other()),
        Ok(Ok(v)) => Ok(v),
    }
}

async fn make(docs: &Path, timeout: Duration) -> Result<Made, UiError> {
    let d = docs.to_path_buf();
    blocking(timeout, move || df::make(&d))
        .await?
        .map_err(make_error)
}

fn name_of(path: &Path) -> String {
    path.file_name().map_or_else(
        || df::FOLDER_NAME.to_owned(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn result(
    folder_id: &str,
    made_path: &Path,
    created: bool,
    already_added: bool,
) -> DefaultFolderResult {
    DefaultFolderResult {
        folder_id: folder_id.to_owned(),
        name: name_of(made_path),
        path: made_path.to_string_lossy().into_owned(),
        created,
        already_added,
        opened: false,
        in_onedrive: false,
    }
}

/// What the app knows when the user clicks.
#[derive(Debug, Clone)]
pub struct CreateInput<'a> {
    /// Documents from the known-folder API (`None`: Windows didn't give one).
    pub docs: Option<&'a Path>,
    pub added: &'a [Added],
    pub roots_managed: bool,
    pub elevated: bool,
    pub timeout: Duration,
}

/// [Create a folder for me]: finds or makes the folder and adds it (see the module docs). Returns
/// the result and, when the service added it now, its `FolderInfo` reply (merged into the view).
pub async fn create<R: Rpc>(
    rpc: &R,
    input: CreateInput<'_>,
) -> Result<(DefaultFolderResult, Option<Value>), UiError> {
    if input.roots_managed {
        return Err(err_policy());
    }
    let Some(docs) = input.docs else {
        return Err(err_documents_missing());
    };
    match existing(docs, input.added) {
        // Already in Selene Retrieval: nothing is made or added (opening it is the command's job).
        Some(Existing::Default(a)) => return Ok((result(&a.id, &a.path, false, true), None)),
        // Inside an added folder (e.g. Documents itself): it is searchable already. The folder is
        // still made so the user has the place they asked for; its parent's details are shown.
        Some(Existing::Covered(parent)) => {
            if input.elevated {
                return Err(err_elevated());
            }
            let m = make(docs, input.timeout).await?;
            return Ok((result(&parent.id, &m.path, m.created, true), None));
        }
        None => {}
    }
    if input.elevated {
        return Err(err_elevated());
    }
    let m = make(docs, input.timeout).await?;
    let path = m.path.to_string_lossy().into_owned();
    match rpc.call(method::FOLDERS_ADD, json!({ "path": path })).await {
        Ok(info) => {
            let id = info
                .get("folder_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let mut r = result(&id, &m.path, m.created, false);
            if let Some(n) = info.get("name").and_then(Value::as_str) {
                n.clone_into(&mut r.name);
            }
            Ok((r, Some(info)))
        }
        // Our view didn't know the folder's path yet: it is already there (or inside an added
        // folder). Ask the service which one and show it; nothing was added.
        Err(e) if e.code == ErrorCode::FolderOverlap => match already_in(rpc, &m.path).await {
            Some(id) => Ok((result(&id, &m.path, m.created, true), None)),
            None => Err(add_error(e)),
        },
        Err(e) => {
            if m.created {
                // Only a folder we made a moment ago, and only while it is still empty
                // (`remove_dir` never deletes contents).
                let p = m.path.clone();
                let _ = blocking(input.timeout, move || std::fs::remove_dir(p)).await;
            }
            Err(add_error(e))
        }
    }
}

/// The added folder (from `folders.list`) that is `path` or contains it.
async fn already_in<R: Rpc>(rpc: &R, path: &Path) -> Option<String> {
    let list = rpc.call(method::FOLDERS_LIST, Value::Null).await.ok()?;
    list.as_array()?
        .iter()
        .filter(|f| f.get("removing").and_then(Value::as_bool) != Some(true))
        .find_map(|f| {
            let p = f.get("path").and_then(Value::as_str)?;
            df::is_within(path, Path::new(p))
                .then(|| {
                    f.get("folder_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .flatten()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake::FakeRpc;

    fn tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("sr-dfa")
            .tempdir()
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn input<'a>(docs: &'a Path, added: &'a [Added]) -> CreateInput<'a> {
        CreateInput {
            docs: Some(docs),
            added,
            roots_managed: false,
            elevated: false,
            timeout: FS_TIMEOUT,
        }
    }

    fn info(id: &str, path: &Path) -> Value {
        json!({"folder_id": id, "name": "Selene Retrieval", "path": path.to_string_lossy()})
    }

    #[tokio::test]
    async fn one_click_makes_the_folder_and_adds_it_once() {
        let t = tmp();
        let docs = t.path().join("Documents");
        std::fs::create_dir(&docs).unwrap_or_else(|e| panic!("{e}"));
        let want = docs.join("Selene Retrieval");
        let rpc = FakeRpc::default();
        rpc.reply(Ok(info("01JNEW", &want)));
        let (r, reply) = create(&rpc, input(&docs, &[])).await.unwrap();
        assert!(want.is_dir());
        assert_eq!(
            rpc.calls(),
            vec![(
                "folders.add".to_owned(),
                json!({"path": want.to_string_lossy()})
            )],
            "exactly one folders.add with the computed path"
        );
        assert_eq!(r.folder_id, "01JNEW");
        assert!(r.created && !r.already_added && !r.opened);
        assert_eq!(r.name, "Selene Retrieval");
        assert!(reply.is_some());

        // Second click: it's in the view now → nothing is made or added.
        let added = vec![Added {
            id: "01JNEW".into(),
            path: want.clone(),
        }];
        let (again, reply) = create(&rpc, input(&docs, &added)).await.unwrap();
        assert_eq!(rpc.calls().len(), 1, "no second folders.add");
        assert!(again.already_added && !again.created);
        assert_eq!(again.folder_id, "01JNEW");
        assert!(reply.is_none());
        assert_eq!(
            offer(Some(docs.as_path()), &added, false),
            DefaultFolderOffer {
                available: false,
                folder_id: Some("01JNEW".into()),
                name: "Selene Retrieval".into()
            }
        );
    }

    #[tokio::test]
    async fn an_existing_folder_is_added_as_it_is() {
        let t = tmp();
        let docs = t.path().to_path_buf();
        let dir = docs.join("Selene Retrieval");
        std::fs::create_dir(&dir).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.join("keep.txt"), "mine").unwrap_or_else(|e| panic!("{e}"));
        let rpc = FakeRpc::default();
        rpc.reply(Ok(info("01JOLD", &dir)));
        let (r, _) = create(&rpc, input(&docs, &[])).await.unwrap();
        assert!(!r.created && !r.already_added);
        assert_eq!(
            std::fs::read_to_string(dir.join("keep.txt")).unwrap_or_default(),
            "mine"
        );
    }

    #[tokio::test]
    async fn a_file_with_the_name_uses_a_numbered_folder_and_then_stays_idempotent() {
        let t = tmp();
        let docs = t.path().to_path_buf();
        std::fs::write(docs.join("Selene Retrieval"), "a file").unwrap_or_else(|e| panic!("{e}"));
        let two = docs.join("Selene Retrieval (2)");
        let rpc = FakeRpc::default();
        rpc.reply(Ok(info("01JTWO", &two)));
        let (r, _) = create(&rpc, input(&docs, &[])).await.unwrap();
        assert_eq!(Path::new(&r.path), two);
        assert!(docs.join("Selene Retrieval").is_file());
        // Even if the file is deleted later, the numbered folder counts as the default folder.
        std::fs::remove_file(docs.join("Selene Retrieval")).unwrap_or_else(|e| panic!("{e}"));
        let added = vec![Added {
            id: "01JTWO".into(),
            path: two.clone(),
        }];
        let (again, _) = create(&rpc, input(&docs, &added)).await.unwrap();
        assert!(again.already_added);
        assert_eq!(rpc.calls().len(), 1);
        assert!(!docs.join("Selene Retrieval").exists(), "no second folder");
    }

    #[tokio::test]
    async fn documents_already_added_covers_it() {
        let t = tmp();
        let docs = t.path().to_path_buf();
        let added = vec![Added {
            id: "01JDOCS".into(),
            path: docs.clone(),
        }];
        assert!(
            !offer(Some(docs.as_path()), &added, false).available,
            "no offer: it's searchable already"
        );
        let rpc = FakeRpc::default();
        let (r, _) = create(&rpc, input(&docs, &added)).await.unwrap();
        assert!(
            rpc.calls().is_empty(),
            "nothing is added (it would overlap)"
        );
        assert_eq!(r.folder_id, "01JDOCS", "the parent's details are shown");
        assert!(r.already_added && r.created);
        assert!(docs.join("Selene Retrieval").is_dir());
    }

    #[tokio::test]
    async fn policy_and_elevation_refuse_before_touching_the_disk() {
        let t = tmp();
        let docs = t.path().to_path_buf();
        let rpc = FakeRpc::default();
        let mut i = input(&docs, &[]);
        i.roots_managed = true;
        let e = create(&rpc, i).await.unwrap_err();
        assert_eq!(e.title, "Managed by your organisation");
        assert!(
            e.details.starts_with("ManagedByPolicy"),
            "the roots hint keys on it"
        );
        assert_eq!(e.action, UiAction::ChooseFolder);
        assert!(!offer(Some(docs.as_path()), &[], true).available);

        let mut i = input(&docs, &[]);
        i.elevated = true;
        let e = create(&rpc, i).await.unwrap_err();
        assert_eq!(e.action, UiAction::ChooseFolder);
        assert!(rpc.calls().is_empty());
        assert!(!docs.join("Selene Retrieval").exists(), "nothing made");
    }

    #[tokio::test]
    async fn a_refused_add_removes_only_the_empty_folder_we_made() {
        let t = tmp();
        let docs = t.path().to_path_buf();
        let rpc = FakeRpc::default();
        rpc.reply(Err(
            Error::new(ErrorCode::ManagedByPolicy).with_name("Selene Retrieval")
        ));
        let e = create(&rpc, input(&docs, &[])).await.unwrap_err();
        assert_eq!(e.action, UiAction::ChooseFolder, "the fallback action");
        assert!(
            !docs.join("Selene Retrieval").exists(),
            "the empty folder we made is gone"
        );

        // A folder that was already there (with the user's files) is never removed.
        let dir = docs.join("Selene Retrieval");
        std::fs::create_dir(&dir).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.join("a.txt"), "x").unwrap_or_else(|e| panic!("{e}"));
        rpc.reply(Err(Error::new(ErrorCode::PathRefused).with_path(&dir)));
        let e = create(&rpc, input(&docs, &[])).await.unwrap_err();
        assert_eq!(
            e.action,
            UiAction::ChooseFolder,
            "not 'choose a subfolder' of our folder"
        );
        assert!(dir.join("a.txt").is_file());

        // A retryable service problem keeps its own action.
        rpc.reply(Err(Error::new(ErrorCode::ServiceUnavailable)));
        let e = create(&rpc, input(&docs, &[])).await.unwrap_err();
        assert_eq!(e.action, UiAction::RestartApp);
    }

    #[tokio::test]
    async fn overlap_unknown_to_the_view_resolves_to_the_added_folder() {
        let t = tmp();
        let docs = t.path().to_path_buf();
        let rpc = FakeRpc::default();
        rpc.reply(Err(Error::new(ErrorCode::FolderOverlap)));
        rpc.reply(Ok(
            json!([{"folder_id": "01JDOCS", "path": docs.to_string_lossy()}]),
        ));
        let (r, reply) = create(&rpc, input(&docs, &[])).await.unwrap();
        assert_eq!(r.folder_id, "01JDOCS");
        assert!(r.already_added && reply.is_none());
        assert_eq!(rpc.calls()[1].0, "folders.list");
    }

    #[tokio::test]
    async fn missing_or_unknown_documents() {
        let t = tmp();
        let rpc = FakeRpc::default();
        let mut i = input(t.path(), &[]);
        i.docs = None;
        assert_eq!(create(&rpc, i).await.unwrap_err(), err_documents_missing());
        let gone = t.path().join("gone");
        assert_eq!(
            create(&rpc, input(&gone, &[])).await.unwrap_err(),
            err_documents_missing()
        );
        assert!(!gone.exists());
        assert!(!offer(None, &[], false).available);
        assert!(offer(Some(t.path()), &[], false).available);
        assert!(rpc.calls().is_empty());
    }

    #[tokio::test]
    async fn a_hung_documents_folder_times_out_plainly() {
        let e = blocking(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_millis(500));
        })
        .await
        .unwrap_err();
        assert_eq!(e, err_not_responding());
    }

    #[tokio::test]
    async fn catch_up_asks_for_one_check_of_that_folder() {
        let rpc = FakeRpc::default();
        catch_up(&rpc, "01JNEW").await;
        assert_eq!(
            rpc.calls(),
            vec![("folders.sync".to_owned(), json!({"folder_id": "01JNEW"}))]
        );
        assert!(
            CATCH_UP_AFTER > Duration::from_secs(2),
            "after the watcher's 2 s probe"
        );
    }

    #[test]
    fn every_error_has_one_plain_action() {
        for e in [
            err_documents_missing(),
            err_access_denied(),
            err_disk_full(),
            err_not_responding(),
            err_elevated(),
            err_other(),
            err_policy(),
        ] {
            assert_eq!(e.action, UiAction::ChooseFolder, "{}", e.title);
            assert_eq!(e.action_label, "Choose a folder");
            assert!(!e.title.is_empty() && !e.body.is_empty());
            for jargon in ["index", "daemon", "MCP", "CFA", "Controlled Folder"] {
                assert!(!e.body.contains(jargon), "{jargon} in {}", e.body);
            }
        }
        assert!(err_access_denied().body.contains("ransomware protection"));
    }

    /// Live check (docs/07 §7): the app's real create-and-add path against a **debug** daemon the
    /// tester started with its own `--data-dir` and `SR_TRUST_ANY_APP=1`, and a temp Documents.
    /// Never the owner's install (connect-only, and the handshake pid must be the started
    /// daemon's, Amendment 11.11) and never the real Documents (must be under the temp folder).
    /// Env: `SR_LIVE_DATA_DIR`, `SR_LIVE_DAEMON_PID`, `SR_LIVE_DOCS`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "live: needs a test daemon (docs/07 §7)"]
    async fn live_create_default_folder() {
        let var = |n: &str| std::env::var(n).unwrap_or_else(|_| panic!("{n} not set"));
        let data = PathBuf::from(var("SR_LIVE_DATA_DIR"));
        let pid: u32 = var("SR_LIVE_DAEMON_PID").parse().unwrap();
        let docs = PathBuf::from(var("SR_LIVE_DOCS"));
        let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(
            std::fs::canonicalize(&docs).unwrap().starts_with(&temp),
            "SR_LIVE_DOCS must be a temp folder"
        );
        let mut opts = sr_client::AttachOptions::new(
            sr_common::protocol::ConnKind::App,
            "live-default-folder",
        );
        opts.data_dir = Some(data);
        opts.launch = false;
        let probe = sr_client::AppClient::connect(&opts).await.unwrap();
        assert_eq!(
            probe.daemon_pid, pid,
            "not the daemon this run started; stopping"
        );
        drop(probe);
        let link = crate::daemon::DaemonLink::with_options(opts);
        let input = |added: &'static [Added]| CreateInput {
            docs: Some(Box::leak(docs.clone().into_boxed_path())),
            added,
            roots_managed: false,
            elevated: sr_client::win::process_elevated().unwrap_or(true),
            timeout: FS_TIMEOUT,
        };

        // Click 1.
        let (r, reply) = create(&link, input(&[])).await.unwrap();
        eprintln!("LIVE click 1: {r:?}");
        assert!(r.created && !r.already_added && reply.is_some());
        let dir = PathBuf::from(&r.path);
        assert!(dir.is_dir() && df::is_default_folder(&docs, &dir));
        // The app's request limit is 30 s: wait for idle in 25 s steps.
        let mut idle = false;
        for _ in 0..8 {
            let v = link
                .call("engine.wait_idle", json!({ "timeout_ms": 25_000 }))
                .await
                .unwrap();
            idle = v["idle"].as_bool() == Some(true);
            if idle {
                break;
            }
        }
        eprintln!("LIVE indexed (idle): {idle}");
        assert!(idle);

        // Worst case: a file dropped in at once (before the watcher armed the new folder, R45),
        // then the command's one catch-up check, exactly as `create_default_folder` schedules it.
        std::fs::write(
            dir.join("live.md"),
            "# Live

The livedefaultfoldertoken is here.
",
        )
        .unwrap();
        tokio::time::sleep(CATCH_UP_AFTER).await;
        catch_up(&link, &r.folder_id).await;
        let search = |q: &'static str, rel: &'static str| {
            let link = &link;
            async move {
                let t0 = std::time::Instant::now();
                loop {
                    let v = link
                        .call(
                            "search",
                            json!({ "query": q, "max_results": 10, "min_score": 0.0 }),
                        )
                        .await
                        .unwrap();
                    let hit = v["hits"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|h| h["rel_path"].as_str() == Some(rel));
                    if hit || t0.elapsed() > Duration::from_secs(60) {
                        return (hit, t0.elapsed());
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        };
        let (found, took) = search("livedefaultfoldertoken", "live.md").await;
        eprintln!("LIVE file dropped at once searchable after the catch-up: {found} ({took:?})");
        assert!(found);
        // Later files: the live watcher alone (no sync call).
        std::fs::write(
            dir.join("later.md"),
            "# Later

The laterwatchedtoken is here.
",
        )
        .unwrap();
        let (found, took) = search("laterwatchedtoken", "later.md").await;
        eprintln!("LIVE later file searchable by the watcher alone: {found} ({took:?})");
        assert!(found);

        // Click 2: idempotent (the view now lists the folder).
        let list = link.call("folders.list", Value::Null).await.unwrap();
        let added: Vec<Added> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| {
                Some(Added {
                    id: f["folder_id"].as_str()?.to_owned(),
                    path: PathBuf::from(f["path"].as_str()?),
                })
            })
            .collect();
        eprintln!("LIVE folders: {}", added.len());
        let (again, reply) = create(&link, input(Box::leak(added.into_boxed_slice())))
            .await
            .unwrap();
        eprintln!("LIVE click 2: {again:?}");
        assert!(again.already_added && !again.created && reply.is_none());
        assert_eq!(again.folder_id, r.folder_id);
        let entries = std::fs::read_dir(&docs).unwrap().count();
        assert_eq!(entries, 1, "still exactly one folder in Documents");
    }

    #[test]
    fn onedrive_and_added_list() {
        let roots = vec![PathBuf::from(r"C:\Users\Ana\OneDrive")];
        assert!(in_onedrive(
            Path::new(r"C:\Users\Ana\OneDrive\Documents\Selene Retrieval"),
            &roots
        ));
        assert!(!in_onedrive(
            Path::new(r"C:\Users\Ana\Documents\Selene Retrieval"),
            &roots
        ));
        assert!(!in_onedrive(Path::new(r"C:\Users\Ana\OneDriveX\a"), &roots));
    }
}
