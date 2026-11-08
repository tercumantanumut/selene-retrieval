//! Activity (M4-E, page 30): engine hooks write plain lines to the catalog `activity_log` and push
//! `activity.added`; egress rows come from `egress_audit` (pushed as `egress.recorded` by the audit
//! writer). Recording is best effort: it never fails or delays the action it describes.

use sr_catalog::repo::activity::{self, EgressRecord, NewActivity};
use sr_catalog::repo::clients;
use sr_common::api::{
    ACTIVITY_PAGE_DEFAULT, ACTIVITY_PAGE_MAX, ACTIVITY_RETENTION_MS, ActivityKind, ActivityPage,
    EgressPage, EgressRow, ProviderRef, default_client_name,
};
use sr_common::error::{ClientKind, Error, ErrorCode, Purpose};
use sr_common::events::EngineEvent;

use crate::EngineHandle;

/// Meta key holding the daemon build that last ran (an "Updated to version …" line on change).
pub const META_LAST_BUILD: &str = "last_build";

/// "This app" (an egress row with no MCP client).
pub const THIS_APP: &str = "This app";

/// Provider display name from the manifest (the id when unknown).
pub(crate) fn provider_name(id: &str) -> String {
    sr_providers::manifest()
        .get(id)
        .map_or_else(|| id.to_string(), |s| s.display_name.clone())
}

/// One stored egress row as the app shows it. Blocking (catalog connection given).
pub(crate) fn egress_row(c: &rusqlite::Connection, r: EgressRecord) -> EgressRow {
    let client_name = match &r.client_id {
        None => THIS_APP.to_string(),
        Some(id) => clients::client(c, id)
            .ok()
            .flatten()
            .map_or_else(|| default_client_name(id), |rec| rec.display_name),
    };
    EgressRow {
        id: r.id,
        ts: r.ts,
        provider_name: provider_name(&r.provider),
        provider: r.provider,
        purpose: r.purpose.unwrap_or(Purpose::Answer),
        bytes: r.bytes,
        client_id: r.client_id,
        client_name,
    }
}

/// The search-model word for a model id (page 30 "Search upgraded to {Better} search").
pub(crate) fn model_word(model_id: &str) -> String {
    use sr_common::tier::{HIGH, MINIMUM, STANDARD};
    if model_id == MINIMUM.model_id {
        "Basic".into()
    } else if model_id == STANDARD.model_id {
        "Better".into()
    } else if model_id == HIGH.model_id {
        "Best".into()
    } else if model_id == crate::multilingual::MULTILINGUAL_MODEL_ID {
        "Multilingual".into()
    } else {
        model_id.to_string()
    }
}

impl EngineHandle {
    /// Append one activity line and push `activity.added`. Never fails the caller.
    pub(crate) async fn record_activity(&self, a: NewActivity) {
        let now = self.inner().clock.now_ms();
        match self
            .inner()
            .cat
            .write(move |c| activity::append(c, &a, now))
            .await
        {
            Ok(entry) => {
                self.inner()
                    .bus
                    .publish(EngineEvent::ActivityAdded { entry });
            }
            Err(e) => {
                tracing::debug!(target: "sr_engine::activity", code = %e.code, "activity line not written");
            }
        }
    }

    /// [`Self::record_activity`] off the caller's path.
    pub(crate) fn record_activity_bg(&self, a: NewActivity) {
        let h = self.clone();
        let t = tokio::spawn(async move { h.record_activity(a).await });
        self.track_task(t);
    }

    /// `activity.list`: newest first, last 7 days, paged by id.
    pub async fn activity_list(
        &self,
        before_id: Option<i64>,
        limit: Option<u32>,
    ) -> Result<ActivityPage, Error> {
        let limit = limit
            .unwrap_or(ACTIVITY_PAGE_DEFAULT)
            .clamp(1, ACTIVITY_PAGE_MAX);
        let since = self
            .inner()
            .clock
            .now_ms()
            .saturating_sub(ACTIVITY_RETENTION_MS);
        let (entries, next_before_id) = self
            .inner()
            .cat
            .read_async(move |c| activity::page(c, before_id, limit, since))
            .await?;
        Ok(ActivityPage {
            entries,
            next_before_id,
        })
    }

