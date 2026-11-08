//! Folder groups (M4-E, page 31): a label over folders that per-app scopes refer to.
//!
//! Every group shares the one search index (the default store collection), so creating, renaming,
//! deleting a group or moving a folder between groups never touches the stores: it only changes
//! which connected apps may see the folder. `folders.group_id` NULL means the default group
//! ("My folders"), which always exists ([`ensure_default`]) and can't be deleted.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use sr_common::api::{DEFAULT_GROUP_NAME, GroupView, clean_group_name};
use sr_common::error::{Error, ErrorCode, Result};
use sr_common::ids::{FolderId, GroupId};

use crate::db::{DbResultExt, atomic};
use crate::model::RowExt;

/// One group row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRecord {
    pub id: GroupId,
    pub name: String,
    pub is_default: bool,
    pub created_at: i64,
}

fn gid(r: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<GroupId> {
    Ok(GroupId::from_bytes(r.get::<_, [u8; 16]>(idx)?))
}

fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<GroupRecord> {
    Ok(GroupRecord {
        id: gid(r, 0)?,
        name: r.get(1)?,
        is_default: r.get(2)?,
        created_at: r.get(3)?,
    })
}

fn bad_name(detail: &str) -> Error {
    Error::new(ErrorCode::InvalidInput).with_detail(detail.to_string())
}

/// The default group's id, if the row exists yet.
pub fn default_group(conn: &Connection) -> Result<Option<GroupId>> {
    conn.query_row(
        "SELECT id FROM folder_groups WHERE is_default = 1",
        [],
        |r| gid(r, 0),
    )
    .optional()
    .db()
}

/// Creates the default group ("My folders") once; returns its id. If a non-default group already
/// uses the name, the default one gets a numbered name instead (never fails the start).
pub fn ensure_default(conn: &Connection, now: i64) -> Result<GroupId> {
    if let Some(id) = default_group(conn)? {
        return Ok(id);
    }
    atomic(conn, |c| {
        let id = GroupId::new();
        let mut name = DEFAULT_GROUP_NAME.to_string();
        let mut n = 2;
        while name_taken(c, &name, None)? {
            name = format!("{DEFAULT_GROUP_NAME} ({n})");
            n += 1;
        }
        c.execute(
            "INSERT INTO folder_groups(id, name, is_default, created_at) VALUES (?1, ?2, 1, ?3)",
            params![id.to_bytes(), name, now],
        )
        .db()?;
        Ok(id)
    })
}

fn name_taken(conn: &Connection, name: &str, except: Option<GroupId>) -> Result<bool> {
    let other: Option<[u8; 16]> = conn
        .query_row(
            "SELECT id FROM folder_groups WHERE name = ?1 COLLATE NOCASE",
            [name],
            |r| r.get(0),
        )
        .optional()
        .db()?;
    Ok(other.is_some_and(|o| Some(GroupId::from_bytes(o)) != except))
}

/// Every group: the default first, then by name.
pub fn groups(conn: &Connection) -> Result<Vec<GroupRecord>> {
    let mut st = conn
        .prepare_cached(
            "SELECT id, name, is_default, created_at FROM folder_groups
             ORDER BY is_default DESC, name COLLATE NOCASE, id",
        )
        .db()?;
    st.query_map([], from_row)
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()
}

pub fn group(conn: &Connection, id: GroupId) -> Result<GroupRecord> {
    conn.query_row(
        "SELECT id, name, is_default, created_at FROM folder_groups WHERE id = ?1",
        [id.to_bytes()],
        from_row,
    )
    .db()
}

/// New group. `InvalidInput` for an empty, too long or taken name (case-insensitive).
pub fn create(conn: &Connection, name: &str, now: i64) -> Result<GroupId> {
    let name = clean_group_name(name).ok_or_else(|| bad_name("group name empty or too long"))?;
    atomic(conn, |c| {
        if name_taken(c, &name, None)? {
            return Err(bad_name("group name taken"));
        }
        let id = GroupId::new();
        c.execute(
            "INSERT INTO folder_groups(id, name, is_default, created_at) VALUES (?1, ?2, 0, ?3)",
            params![id.to_bytes(), name, now],
        )
        .db()?;
        Ok(id)
    })
}

/// Rename (the default group too).
pub fn rename(conn: &Connection, id: GroupId, name: &str) -> Result<()> {
    let name = clean_group_name(name).ok_or_else(|| bad_name("group name empty or too long"))?;
    atomic(conn, |c| {
        group(c, id)?;
        if name_taken(c, &name, Some(id))? {
            return Err(bad_name("group name taken"));
        }
        c.execute(
            "UPDATE folder_groups SET name = ?2 WHERE id = ?1",
            params![id.to_bytes(), name],
        )
        .db()?;
        Ok(())
    })
}

/// Delete a group: its folders move to the default group first (nothing leaves search). Returns
/// how many folders moved. The default group can't be deleted (`InvalidInput`).
pub fn delete(conn: &Connection, id: GroupId) -> Result<u32> {
    atomic(conn, |c| {
        let g = group(c, id)?;
        if g.is_default {
            return Err(bad_name("the default group can't be deleted"));
        }
        let moved = c
            .execute(
                "UPDATE folders SET group_id = NULL WHERE group_id = ?1",
                [id.to_bytes()],
            )
            .db()?;
        c.execute("DELETE FROM folder_groups WHERE id = ?1", [id.to_bytes()])
            .db()?;
        Ok(u32::try_from(moved).unwrap_or(u32::MAX))
    })
}

/// Move a folder into a group (the default group is stored as NULL).
pub fn move_folder(conn: &Connection, folder: FolderId, to: GroupId, now: i64) -> Result<()> {
    atomic(conn, |c| {
        let g = group(c, to)?;
        let value = (!g.is_default).then(|| to.to_bytes());
        let n = c
            .execute(
                "UPDATE folders SET group_id = ?2, updated_at = ?3 WHERE id = ?1",
                params![folder.to_bytes(), value, now],
            )
            .db()?;
        if n == 0 {
            return Err(Error::new(ErrorCode::NotFound));
        }
        Ok(())
    })
}

/// Every folder's group, resolved (`NULL` → the default group). Needs the default group row.
pub fn folder_groups(conn: &Connection) -> Result<HashMap<FolderId, GroupId>> {
    let default = default_group(conn)?;
    let mut st = conn
        .prepare_cached("SELECT id, group_id FROM folders")
        .db()?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.folder_id(0)?,
                r.get::<_, Option<[u8; 16]>>(1)?.map(GroupId::from_bytes),
            ))
        })
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()?;
    let fallback = default.unwrap_or_else(|| GroupId::from_bytes([0; 16]));
    Ok(rows
        .into_iter()
        .map(|(f, g)| (f, g.unwrap_or(fallback)))
        .collect())
}

