//! Connected apps, service side (page 29 Advanced; contract §4, §5): which folders each app may
//! see, the opt-in local HTTP server and its per-tool tokens.
//!
//! Tokens: the service returns a token **once** (create / rotate). The app passes it to the UI for
//! [Copy] in a snippet with the token filled in, plus a masked copy for the screen (`••••` + last
//! 4, OQ-45). It is never stored, logged or put into diagnostics by the app.
#![allow(clippy::result_large_err)] // `UiError` is the Tauri IPC error (see commands.rs).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use specta::Type;
use sr_common::api::{ClientScope, ClientView, ClientsList, HttpStatus, TokenCreated};
use sr_common::error::{ClientKind, Error, ErrorCode};

use crate::daemon::{Rpc, method};
use crate::dto::{ClientApp, UiError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AppKind {
    ClaudeDesktop,
    Cursor,
    VsCode,
    Other,
    Http,
}

fn kind_view(k: ClientKind) -> AppKind {
    match k {
        ClientKind::ClaudeDesktop => AppKind::ClaudeDesktop,
        ClientKind::Cursor => AppKind::Cursor,
        ClientKind::VsCode => AppKind::VsCode,
        ClientKind::Other => AppKind::Other,
        ClientKind::Http => AppKind::Http,
    }
}

/// "All my folders" (default) or only some folder groups. Unknown groups grant nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ScopeView {
    All,
    Groups { group_ids: Vec<String> },
}