    /// `activity.egress`: what was sent to which provider, newest first, paged by id.
    pub async fn activity_egress(
        &self,
        before_id: Option<i64>,
        limit: Option<u32>,
        provider: Option<String>,
    ) -> Result<EgressPage, Error> {
        let limit = limit
            .unwrap_or(ACTIVITY_PAGE_DEFAULT)
            .clamp(1, ACTIVITY_PAGE_MAX);
        self.inner()
            .cat
            .read_async(move |c| {
                let (rows, next) = activity::egress_page(c, before_id, limit, provider.as_deref())?;
                let rows = rows.into_iter().map(|r| egress_row(c, r)).collect();
                let providers = activity::egress_providers(c)?
                    .into_iter()
                    .map(|id| ProviderRef {
                        name: provider_name(&id),
                        id,
                    })
                    .collect();
                Ok(EgressPage {
                    rows,
                    next_before_id: next,
                    providers,
                })
            })
            .await
    }

    /// `activity.note`: the app records Claude / Cursor / VS Code Connect and Disconnect (its
    /// config writers run in the app).
    pub async fn activity_note(
        &self,
        kind: ActivityKind,
        client: ClientKind,
    ) -> Result<i64, Error> {
        if !matches!(
            kind,
            ActivityKind::ClientConnected | ActivityKind::ClientDisconnected
        ) {
            return Err(Error::new(ErrorCode::InvalidInput)
                .with_detail("only client_connected / client_disconnected can be noted"));
        }
        let name = match client {
            ClientKind::ClaudeDesktop => "Claude Desktop",
            ClientKind::Cursor => "Cursor",
            ClientKind::VsCode => "VS Code",
            ClientKind::Other | ClientKind::Http => "An app",
        };
        let now = self.inner().clock.now_ms();
        let a = NewActivity::new(kind).client(client, name);
        let entry = self
            .inner()
            .cat
            .write(move |c| activity::append(c, &a, now))
            .await?;
        let id = entry.id;
        self.inner()
            .bus
            .publish(EngineEvent::ActivityAdded { entry });
        Ok(id)
    }

    /// At start: an "Updated to version {v}" line when the daemon build changed.
    pub(crate) async fn note_build(&self) {
        let build = env!("CARGO_PKG_VERSION").to_string();
        let b2 = build.clone();
        let prev = self
            .inner()
            .cat
            .write(move |c| {
                let prev = sr_catalog::repo::meta::get(c, META_LAST_BUILD)?;
                if prev.as_deref() != Some(b2.as_str()) {
                    sr_catalog::repo::meta::set(c, META_LAST_BUILD, &b2)?;
                }
                Ok(prev)
            })
            .await;
        if let Ok(Some(prev)) = prev
            && prev != build
        {
            self.record_activity(NewActivity::new(ActivityKind::Updated).text(build))
                .await;
        }
    }

    /// Maintenance: drop activity older than 7 days.
    pub(crate) async fn prune_activity(&self) {
        let now = self.inner().clock.now_ms();
        let _ = self
            .inner()
            .cat
            .write(move |c| activity::prune(c, now, ACTIVITY_RETENTION_MS))
            .await;
    }

    /// Bus → activity for events with no direct hook (`index.swapped` → "Search upgraded").
    pub(crate) fn spawn_activity_listener(&self) {
        let h = self.clone();
        let mut rx = self.inner().bus.subscribe();
        let t = tokio::spawn(async move {
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                if let EngineEvent::IndexSwapped { model_id, .. } = ev.payload {
                    h.record_activity(
                        NewActivity::new(ActivityKind::SearchUpgraded).text(model_word(&model_id)),
                    )
                    .await;
                }
            }
        });
        self.track_task(t);
    }

    /// The daemon finished an index move (page 32).
    pub async fn note_index_moved(&self, path: &std::path::Path) {
        self.record_activity(
            NewActivity::new(ActivityKind::IndexMoved).text(path.display().to_string()),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_words() {
        assert_eq!(model_word(sr_common::tier::MINIMUM.model_id), "Basic");
        assert_eq!(model_word(sr_common::tier::STANDARD.model_id), "Better");
        assert_eq!(model_word(sr_common::tier::HIGH.model_id), "Best");
        assert_eq!(
            model_word(crate::multilingual::MULTILINGUAL_MODEL_ID),
            "Multilingual"
        );
    }
}