/// The group of one folder (resolved).
pub fn group_of(conn: &Connection, folder: FolderId) -> Result<GroupId> {
    let g: Option<[u8; 16]> = conn
        .query_row(
            "SELECT group_id FROM folders WHERE id = ?1",
            [folder.to_bytes()],
            |r| r.get(0),
        )
        .db()?;
    match g {
        Some(b) => Ok(GroupId::from_bytes(b)),
        None => Ok(default_group(conn)?.unwrap_or_else(|| GroupId::from_bytes([0; 16]))),
    }
}

/// Groups as the app shows them.
pub fn views(conn: &Connection) -> Result<Vec<GroupView>> {
    let map = folder_groups(conn)?;
    let mut by_group: HashMap<GroupId, Vec<FolderId>> = HashMap::new();
    for (f, g) in map {
        by_group.entry(g).or_default().push(f);
    }
    Ok(groups(conn)?
        .into_iter()
        .map(|g| {
            let mut folder_ids = by_group.remove(&g.id).unwrap_or_default();
            folder_ids.sort();
            GroupView {
                group_id: g.id,
                name: g.name,
                is_default: g.is_default,
                folder_ids,
                created_at: g.created_at,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::migrations;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations().to_latest(&mut c).unwrap();
        c.execute_batch(&format!(
            "INSERT INTO embedding_profiles(id, provider, model, model_revision, dims, pooling, variant_path,
                 variant_sha256, precision, tier, fingerprint, created_at)
                 VALUES (1, 'local', 'm', 'r', 4, 'cls', 'p', 's', 'fp32', 'minimum', x'{}', 1);
             INSERT INTO collections(id, name, embedding_profile_id, created_at) VALUES (x'{}', 'c', 1, 1);",
            "ab".repeat(32),
            "01".repeat(16),
        ))
        .unwrap();
        c
    }

    fn add_folder(c: &Connection, n: u8) -> FolderId {
        let id = FolderId::from_bytes([n; 16]);
        c.execute(
            "INSERT INTO folders(id, collection_id, kind, root_path, root_key, created_at, updated_at)
             VALUES (?1, ?2, 'indexed', ?3, ?3, 1, 1)",
            params![id.to_bytes(), [1u8; 16], format!("c:/f{n}")],
        )
        .unwrap();
        id
    }

    #[test]
    fn default_group_is_created_once_and_holds_new_folders() {
        let c = db();
        let d = ensure_default(&c, 1).unwrap();
        assert_eq!(ensure_default(&c, 2).unwrap(), d);
        let f = add_folder(&c, 9);
        assert_eq!(group_of(&c, f).unwrap(), d);
        let v = views(&c).unwrap();
        assert_eq!(v.len(), 1);
        assert!(v[0].is_default && v[0].name == DEFAULT_GROUP_NAME);
        assert_eq!(v[0].folder_ids, vec![f]);
    }

    #[test]
    fn create_rename_move_delete() {
        let c = db();
        let d = ensure_default(&c, 1).unwrap();
        let f = add_folder(&c, 9);
        let w = create(&c, " Work ", 2).unwrap();
        assert_eq!(group(&c, w).unwrap().name, "Work");
        assert_eq!(
            create(&c, "work", 3).unwrap_err().code,
            ErrorCode::InvalidInput,
            "names are unique, case-insensitive"
        );
        assert_eq!(
            create(&c, "  ", 3).unwrap_err().code,
            ErrorCode::InvalidInput
        );
        move_folder(&c, f, w, 4).unwrap();
        assert_eq!(group_of(&c, f).unwrap(), w);
        rename(&c, w, "Projects").unwrap();
        rename(&c, w, "projects").unwrap(); // its own name, other case
        assert_eq!(
            rename(&c, w, DEFAULT_GROUP_NAME).unwrap_err().code,
            ErrorCode::InvalidInput
        );
        assert_eq!(delete(&c, d).unwrap_err().code, ErrorCode::InvalidInput);
        assert_eq!(delete(&c, w).unwrap(), 1);
        assert_eq!(
            group_of(&c, f).unwrap(),
            d,
            "folders fall back, never removed"
        );
        let n: i64 = c
            .query_row("SELECT count(*) FROM folders", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            move_folder(&c, FolderId::new(), d, 5).unwrap_err().code,
            ErrorCode::NotFound
        );
    }
}
