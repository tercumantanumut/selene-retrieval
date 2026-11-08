//! Opt-in streamable HTTP (M4-E, page 29 Advanced; decision 6 "no TCP port by default").
//!
//! The engine owns the **policy**: whether HTTP is on (`[integrations] http_server_enabled`),
//! the remembered port, and the per-tool bearer tokens. The listener itself lives in
//! `sr-mcp-server` (it needs rmcp); it watches [`EngineHandle::http_wanted`] and reports back with
//! [`EngineHandle::http_report`].
//!
//! Tokens: 256 random bits (OS CSPRNG), 43-char base64url, shown once. Stored only in Windows
//! Credential Manager (`SeleneRetrieval/http-token:<client_id>`, removed by the uninstaller with
//! every `SeleneRetrieval/*` credential); the daemon keeps a BLAKE3 hash in memory to check
//! requests. Never logged, never in diagnostics or events.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

use sr_catalog::repo::clients;
use sr_common::api::{HTTP_CLIENT_PREFIX, HttpStatus, MAX_TOOL_NAME_CHARS, TokenCreated};
use sr_common::error::{ClientKind, Error, ErrorCode};
use sr_common::events::EngineEvent;
use tokio::sync::watch;

use crate::EngineHandle;

/// What the listener should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HttpWanted {
    pub enabled: bool,
    /// Remembered port (`None` = pick a free one, then remember it).
    pub port: Option<u16>,
    /// Bumped when a token is revoked or rotated: the listener restarts, ending every session.
    pub generation: u64,
}

#[derive(Debug, Default)]
struct Runtime {
    running: bool,
    port: Option<u16>,
    error: Option<ErrorCode>,
}

pub(crate) struct HttpControl {
    wanted: watch::Sender<HttpWanted>,
    runtime: Mutex<Runtime>,
    /// blake3(token) → client id.
    tokens: RwLock<HashMap<[u8; 32], String>>,
}

impl HttpControl {
    pub(crate) fn new(enabled: bool, port: Option<u16>) -> Self {
        let (wanted, _) = watch::channel(HttpWanted {
            enabled,
            port,
            generation: 0,
        });
        Self {
            wanted,
            runtime: Mutex::new(Runtime::default()),
            tokens: RwLock::new(HashMap::new()),
        }
    }
}

/// Credential Manager account of a tool's token.
pub fn token_account(client_id: &str) -> String {
    format!("http-token:{client_id}")
}

/// The last 4 characters ("hidden, ends in {last4}"); tokens are ASCII.
fn token_hint(token: &str) -> String {
    token
        .get(token.len().saturating_sub(4)..)
        .unwrap_or_default()
        .to_string()
}

fn hash(token: &str) -> [u8; 32] {
    *blake3::hash(token.as_bytes()).as_bytes()
}

