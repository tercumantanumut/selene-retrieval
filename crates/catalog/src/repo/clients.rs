//! Connected apps (M4-E, page 29): one row per stdio app seen over the pipe and per HTTP tool,
//! with its folder scope. The scope is stored as JSON (`{"mode":"all"}` or
//! `{"mode":"groups","group_ids":[...]}`; V1's default `"all"` still reads as "All my folders").
//! A scope that can't be read grants **nothing** (fail closed, P1.7). HTTP tokens are never stored
//! here: `token_ref` names the Credential Manager entry and `token_hint` keeps the last 4 chars.

use rusqlite::{Connection, OptionalExtension, params};
use sr_common::api::ClientScope;
use sr_common::error::{ClientKind, Error, ErrorCode, Result};
use sr_common::ids::GroupId;

use crate::db::{DbResultExt, atomic};

/// One connected app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRecord {
    pub id: String,
    pub kind: ClientKind,
    pub display_name: String,
    pub scope: ClientScope,
    pub token_ref: Option<String>,
    pub token_hint: Option<String>,
    pub first_seen_at: Option<i64>,
    pub last_seen_at: Option<i64>,
}

/// `clients.kind` column values (V1 spells VS Code `vscode`).
pub fn kind_to_sql(k: ClientKind) -> &'static str {
    match k {
        ClientKind::ClaudeDesktop => "claude_desktop",
        ClientKind::Cursor => "cursor",
        ClientKind::VsCode => "vscode",
        ClientKind::Other => "other",
        ClientKind::Http => "http",
    }
}

pub fn kind_from_sql(s: &str) -> ClientKind {
    match s {
        "claude_desktop" => ClientKind::ClaudeDesktop,
        "cursor" => ClientKind::Cursor,
        "vscode" | "vs_code" => ClientKind::VsCode,
        "http" => ClientKind::Http,
        _ => ClientKind::Other,
    }
}

/// Stored scope → scope. `"all"` (V1 default) and `{"mode":"all"}` are "All my folders"; anything
/// unreadable is an empty group list (no access).
pub fn parse_scope(s: &str) -> ClientScope {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(serde_json::Value::String(v)) if v == "all" => ClientScope::All,
        Ok(v) => serde_json::from_value(v).unwrap_or(ClientScope::Groups {
            group_ids: Vec::new(),
        }),
        Err(_) => ClientScope::Groups {
            group_ids: Vec::new(),
        },
    }
}

fn scope_to_sql(s: &ClientScope) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| r#"{"mode":"groups","group_ids":[]}"#.into())
}

const COLS: &str =
    "id, kind, display_name, scope, token_ref, token_hint, connected_at, last_seen_at";

fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ClientRecord> {
    Ok(ClientRecord {
        id: r.get(0)?,
        kind: kind_from_sql(&r.get::<_, String>(1)?),
        display_name: r.get(2)?,
        scope: parse_scope(&r.get::<_, String>(3)?),
        token_ref: r.get(4)?,
        token_hint: r.get(5)?,
        first_seen_at: r.get(6)?,
        last_seen_at: r.get(7)?,
    })
}

pub fn client(conn: &Connection, id: &str) -> Result<Option<ClientRecord>> {
    conn.query_row(
        &format!("SELECT {COLS} FROM clients WHERE id = ?1"),
        [id],
        from_row,
    )
    .optional()
    .db()
}

/// Every client, by kind then name.
pub fn clients(conn: &Connection) -> Result<Vec<ClientRecord>> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {COLS} FROM clients ORDER BY kind, display_name COLLATE NOCASE, id"
        ))
        .db()?;
    st.query_map([], from_row)
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()
}

/// A pipe app was seen: insert it (scope "All my folders") or refresh `last_seen_at`. Returns
/// `true` when the row was created (its first session).
pub fn touch(
    conn: &Connection,
    id: &str,
    kind: ClientKind,
    display_name: &str,
    now: i64,
) -> Result<bool> {
    atomic(conn, |c| {
        let n = c
            .execute(
                "UPDATE clients SET last_seen_at = ?2 WHERE id = ?1",
                params![id, now],
            )
            .db()?;
        if n > 0 {
            return Ok(false);
        }
        c.execute(
            "INSERT INTO clients(id, kind, display_name, scope, connected_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![
                id,
                kind_to_sql(kind),
                display_name,
                scope_to_sql(&ClientScope::All),
                now
            ],
        )
        .db()?;
        Ok(true)
    })
}

/// A new HTTP tool row (scope "All my folders").
pub fn insert_http(
    conn: &Connection,
    id: &str,
    name: &str,
    token_ref: &str,
    token_hint: &str,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO clients(id, kind, display_name, scope, token_ref, token_hint, connected_at)
         VALUES (?1, 'http', ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            name,
            scope_to_sql(&ClientScope::All),
            token_ref,
            token_hint,
            now
        ],
    )
    .db()?;
    Ok(())
}

