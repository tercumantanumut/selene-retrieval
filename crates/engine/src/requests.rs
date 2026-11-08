//! In-flight app requests (M4-E, pages 27–28): `answer` / `enhance_prompt` calls that carry a
//! client-chosen `request_id` can be stopped with `requests.cancel`, and every call of a pipe
//! session is stopped when that session closes. Stopping drops the provider request at once
//! (nothing more is billed, M3 exit "cancel stops billing").

use std::collections::HashMap;
use std::sync::Mutex;

use sr_common::api::is_valid_request_id;
use sr_common::error::{Error, ErrorCode};
use tokio_util::sync::CancellationToken;

use crate::EngineHandle;

#[derive(Debug, Default)]
pub(crate) struct Requests {
    map: Mutex<HashMap<String, Entry>>,
    next: std::sync::atomic::AtomicU64,
}

#[derive(Debug)]
struct Entry {
    token: CancellationToken,
    session: Option<u64>,
    serial: u64,
}

/// Removes the request from the registry when the call ends (any way).
pub struct RequestGuard {
    handle: EngineHandle,
    key: String,
    serial: u64,
    token: CancellationToken,
}

impl RequestGuard {
    pub fn token(&self) -> &CancellationToken {
        &self.token
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let mut m = self
            .handle
            .inner()
            .requests
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if m.get(&self.key).is_some_and(|e| e.serial == self.serial) {
            m.remove(&self.key);
        }
    }
}

impl EngineHandle {
    /// Register a call. `request_id` is the client's id (`None` = not cancellable by id, still
    /// cancelled with its session). The token is a child of the engine's shutdown token.
    pub fn begin_request(
        &self,
        request_id: Option<&str>,
        session: Option<u64>,
    ) -> Result<RequestGuard, Error> {
        let token = self.inner().shutdown.child_token();
        let serial = self
            .inner()
            .requests
            .next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key = match request_id {
            Some(id) => {
                if !is_valid_request_id(id) {
                    return Err(Error::new(ErrorCode::InvalidInput)
                        .with_detail("request_id must be 1-64 of [A-Za-z0-9_-]"));
                }
                id.to_string()
            }
            None => format!("~anon-{serial}"),
        };
        let mut m = self
            .inner()
            .requests
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if m.contains_key(&key) {
            return Err(Error::new(ErrorCode::InvalidInput)
                .with_detail("a request with this request_id is still running"));
        }
        m.insert(
            key.clone(),
            Entry {
                token: token.clone(),
                session,
                serial,
            },
        );
        drop(m);
        Ok(RequestGuard {
            handle: self.clone(),
            key,
            serial,
            token,
        })
    }

    /// `requests.cancel`: `true` when a running request was stopped.
    pub fn cancel_request(&self, request_id: &str) -> bool {
        let m = self
            .inner()
            .requests
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match m.get(request_id) {
            Some(e) if !request_id.starts_with('~') => {
                e.token.cancel();
                true
            }
            _ => false,
        }
    }

    /// A pipe session closed: stop every call it started. Returns how many.
    pub fn cancel_session_requests(&self, session: u64) -> usize {
        let m = self
            .inner()
            .requests
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut n = 0;
        for e in m.values() {
            if e.session == Some(session) {
                e.token.cancel();
                n += 1;
            }
        }
        n
    }
}