fn scope_view(s: &ClientScope) -> ScopeView {
    match s {
        ClientScope::All => ScopeView::All,
        ClientScope::Groups { group_ids } => ScopeView::Groups {
            group_ids: group_ids.iter().map(ToString::to_string).collect(),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ServiceClient {
    pub client_id: String,
    pub kind: AppKind,
    pub name: String,
    pub scope: ScopeView,
    /// "Last used {time}" (ms since the Unix epoch).
    pub last_used_ms: Option<f64>,
    /// Live sessions right now.
    pub sessions_open: u32,
    /// HTTP tools: shown as "hidden, ends in {last4}".
    pub token_last4: Option<String>,
}

#[allow(clippy::cast_precision_loss)] // ms timestamps stay far below 2^53
fn ms(t: i64) -> f64 {
    t as f64
}

fn client(c: &ClientView) -> ServiceClient {
    ServiceClient {
        client_id: c.client_id.clone(),
        kind: kind_view(c.kind),
        name: c.display_name.clone(),
        scope: scope_view(&c.scope),
        last_used_ms: c.last_seen_at.map(ms),
        sessions_open: c.sessions_open,
        // Never more than 4 characters, even if a service sent more.
        token_last4: c.token_last4.as_ref().map(|t| {
            let n = t.chars().count();
            t.chars().skip(n.saturating_sub(4)).collect()
        }),
    }
}

/// The opt-in local server (off by default, 127.0.0.1 only, token required).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct HttpView {
    pub enabled: bool,
    pub running: bool,
    /// "Address: http://127.0.0.1:{port}" while running.
    pub url: Option<String>,
    /// Set when it is on but couldn't start (e.g. the port couldn't be opened).
    pub problem: Option<UiError>,
    pub tools: u32,
}

fn http_view(h: &HttpStatus) -> HttpView {
    HttpView {
        enabled: h.enabled,
        running: h.running,
        url: h.url.clone().filter(|_| h.running),
        problem: h.error.filter(|_| h.enabled && !h.running).map(|c| {
            // #41 PortInUse (M5-E, S-03): the message names the remembered port; its
            // [Retry] turns HTTP on again (`http.set_enabled(true)`, page 29).
            let e = Error::new(c);
            let e = match (c, h.port) {
                (ErrorCode::PortInUse, Some(p)) => e.with_name(p.to_string()),
                _ => e,
            };
            UiError::from(e)
        }),
        tools: h.tools,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ServiceClientsView {
    pub clients: Vec<ServiceClient>,
    pub http: HttpView,
}

fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, UiError> {
    serde_json::from_value(v).map_err(|e| {
        UiError::from(Error::new(ErrorCode::Internal).with_detail(format!("unexpected reply: {e}")))
    })
}

pub async fn list<R: Rpc>(rpc: &R) -> Result<ServiceClientsView, UiError> {
    let v = rpc
        .call(method::CLIENTS_LIST, json!({}))
        .await
        .map_err(UiError::from)?;
    let l: ClientsList = parse(v)?;
    Ok(ServiceClientsView {
        clients: l.clients.iter().map(client).collect(),
        http: http_view(&l.http),
    })
}

/// "Last used" per one-click app (the newest of its rows), for the app cards.
pub fn last_used(v: &ServiceClientsView) -> HashMap<ClientApp, f64> {
    let mut out: HashMap<ClientApp, f64> = HashMap::new();
    for c in &v.clients {
        let app = match c.kind {
            AppKind::ClaudeDesktop => ClientApp::ClaudeDesktop,
            AppKind::Cursor => ClientApp::Cursor,
            AppKind::VsCode => ClientApp::VsCode,
            AppKind::Other | AppKind::Http => continue,
        };
        if let Some(t) = c.last_used_ms {
            let e = out.entry(app).or_insert(t);
            if t > *e {
                *e = t;
            }
        }
    }
    out
}

pub async fn set_scope<R: Rpc>(
    rpc: &R,
    client_id: &str,
    scope: &ScopeView,
) -> Result<ServiceClient, UiError> {
    let v = rpc
        .call(
            method::CLIENTS_SET_SCOPE,
            json!({ "client_id": client_id, "scope": scope }),
        )
        .await
        .map_err(UiError::from)?;
    Ok(client(&parse::<ClientView>(v)?))
}

pub async fn set_http<R: Rpc>(rpc: &R, enabled: bool) -> Result<HttpView, UiError> {
    let v = rpc
        .call(method::HTTP_SET_ENABLED, json!({ "enabled": enabled }))
        .await
        .map_err(UiError::from)?;
    Ok(http_view(&parse::<HttpStatus>(v)?))
}

/// A new tool's password, shown once (OQ-45).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct TokenView {
    pub client: ServiceClient,
    /// Setup text with the token filled in: what [Copy] copies.
    pub snippet: String,
    /// The same text with the token shown as `••••` + its last 4 characters (on screen).
    pub masked_snippet: String,
    pub last4: String,
}

const PLACEHOLDER: &str = "<token>";

fn mask(token: &str) -> (String, String) {
    let n = token.chars().count();
    let last4: String = token.chars().skip(n.saturating_sub(4)).collect();
    (format!("••••{last4}"), last4)
}

pub fn token_view(t: &TokenCreated) -> TokenView {
    let (masked, last4) = mask(&t.token);
    // The service's snippet has the literal `<token>` placeholder (or the token itself).
    let template = if t.snippet.text.contains(PLACEHOLDER) {
        t.snippet.text.clone()
    } else {
        t.snippet.text.replace(&t.token, PLACEHOLDER)
    };
    TokenView {
        client: client(&t.client),
        snippet: template.replace(PLACEHOLDER, &t.token),
        masked_snippet: template.replace(PLACEHOLDER, &masked),
        last4,
    }
}

/// [Add a tool…] → name → a token and its setup text.
pub async fn create_token<R: Rpc>(rpc: &R, name: &str) -> Result<TokenView, UiError> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > sr_common::api::MAX_TOOL_NAME_CHARS {
        return Err(UiError::new(
            "Check the name",
            "Use a name of 1 to 60 characters.",
            crate::dto::UiAction::None,
            "",
            "InvalidInput · name",
        ));
    }
    let v = rpc
        .call(method::HTTP_CREATE_TOKEN, json!({ "name": n }))
        .await
        .map_err(UiError::from)?;
    Ok(token_view(&parse::<TokenCreated>(v)?))
}

/// Copy the setup text again later = a new password (the old one stops at once).
pub async fn rotate_token<R: Rpc>(rpc: &R, client_id: &str) -> Result<TokenView, UiError> {
    let v = rpc
        .call(method::HTTP_ROTATE_TOKEN, json!({ "client_id": client_id }))
        .await
        .map_err(UiError::from)?;
    Ok(token_view(&parse::<TokenCreated>(v)?))
}

/// [Revoke…] → [Revoke]: the tool loses access at once.
pub async fn revoke<R: Rpc>(rpc: &R, client_id: &str) -> Result<(), UiError> {
    rpc.call(method::HTTP_REVOKE, json!({ "client_id": client_id }))
        .await
        .map(|_| ())
        .map_err(UiError::from)
}

/// [Remove…] → [Remove] (M5, contract §4 `clients.forget`): a stale app leaves the list (an HTTP
/// tool's password is revoked too). If the app connects again it comes back with "All my folders".
/// Only offered for apps with no open session, so a running app is never cut off by surprise.
pub async fn forget<R: Rpc>(rpc: &R, client_id: &str) -> Result<bool, UiError> {
    let id = client_id.trim();
    if id.is_empty() || id.chars().count() > sr_common::api::MAX_CLIENT_ID_CHARS {
        return Err(UiError::from(Error::new(ErrorCode::InvalidInput)));
    }
    let v = rpc
        .call(method::CLIENTS_FORGET, json!({ "client_id": id }))
        .await
        .map_err(UiError::from)?;
    Ok(v.get("removed").and_then(Value::as_bool).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake::FakeRpc;

    const TOKEN: &str = "abcdefghijklmnopqrstuvwxyz0123456789ABCDwxyz";

    fn client_json(id: &str, kind: &str, last: Option<i64>) -> Value {
        json!({"client_id": id, "kind": kind, "display_name": id, "scope": {"mode": "all"},
            "first_seen_at": null, "last_seen_at": last, "sessions_open": 1})
    }

    #[test]
    fn port_in_use_names_the_port_and_offers_retry() {
        let h = HttpStatus {
            enabled: true,
            running: false,
            port: Some(4242),
            url: None,
            error: Some(ErrorCode::PortInUse),
            tools: 1,
        };
        let p = http_view(&h).problem.expect("a notice");
        assert_eq!(p.title, "Port in use");
        assert!(
            p.body.starts_with("Port 4242 is in use by another program"),
            "{}",
            p.body
        );
        assert_eq!(p.action_label, "Retry");
    }

    #[tokio::test]
    async fn list_maps_scopes_last_used_and_http() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(json!({
            "clients": [
                client_json("claude", "claude_desktop", Some(1_000)),
                client_json("cursor", "cursor", None),
                {"client_id": "http-01", "kind": "http", "display_name": "My script",
                 "scope": {"mode": "groups", "group_ids": []}, "first_seen_at": null,
                 "last_seen_at": 5, "sessions_open": 0, "token_last4": "LONGERwxyz"}
            ],
            "http": {"enabled": true, "running": false, "port": 4242,
                     "url": "http://127.0.0.1:4242/mcp", "error": "Internal", "tools": 1}
        })));
        let v = list(&rpc).await.unwrap();
        assert_eq!(v.clients[2].scope, ScopeView::Groups { group_ids: vec![] });
        assert_eq!(v.clients[2].token_last4.as_deref(), Some("wxyz"));
        assert!(v.http.problem.is_some(), "on but not running: plain error");
        assert!(v.http.url.is_none(), "no address while not running");
        let lu = last_used(&v);
        assert_eq!(lu.get(&ClientApp::ClaudeDesktop), Some(&1_000.0));
        assert!(!lu.contains_key(&ClientApp::Cursor));
    }

    #[test]
    fn tokens_are_masked_on_screen_and_full_on_copy() {
        let t: TokenCreated = serde_json::from_value(json!({
            "client": client_json("http-01", "http", None),
            "token": TOKEN,
            "snippet": {"kind": "http", "file_hint": "your app's MCP settings",
                "text": "{\"headers\":{\"Authorization\":\"Bearer <token>\"}}"}
        }))
        .unwrap();
        let v = token_view(&t);
        assert!(v.snippet.contains(TOKEN));
        assert!(!v.masked_snippet.contains(TOKEN));
        assert!(v.masked_snippet.contains("••••wxyz"));
        assert_eq!(v.last4, "wxyz");
    }

    #[tokio::test]
    async fn scope_http_and_names_forward() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(client_json("cursor", "cursor", None)));
        set_scope(
            &rpc,
            "cursor",
            &ScopeView::Groups {
                group_ids: vec!["G1".into()],
            },
        )
        .await
        .unwrap();
        assert_eq!(
            rpc.calls()[0].1,
            json!({"client_id": "cursor", "scope": {"mode": "groups", "group_ids": ["G1"]}})
        );
        rpc.reply(Ok(
            json!({"enabled": false, "running": false, "port": null, "url": null,
            "error": null, "tools": 0}),
        ));
        assert!(!set_http(&rpc, false).await.unwrap().enabled);
        assert!(create_token(&rpc, "   ").await.is_err());
        assert_eq!(
            rpc.calls().len(),
            2,
            "an empty name never reaches the service"
        );
    }

    #[tokio::test]
    async fn forget_sends_only_a_real_id() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(json!({"removed": true})));
        assert!(forget(&rpc, " cursor ").await.unwrap());
        assert_eq!(
            rpc.calls()[0],
            (
                method::CLIENTS_FORGET.to_owned(),
                json!({"client_id": "cursor"})
            )
        );
        assert!(forget(&rpc, "  ").await.is_err());
        assert!(forget(&rpc, &"x".repeat(65)).await.is_err());
        assert_eq!(rpc.calls().len(), 1, "bad ids never reach the service");
    }
}