/// RFC 4648 base64url without padding.
fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let take = chunk.len() + 1;
        for i in 0..take {
            out.push(char::from(A[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

/// A fresh 256-bit token.
pub fn new_token() -> Result<String, Error> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf)
        .map_err(|e| Error::new(ErrorCode::Internal).with_detail(format!("random: {e}")))?;
    Ok(b64url(&buf))
}

fn clean_tool_name(name: &str) -> Option<String> {
    let n = name.trim();
    (!n.is_empty() && n.chars().count() <= MAX_TOOL_NAME_CHARS && !n.chars().any(char::is_control))
        .then(|| n.to_string())
}

impl EngineHandle {
    fn http(&self) -> &HttpControl {
        &self.inner().http
    }

    /// The listener's instructions (sr-mcp-server watches this).
    pub fn http_wanted(&self) -> watch::Receiver<HttpWanted> {
        self.http().wanted.subscribe()
    }

    /// `http.status`.
    pub fn http_status(&self) -> HttpStatus {
        let enabled = self.http().wanted.borrow().enabled;
        let rt = self
            .http()
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tools = self
            .http()
            .tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        HttpStatus {
            enabled,
            running: rt.running,
            port: rt.port.or(self.http().wanted.borrow().port),
            url: rt
                .port
                .filter(|_| rt.running)
                .map(|p| format!("http://127.0.0.1:{p}/mcp")),
            error: rt.error,
            tools: u32::try_from(tools).unwrap_or(u32::MAX),
        }
    }

    /// The listener reports its state. A port picked for the first time is remembered in
    /// `settings.toml` so setup texts keep working after a restart.
    pub async fn http_report(&self, running: bool, port: Option<u16>, error: Option<ErrorCode>) {
        {
            let mut rt = self
                .http()
                .runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            rt.running = running;
            if port.is_some() {
                rt.port = port;
            }
            rt.error = error;
        }
        if let Some(p) = port
            && running
            && self.http().wanted.borrow().port != Some(p)
        {
            let r = self
                .update_settings(|s| s.integrations.http_port = Some(p))
                .await;
            if let Err(e) = r {
                tracing::warn!(target: "sr_engine::http", code = %e.code, "HTTP port not remembered");
            }
            self.http().wanted.send_modify(|w| w.port = Some(p));
        }
        self.inner()
            .bus
            .publish(EngineEvent::HttpState(self.http_status()));
    }

    /// `http.set_enabled` (privileged). Applies at once (the listener starts or stops).
    pub async fn http_set_enabled(&self, on: bool) -> Result<HttpStatus, Error> {
        // M5-E: an administrator's policy can forbid the listener (turning it off always works).
        if on && !self.inner().policy.http_server_allowed() {
            return Err(crate::policy::refused("HTTP for local tools"));
        }
        self.update_settings(|s| s.integrations.http_server_enabled = on)
            .await?;
        if on {
            self.load_http_tokens().await;
        }
        // S-03: "Retry" after "Port in use" is turning HTTP on again: restart the listener now
        // (same port; it never moves to another one).
        let retry_now = {
            let mut rt = self
                .http()
                .runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let taken = rt.error == Some(ErrorCode::PortInUse);
            if !on && taken {
                // Off is off: no stale "Port in use" notice.
                rt.error = None;
            }
            on && taken
        };
        self.http().wanted.send_modify(|w| {
            w.enabled = on;
            if retry_now {
                w.generation += 1;
            }
        });
        tracing::info!(target: "sr_engine::http", enabled = on, "HTTP for local tools");
        let st = self.http_status();
        self.inner().bus.publish(EngineEvent::HttpState(st.clone()));
        Ok(st)
    }

    /// Load every HTTP tool's token hash from Credential Manager (start, or when HTTP is turned on).
    pub(crate) async fn load_http_tokens(&self) {
        let rows = match self.inner().cat.read_async(clients::clients).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(target: "sr_engine::http", code = %e.code, "HTTP tools not loaded");
                return;
            }
        };
        let secrets = self.inner().llm.secrets.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            let mut m = HashMap::new();
            for r in rows.into_iter().filter(|r| r.kind == ClientKind::Http) {
                let account = r.token_ref.clone().unwrap_or_else(|| token_account(&r.id));
                match secrets.get(&account) {
                    Ok(Some(t)) => {
                        m.insert(hash(&t), r.id);
                    }
                    Ok(None) => {
                        tracing::warn!(target: "sr_engine::http", "an HTTP tool has no stored token; it can't connect until rotated");
                    }
                    Err(e) => {
                        tracing::warn!(target: "sr_engine::http", code = %e.code, "an HTTP tool's token couldn't be read");
                    }
                }
            }
            m
        })
        .await
        .unwrap_or_default();
        *self
            .http()
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = loaded;
    }

    /// The HTTP tool a bearer token belongs to (`None` = refuse the request).
    pub fn http_client_for_token(&self, token: &str) -> Option<String> {
        if token.is_empty() || token.len() > 256 {
            return None;
        }
        self.http()
            .tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&hash(token))
            .cloned()
    }

    async fn store_token(&self, client_id: &str) -> Result<String, Error> {
        let token = new_token()?;
        let (secrets, account, t2) = (
            self.inner().llm.secrets.clone(),
            token_account(client_id),
            token.clone(),
        );
        tokio::task::spawn_blocking(move || secrets.set(&account, &t2))
            .await
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))??;
        Ok(token)
    }

    fn forget_hashes_of(&self, client_id: &str) {
        self.http()
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, v| v != client_id);
    }

    fn end_http_sessions(&self) {
        self.http().wanted.send_modify(|w| w.generation += 1);
    }

    async fn token_created(&self, client_id: &str, token: String) -> Result<TokenCreated, Error> {
        let id = client_id.to_string();
        let rec = self
            .inner()
            .cat
            .read_async(move |c| clients::client(c, &id))
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        let bridge = self.inner().cfg.app_dir.join(crate::clients::BRIDGE_EXE);
        let snippet = crate::clients::snippet_text(
            ClientKind::Http,
            &bridge,
            self.http_status().port,
            Some(&token),
        );
        Ok(TokenCreated {
            client: self.client_view(rec),
            token,
            snippet,
        })
    }

    /// `http.create_token` (privileged): a new HTTP tool with its own token, scope "All my
    /// folders". The token is returned once.
    pub async fn http_create_token(&self, name: String) -> Result<TokenCreated, Error> {
        let name = clean_tool_name(&name).ok_or_else(|| {
            Error::new(ErrorCode::InvalidInput).with_detail("tool name empty or too long")
        })?;
        let id = format!("{HTTP_CLIENT_PREFIX}{}", sr_common::ids::TraceId::new()).to_lowercase();
        let token = self.store_token(&id).await?;
        let (id2, account, hint) = (id.clone(), token_account(&id), token_hint(&token));
        let now = self.inner().clock.now_ms();
        let stored = self
            .inner()
            .cat
            .write(move |c| clients::insert_http(c, &id2, &name, &account, &hint, now))
            .await;
        if let Err(e) = stored {
            // Never leave a credential behind for a tool that doesn't exist.
            let (secrets, account) = (self.inner().llm.secrets.clone(), token_account(&id));
            let _ = tokio::task::spawn_blocking(move || secrets.delete(&account)).await;
            return Err(e);
        }
        self.http()
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(hash(&token), id.clone());
        tracing::info!(target: "sr_engine::http", client = %id, "HTTP tool added");
        self.inner().bus.publish(EngineEvent::ClientsChanged {
            client_id: Some(id.clone()),
        });
        self.inner()
            .bus
            .publish(EngineEvent::HttpState(self.http_status()));
        self.token_created(&id, token).await
    }

    /// `http.rotate_token` (privileged): a new token; the old one stops at once.
    pub async fn http_rotate_token(&self, client_id: String) -> Result<TokenCreated, Error> {
        let id = client_id.clone();
        let rec = self
            .inner()
            .cat
            .read_async(move |c| clients::client(c, &id))
            .await?
            .filter(|r| r.kind == ClientKind::Http)
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        let token = self.store_token(&rec.id).await?;
        let hint = token_hint(&token);
        let (id2, account) = (rec.id.clone(), token_account(&rec.id));
        self.inner()
            .cat
            .write(move |c| clients::set_token(c, &id2, &account, &hint))
            .await?;
        self.forget_hashes_of(&rec.id);
        self.http()
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(hash(&token), rec.id.clone());
        self.end_http_sessions();
        tracing::info!(target: "sr_engine::http", client = %rec.id, "HTTP token rotated");
        self.inner().bus.publish(EngineEvent::ClientsChanged {
            client_id: Some(rec.id.clone()),
        });
        self.token_created(&rec.id, token).await
    }

    /// `http.revoke`: the token is deleted and the tool removed; its next request is refused and
    /// open HTTP sessions end.
    pub async fn http_revoke(&self, client_id: String) -> Result<bool, Error> {
        if !client_id.starts_with(HTTP_CLIENT_PREFIX) {
            return Ok(false);
        }
        self.forget_hashes_of(&client_id);
        self.end_http_sessions();
        let (secrets, account) = (self.inner().llm.secrets.clone(), token_account(&client_id));
        let deleted = tokio::task::spawn_blocking(move || secrets.delete(&account))
            .await
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
        if let Err(e) = deleted {
            tracing::warn!(target: "sr_engine::http", code = %e.code, "HTTP token credential not deleted");
        }
        let id2 = client_id.clone();
        let removed = self
            .inner()
            .cat
            .write(move |c| clients::remove(c, &id2))
            .await?;
        tracing::info!(target: "sr_engine::http", client = %client_id, "HTTP tool revoked");
        self.inner().bus.publish(EngineEvent::ClientsChanged {
            client_id: Some(client_id),
        });
        self.inner()
            .bus
            .publish(EngineEvent::HttpState(self.http_status()));
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_256_bit_base64url() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(token_account("http-01"), "http-token:http-01");
        assert_eq!(clean_tool_name(" script "), Some("script".into()));
        assert_eq!(clean_tool_name(""), None);
    }
}
