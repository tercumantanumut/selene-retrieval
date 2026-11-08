//! Hidden `sr-daemon admin …` CLI (U1 / R-UX1: support and IT only; M1 is an internal preview; the
//! first public build adds folders in the app wizard). It is a pipe client like the app: it
//! attaches (launching the daemon if needed) and calls app-RPC methods, so the daemon stays the
//! only owner of the stores.
//!
//! ```text
//! sr-daemon admin [--data-dir <dir>] folder add <path> [--name <name>] [--wait]
//!                                    folder remove <folder-id>
//!                                    folder list
//!                                    sync [<folder-id>] [--wait]
//!                                    status | diagnostics
//!                                    search <query…> [--max <n>]
//!                                    wait-idle [--timeout-ms <n>]
//!                                    shutdown
//! ```

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use sr_client::{AppClient, AttachOptions};
use sr_common::error::Error;
use sr_common::protocol::ConnKind;
use sr_engine::rpc;

/// Parsed admin command.
#[derive(Debug, Clone, PartialEq)]
pub enum AdminCmd {
    FolderAdd {
        path: String,
        name: Option<String>,
        wait: bool,
    },
    FolderRemove(String),
    FolderList,
    Sync {
        folder: Option<String>,
        wait: bool,
    },
    Status,
    Diagnostics,
    Search {
        query: String,
        max: Option<u32>,
    },
    WaitIdle(u64),
    Shutdown,
}

/// `admin` arguments: the data dir and the command.
#[derive(Debug, Clone, PartialEq)]
pub struct AdminArgs {
    pub data_dir: Option<PathBuf>,
    pub cmd: AdminCmd,
}

pub const USAGE: &str = "usage: sr-daemon admin [--data-dir <dir>] (folder add <path> [--name <n>] [--wait] | folder remove <id> | folder list | sync [<id>] [--wait] | status | diagnostics | search <query> [--max <n>] | wait-idle [--timeout-ms <n>] | shutdown)";

pub fn parse(args: &[String]) -> Result<AdminArgs, String> {
    let mut data_dir = None;
    let mut rest: Vec<String> = Vec::new();
    let mut flags: std::collections::HashMap<String, Option<String>> = Default::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(it.next().ok_or(USAGE)?)),
            "--name" | "--max" | "--timeout-ms" => {
                flags.insert(a.clone(), Some(it.next().ok_or(USAGE)?.clone()));
            }
            "--wait" => {
                flags.insert(a.clone(), None);
            }
            _ => rest.push(a.clone()),
        }
    }
    let wait = flags.contains_key("--wait");
    let flag = |k: &str| flags.get(k).cloned().flatten();
    let words: Vec<&str> = rest.iter().map(String::as_str).collect();
    let cmd = match words.as_slice() {
        ["folder", "add", path] => AdminCmd::FolderAdd {
            path: (*path).to_string(),
            name: flag("--name"),
            wait,
        },
        ["folder", "remove", id] => AdminCmd::FolderRemove((*id).to_string()),
        ["folder", "list"] | ["folders"] => AdminCmd::FolderList,
        ["sync"] => AdminCmd::Sync { folder: None, wait },
        ["sync", id] => AdminCmd::Sync {
            folder: Some((*id).to_string()),
            wait,
        },
        ["status"] => AdminCmd::Status,
        ["diagnostics"] => AdminCmd::Diagnostics,
        ["search", q @ ..] if !q.is_empty() => AdminCmd::Search {
            query: q.join(" "),
            max: flag("--max").and_then(|m| m.parse().ok()),
        },
        ["wait-idle"] => AdminCmd::WaitIdle(
            flag("--timeout-ms")
                .and_then(|m| m.parse().ok())
                .unwrap_or(600_000),
        ),
        ["shutdown"] => AdminCmd::Shutdown,
        _ => return Err(USAGE.into()),
    };
    Ok(AdminArgs { data_dir, cmd })
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

async fn wait_idle(c: &AppClient, ms: u64) -> Result<Value, Error> {
    c.request(rpc::WAIT_IDLE, json!({ "timeout_ms": ms })).await
}

/// Run one admin command against the daemon. Output: JSON on stdout; errors as plain text.
pub async fn run(a: AdminArgs) -> Result<String, Error> {
    let mut opts = AttachOptions::new(ConnKind::App, "admin");
    opts.data_dir = a.data_dir.clone();
    opts.timeout = Duration::from_secs(30);
    let c = AppClient::connect(&opts).await?;
    let out = match a.cmd {
        AdminCmd::FolderAdd { path, name, wait } => {
            let v = c
                .request(rpc::FOLDERS_ADD, json!({ "path": path, "name": name }))
                .await?;
            if wait {
                wait_idle(&c, 30 * 60_000).await?;
                c.request(rpc::FOLDERS_LIST, Value::Null).await?
            } else {
                v
            }
        }
        AdminCmd::FolderRemove(id) => {
            c.request(rpc::FOLDERS_REMOVE, json!({ "folder_id": id }))
                .await?
        }
        AdminCmd::FolderList => c.request(rpc::FOLDERS_LIST, Value::Null).await?,
        AdminCmd::Sync { folder, wait } => {
            let v = c
                .request(rpc::FOLDERS_SYNC, json!({ "folder_id": folder }))
                .await?;
            if wait {
                wait_idle(&c, 30 * 60_000).await?;
            }
            v
        }
        AdminCmd::Status => c.request(rpc::STATUS, Value::Null).await?,
        AdminCmd::Diagnostics => c.request(rpc::DIAGNOSTICS, Value::Null).await?,
        AdminCmd::Search { query, max } => {
            c.request(rpc::SEARCH, json!({ "query": query, "max_results": max }))
                .await?
        }
        AdminCmd::WaitIdle(ms) => wait_idle(&c, ms).await?,
        AdminCmd::Shutdown => {
            c.shutdown(true).await?;
            json!({ "shutdown": true })
        }
    };
    Ok(pretty(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn parses_commands() {
        let a = parse(&s(&[
            "--data-dir",
            "C:\\d",
            "folder",
            "add",
            "C:\\x",
            "--wait",
        ]))
        .unwrap();
        assert_eq!(a.data_dir, Some(PathBuf::from("C:\\d")));
        assert_eq!(
            a.cmd,
            AdminCmd::FolderAdd {
                path: "C:\\x".into(),
                name: None,
                wait: true
            }
        );
        assert_eq!(
            parse(&s(&["folder", "list"])).unwrap().cmd,
            AdminCmd::FolderList
        );
        assert_eq!(
            parse(&s(&["search", "a", "b", "--max", "5"])).unwrap().cmd,
            AdminCmd::Search {
                query: "a b".into(),
                max: Some(5)
            }
        );
        assert!(parse(&s(&["folder", "explode"])).is_err());
    }
}