/// New token for an HTTP tool (rotation).
pub fn set_token(conn: &Connection, id: &str, token_ref: &str, token_hint: &str) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE clients SET token_ref = ?2, token_hint = ?3 WHERE id = ?1",
            params![id, token_ref, token_hint],
        )
        .db()?;
    if n == 0 {
        return Err(Error::new(ErrorCode::NotFound));
    }
    Ok(())
}

pub fn set_scope(conn: &Connection, id: &str, scope: &ClientScope) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE clients SET scope = ?2 WHERE id = ?1",
            params![id, scope_to_sql(scope)],
        )
        .db()?;
    if n == 0 {
        return Err(Error::new(ErrorCode::NotFound));
    }
    Ok(())
}

/// Forget a client. Returns whether a row was removed.
pub fn remove(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn
        .execute("DELETE FROM clients WHERE id = ?1", [id])
        .db()?
        > 0)
}

/// A deleted group leaves every scope (never widening it). Returns the ids of clients whose scope
/// now names no group at all ("no longer has access to any folders").
pub fn drop_group_from_scopes(conn: &Connection, group: GroupId) -> Result<Vec<String>> {
    atomic(conn, |c| {
        let mut empty = Vec::new();
        for rec in clients(c)? {
            if let ClientScope::Groups { group_ids } = &rec.scope
                && group_ids.contains(&group)
            {
                let left: Vec<GroupId> =
                    group_ids.iter().copied().filter(|g| *g != group).collect();
                if left.is_empty() {
                    empty.push(rec.id.clone());
                }
                set_scope(c, &rec.id, &ClientScope::Groups { group_ids: left })?;
            }
        }
        Ok(empty)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::migrations;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrations().to_latest(&mut c).unwrap();
        c
    }

    #[test]
    fn touch_creates_once_with_all_folders() {
        let c = db();
        assert!(touch(&c, "claude", ClientKind::ClaudeDesktop, "Claude Desktop", 5).unwrap());
        assert!(!touch(&c, "claude", ClientKind::ClaudeDesktop, "Claude Desktop", 9).unwrap());
        let r = client(&c, "claude").unwrap().unwrap();
        assert_eq!(r.scope, ClientScope::All);
        assert_eq!((r.first_seen_at, r.last_seen_at), (Some(5), Some(9)));
        // VS Code passes the V1 CHECK (`vscode`).
        assert!(touch(&c, "vscode", ClientKind::VsCode, "VS Code", 1).unwrap());
        assert_eq!(
            client(&c, "vscode").unwrap().unwrap().kind,
            ClientKind::VsCode
        );
    }

    #[test]
    fn scopes_fail_closed() {
        assert_eq!(parse_scope(r#""all""#), ClientScope::All);
        assert_eq!(parse_scope(r#"{"mode":"all"}"#), ClientScope::All);
        assert_eq!(
            parse_scope("garbage"),
            ClientScope::Groups {
                group_ids: Vec::new()
            }
        );
        assert_eq!(
            parse_scope(r#"{"mode":"everything"}"#),
            ClientScope::Groups {
                group_ids: Vec::new()
            }
        );
        let c = db();
        touch(&c, "cursor", ClientKind::Cursor, "Cursor", 1).unwrap();
        touch(&c, "claude", ClientKind::ClaudeDesktop, "Claude Desktop", 1).unwrap();
        let (a, b) = (GroupId::new(), GroupId::new());
        set_scope(&c, "cursor", &ClientScope::Groups { group_ids: vec![a] }).unwrap();
        set_scope(
            &c,
            "claude",
            &ClientScope::Groups {
                group_ids: vec![a, b],
            },
        )
        .unwrap();
        let empty = drop_group_from_scopes(&c, a).unwrap();
        assert_eq!(empty, vec!["cursor".to_string()]);
        assert_eq!(
            client(&c, "cursor").unwrap().unwrap().scope,
            ClientScope::Groups {
                group_ids: Vec::new()
            },
            "never widens to all"
        );
        assert_eq!(
            client(&c, "claude").unwrap().unwrap().scope,
            ClientScope::Groups { group_ids: vec![b] }
        );
        assert_eq!(
            set_scope(&c, "nobody", &ClientScope::All).unwrap_err().code,
            ErrorCode::NotFound
        );
    }

    #[test]
    fn http_tools_keep_only_a_hint() {
        let c = db();
        insert_http(&c, "http-01", "My script", "http-token:http-01", "a1B2", 3).unwrap();
        let r = client(&c, "http-01").unwrap().unwrap();
        assert_eq!(r.kind, ClientKind::Http);
        assert_eq!(r.token_hint.as_deref(), Some("a1B2"));
        set_token(&c, "http-01", "http-token:http-01", "zz99").unwrap();
        assert_eq!(
            client(&c, "http-01")
                .unwrap()
                .unwrap()
                .token_hint
                .as_deref(),
            Some("zz99")
        );
        assert!(remove(&c, "http-01").unwrap());
        assert!(!remove(&c, "http-01").unwrap());
    }
}
