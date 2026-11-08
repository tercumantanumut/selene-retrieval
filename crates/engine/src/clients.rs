//! Connected apps and per-client scopes (M4-E, page 29; decision 6 "per-client scopes default
//! All my folders").
//!
//! * A **client** is one stdio app seen over the pipe (`sr-bridge --client <id>`, normalised with
//!   [`sr_common::api::normalize_client_id`]) or one HTTP tool (`http-<ulid>`, one token each).
//! * Its **scope** is "All my folders" or a list of folder groups. [`Scope`] is what MCP tools
//!   enforce: a folder-id filter for `context_search`, `context_answer`, `list_folders` and
//!   `sync_folder`. Unknown or deleted groups grant nothing (fail closed, P1.7).
//! * Sessions and "last used" are tracked in memory; `last_seen_at` is written at most once a
//!   minute per client ([`TOUCH_EVERY_MS`]).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use sr_catalog::repo::activity::NewActivity;
use sr_catalog::repo::clients::{self, ClientRecord};
use sr_catalog::repo::groups;
use sr_common::api::{
    ActivityKind, ClientScope, ClientView, ClientsList, HTTP_CLIENT_PREFIX, Snippet,
    client_kind_of, default_client_name, normalize_client_id,
};
use sr_common::error::{ClientKind, Error, ErrorCode};
use sr_common::events::EngineEvent;
use sr_common::ids::FolderId;

use crate::EngineHandle;

/// `last_seen_at` is written at most this often per client.
pub const TOUCH_EVERY_MS: i64 = 60_000;
/// MCP server key in client configs (matches the app's Connect writers).
pub const SERVER_KEY: &str = "selene-retrieval";
/// The bridge next to the daemon.
pub const BRIDGE_EXE: &str = "sr-bridge.exe";

/// In-memory session bookkeeping.
#[derive(Debug, Default)]
pub(crate) struct ClientsState {
    sessions: HashMap<String, u32>,
    last_touch: HashMap<String, i64>,
}

/// What a client may see, resolved to folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// "All my folders".
    All,
    /// Only these folders (possibly none).
    Folders(HashSet<FolderId>),
}

impl Scope {
    pub fn allows(&self, f: FolderId) -> bool {
        match self {
            Scope::All => true,
            Scope::Folders(s) => s.contains(&f),
        }
    }

    /// Whether the client sees no folder at all.
    pub fn is_empty(&self) -> bool {
        matches!(self, Scope::Folders(s) if s.is_empty())
    }

    /// Apply the scope to a caller's folder list. `None` stays `None` for "All my folders";
    /// otherwise every named folder must be in scope (a folder outside it is refused exactly like
    /// an unknown one, `NotFound`), and `None` becomes the scope's own folders.
    pub fn restrict(
        &self,
        requested: Option<Vec<FolderId>>,
    ) -> Result<Option<Vec<FolderId>>, Error> {
        match (self, requested) {
            (Scope::All, r) => Ok(r),
            (Scope::Folders(set), Some(ids)) => {
                if ids.iter().any(|f| !set.contains(f)) {
                    return Err(Error::new(ErrorCode::NotFound).with_detail("unknown folder id"));
                }
                Ok(Some(ids))
            }
            (Scope::Folders(set), None) => {
                let mut v: Vec<FolderId> = set.iter().copied().collect();
                v.sort();
                Ok(Some(v))
            }
        }
    }
}

/// Agent text for a client whose scope resolves to no folder (contract §4).
pub const NO_ACCESS_TEXT: &str = "This app doesn't have access to any folders in Selene Retrieval. \
Ask the user to choose its folders in Selene Retrieval → Connected apps.";

fn kind_name(kind: ClientKind) -> &'static str {
    match kind {
        ClientKind::ClaudeDesktop => "claude",
        ClientKind::Cursor => "cursor",
        ClientKind::VsCode => "vscode",
        ClientKind::Other | ClientKind::Http => "other",
    }
}

/// JSON string escaping for snippet text (paths with backslashes).
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Setup text per app kind (OQ-44). No token ever appears in a stdio snippet; the HTTP one has the
/// literal placeholder `<token>` unless `token` is given (create / rotate replies only).
pub fn snippet_text(
    kind: ClientKind,
    bridge: &Path,
    port: Option<u16>,
    token: Option<&str>,
) -> Snippet {
    let cmd = json_str(&bridge.display().to_string());
    let (file_hint, text) = match kind {
        ClientKind::VsCode => (
            r".vscode\mcp.json or the user mcp.json".to_string(),
            format!(
                "{{\n  \"servers\": {{\n    \"{SERVER_KEY}\": {{\n      \"type\": \"stdio\",\n      \"command\": {cmd},\n      \"args\": [\"--client\", \"vscode\"]\n    }}\n  }}\n}}"
            ),
        ),
        ClientKind::Http => {
            let url = format!("http://127.0.0.1:{}/mcp", port.unwrap_or(0));
            let auth = format!("Bearer {}", token.unwrap_or("<token>"));
            (
                "your tool's MCP settings".to_string(),
                format!(
                    "{{\n  \"mcpServers\": {{\n    \"{SERVER_KEY}\": {{\n      \"type\": \"http\",\n      \"url\": {},\n      \"headers\": {{ \"Authorization\": {} }}\n    }}\n  }}\n}}",
                    json_str(&url),
                    json_str(&auth)
                ),
            )
        }
        other => {
            let hint = match other {
                ClientKind::ClaudeDesktop => "claude_desktop_config.json",
                ClientKind::Cursor => r"%USERPROFILE%\.cursor\mcp.json",
                _ => "your app's MCP settings",
            };
            (
                hint.to_string(),
                format!(
                    "{{\n  \"mcpServers\": {{\n    \"{SERVER_KEY}\": {{\n      \"command\": {cmd},\n      \"args\": [\"--client\", \"{}\"]\n    }}\n  }}\n}}",
                    kind_name(other)
                ),
            )
        }
    };
    Snippet {
        kind,
        file_hint,
        text,
    }
}

