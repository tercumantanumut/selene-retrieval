//! Folder groups over the engine (M4-E, page 31). Groups are labels over folders (catalog
//! `folder_groups`); every group shares the one search index and model, so nothing here touches
//! the stores. See `sr_catalog::repo::groups`.

use sr_catalog::repo::{clients, groups};
use sr_common::api::{ClientView, GroupView};
use sr_common::error::{Error, ErrorCode};
use sr_common::events::EngineEvent;
use sr_common::ids::{FolderId, GroupId};

use crate::EngineHandle;

impl EngineHandle {
    /// The default group ("My folders"), created at start.
    pub fn default_group(&self) -> GroupId {
        self.inner().default_group
    }

    /// `groups.list`: the default group first, then by name.
    pub async fn groups_list(&self) -> Result<Vec<GroupView>, Error> {
        let removing: Vec<FolderId> = self.removing_ids();
        let mut v = self.inner().cat.read_async(groups::views).await?;
        for g in &mut v {
            g.folder_ids.retain(|f| !removing.contains(f));
        }
        Ok(v)
    }

    async fn group_view(&self, id: GroupId) -> Result<GroupView, Error> {
        self.groups_list()
            .await?
            .into_iter()
            .find(|g| g.group_id == id)
            .ok_or_else(|| Error::new(ErrorCode::NotFound))
    }

    /// `groups.create`.
    pub async fn create_group(&self, name: String) -> Result<GroupView, Error> {
        let now = self.inner().clock.now_ms();
        let id = self
            .inner()
            .cat
            .write(move |c| groups::create(c, &name, now))
            .await?;
        self.inner().bus.publish(EngineEvent::GroupsChanged);
        self.group_view(id).await
    }

    /// `groups.rename`.
    pub async fn rename_group(&self, id: GroupId, name: String) -> Result<GroupView, Error> {
        self.inner()
            .cat
            .write(move |c| groups::rename(c, id, &name))
            .await?;
        self.inner().bus.publish(EngineEvent::GroupsChanged);
        self.group_view(id).await
    }

    /// `groups.delete`: folders move to the default group (nothing leaves search); the group
    /// leaves every app scope without widening it. Returns the folders moved and the apps that now
    /// see no folder at all.
    pub async fn delete_group(&self, id: GroupId) -> Result<(u32, Vec<ClientView>), Error> {
        let (moved, empty) = self
            .inner()
            .cat
            .write(move |c| {
                sr_catalog::atomic(c, |c| {
                    let moved = groups::delete(c, id)?;
                    let empty_ids = clients::drop_group_from_scopes(c, id)?;
                    let mut recs = Vec::new();
                    for cid in empty_ids {
                        if let Some(r) = clients::client(c, &cid)? {
                            recs.push(r);
                        }
                    }
                    Ok((moved, recs))
                })
            })
            .await?;
        tracing::info!(target: "sr_engine::groups", moved, apps_without_access = empty.len(), "folder group deleted");
        self.inner().bus.publish(EngineEvent::GroupsChanged);
        self.inner()
            .bus
            .publish(EngineEvent::ClientsChanged { client_id: None });
        self.refresh_statuses(None).await;
        Ok((
            moved,
            empty.into_iter().map(|r| self.client_view(r)).collect(),
        ))
    }

    /// `groups.move_folder`: only changes which apps may see the folder (no re-index).
    pub async fn move_folder_to_group(&self, folder: FolderId, to: GroupId) -> Result<(), Error> {
        let now = self.inner().clock.now_ms();
        self.inner()
            .cat
            .write(move |c| groups::move_folder(c, folder, to, now))
            .await?;
        self.inner().bus.publish(EngineEvent::GroupsChanged);
        Ok(())
    }

    fn removing_ids(&self) -> Vec<FolderId> {
        self.inner()
            .maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .removing
            .keys()
            .copied()
            .collect()
    }
}