impl EngineHandle {
    fn sessions_of(&self, id: &str) -> u32 {
        self.inner()
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sessions
            .get(id)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn client_view(&self, r: ClientRecord) -> ClientView {
        let sessions_open = self.sessions_of(&r.id);
        ClientView {
            sessions_open,
            client_id: r.id,
            kind: r.kind,
            display_name: r.display_name,
            scope: r.scope,
            first_seen_at: r.first_seen_at,
            last_seen_at: r.last_seen_at,
            token_last4: r.token_hint,
        }
    }

    /// A pipe MCP session opened (`sr-mcp-server`). Records the app (first time: an activity
    /// line) and returns its canonical id.
    pub async fn client_session_opened(&self, raw_id: &str) -> String {
        let id = normalize_client_id(raw_id);
        *self
            .inner()
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sessions
            .entry(id.clone())
            .or_insert(0) += 1;
        self.touch_client(&id, true).await;
        self.inner().bus.publish(EngineEvent::ClientsChanged {
            client_id: Some(id.clone()),
        });
        id
    }

    /// The session ended.
    pub fn client_session_closed(&self, id: &str) {
        {
            let mut g = self
                .inner()
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(n) = g.sessions.get_mut(id) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    g.sessions.remove(id);
                }
            }
        }
        self.inner().bus.publish(EngineEvent::ClientsChanged {
            client_id: Some(id.to_string()),
        });
    }

    /// A tool call from `id`: refresh "last used" (throttled).
    pub async fn client_seen(&self, id: &str) {
        self.touch_client(id, false).await;
    }

    async fn touch_client(&self, id: &str, force: bool) {
        let now = self.inner().clock.now_ms();
        {
            let mut g = self
                .inner()
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let last = g.last_touch.get(id).copied().unwrap_or(i64::MIN);
            if !force && now.saturating_sub(last) < TOUCH_EVERY_MS {
                return;
            }
            g.last_touch.insert(id.to_string(), now);
        }
        let (cid, kind, name) = (id.to_string(), client_kind_of(id), default_client_name(id));
        // HTTP tools are created by `http.create_token`; a touch never creates one.
        if kind == ClientKind::Http {
            let cid2 = cid.clone();
            let _ = self
                .inner()
                .cat
                .write(move |c| {
                    c.execute(
                        "UPDATE clients SET last_seen_at = ?2 WHERE id = ?1",
                        rusqlite::params![cid2, now],
                    )
                    .map_err(sr_catalog::map_db_err)
                })
                .await;
        } else {
            let n2 = name.clone();
            match self
                .inner()
                .cat
                .write(move |c| clients::touch(c, &cid, kind, &n2, now))
                .await
            {
                Ok(true) => {
                    self.record_activity(
                        NewActivity::new(ActivityKind::ClientFirstSeen).client(kind, name),
                    )
                    .await;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::debug!(target: "sr_engine::clients", code = %e.code, "client not recorded");
                }
            }
        }
        if !force {
            self.inner().bus.publish(EngineEvent::ClientsChanged {
                client_id: Some(id.to_string()),
            });
        }
    }

    /// What `client_id` may see. An app with no row yet (it has never connected) gets "All my
    /// folders" (decision 6).
    pub async fn client_scope(&self, client_id: &str) -> Result<Scope, Error> {
        let id = client_id.to_string();
        self.inner()
            .cat
            .read_async(move |c| {
                let scope = clients::client(c, &id)?.map_or(ClientScope::All, |r| r.scope);
                match scope {
                    ClientScope::All => Ok(Scope::All),
                    ClientScope::Groups { group_ids } => {
                        let wanted: HashSet<_> = group_ids.into_iter().collect();
                        let set = groups::folder_groups(c)?
                            .into_iter()
                            .filter(|(_, g)| wanted.contains(g))
                            .map(|(f, _)| f)
                            .collect();
                        Ok(Scope::Folders(set))
                    }
                }
            })
            .await
    }

    /// `clients.list`.
    pub async fn clients_list(&self) -> Result<ClientsList, Error> {
        let rows = self.inner().cat.read_async(clients::clients).await?;
        Ok(ClientsList {
            clients: rows.into_iter().map(|r| self.client_view(r)).collect(),
            http: self.http_status(),
        })
    }

    /// `clients.set_scope` (privileged): unknown groups are refused, never silently dropped.
    pub async fn set_client_scope(
        &self,
        id: String,
        scope: ClientScope,
    ) -> Result<ClientView, Error> {
        let id2 = id.clone();
        let rec = self
            .inner()
            .cat
            .write(move |c| {
                if let ClientScope::Groups { group_ids } = &scope {
                    for g in group_ids {
                        groups::group(c, *g).map_err(|e| {
                            if e.code == ErrorCode::NotFound {
                                Error::new(ErrorCode::InvalidInput).with_detail("unknown group id")
                            } else {
                                e
                            }
                        })?;
                    }
                }
                clients::set_scope(c, &id2, &scope)?;
                clients::client(c, &id2)?.ok_or_else(|| Error::new(ErrorCode::NotFound))
            })
            .await?;
        tracing::info!(target: "sr_engine::clients", client = %id, "scope changed");
        self.inner().bus.publish(EngineEvent::ClientsChanged {
            client_id: Some(id),
        });
        Ok(self.client_view(rec))
    }

    /// `clients.forget`: an HTTP tool's token is revoked too.
    pub async fn forget_client(&self, id: String) -> Result<bool, Error> {
        if id.starts_with(HTTP_CLIENT_PREFIX) {
            return self.http_revoke(id).await;
        }
        let id2 = id.clone();
        let removed = self
            .inner()
            .cat
            .write(move |c| clients::remove(c, &id2))
            .await?;
        self.inner()
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_touch
            .remove(&id);
        self.inner().bus.publish(EngineEvent::ClientsChanged {
            client_id: Some(id),
        });
        Ok(removed)
    }

    /// `clients.snippet`.
    pub async fn client_snippet(
        &self,
        kind: ClientKind,
        client_id: Option<String>,
    ) -> Result<Snippet, Error> {
        let bridge = self.inner().cfg.app_dir.join(BRIDGE_EXE);
        if kind == ClientKind::Http {
            let id = client_id.ok_or_else(|| {
                Error::new(ErrorCode::InvalidInput).with_detail("client_id is required for http")
            })?;
            let id2 = id.clone();
            let exists = self
                .inner()
                .cat
                .read_async(move |c| clients::client(c, &id2))
                .await?
                .is_some_and(|r| r.kind == ClientKind::Http);
            if !exists {
                return Err(Error::new(ErrorCode::NotFound));
            }
            let port = self.http_status().port;
            return Ok(snippet_text(kind, &bridge, port, None));
        }
        Ok(snippet_text(kind, &bridge, None, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_restricts_fail_closed() {
        let (a, b) = (FolderId::new(), FolderId::new());
        let s = Scope::Folders([a].into_iter().collect());
        assert_eq!(s.restrict(None).unwrap(), Some(vec![a]));
        assert_eq!(s.restrict(Some(vec![a])).unwrap(), Some(vec![a]));
        assert_eq!(
            s.restrict(Some(vec![a, b])).unwrap_err().code,
            ErrorCode::NotFound,
            "a folder outside the scope looks like an unknown one"
        );
        assert!(!s.allows(b) && s.allows(a));
        assert_eq!(Scope::All.restrict(None).unwrap(), None);
        assert!(Scope::Folders(HashSet::new()).is_empty());
        assert!(!Scope::All.is_empty());
    }

    #[test]
    fn snippets_per_kind() {
        let bridge = Path::new(r"C:\Users\u\AppData\Local\Programs\Selene Retrieval\sr-bridge.exe");
        let c = snippet_text(ClientKind::ClaudeDesktop, bridge, None, None);
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        assert_eq!(
            v["mcpServers"][SERVER_KEY]["command"],
            bridge.display().to_string()
        );
        assert_eq!(
            v["mcpServers"][SERVER_KEY]["args"],
            serde_json::json!(["--client", "claude"])
        );
        let vs = snippet_text(ClientKind::VsCode, bridge, None, None);
        let v: serde_json::Value = serde_json::from_str(&vs.text).unwrap();
        assert_eq!(v["servers"][SERVER_KEY]["type"], "stdio");
        assert_eq!(
            v["servers"][SERVER_KEY]["args"],
            serde_json::json!(["--client", "vscode"])
        );
        let cur = snippet_text(ClientKind::Cursor, bridge, None, None);
        assert!(cur.text.contains("\"cursor\""));
        let h = snippet_text(ClientKind::Http, bridge, Some(41234), None);
        let v: serde_json::Value = serde_json::from_str(&h.text).unwrap();
        assert_eq!(
            v["mcpServers"][SERVER_KEY]["url"],
            "http://127.0.0.1:41234/mcp"
        );
        assert_eq!(
            v["mcpServers"][SERVER_KEY]["headers"]["Authorization"],
            "Bearer <token>"
        );
        for s in [&c, &vs, &cur] {
            assert!(!s.text.contains("Bearer"), "no token in stdio snippets");
        }
        let t = snippet_text(ClientKind::Http, bridge, Some(1), Some("abc"));
        assert!(t.text.contains("Bearer abc"));
    }
}
