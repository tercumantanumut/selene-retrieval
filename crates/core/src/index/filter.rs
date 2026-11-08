//! The one Lance filter builder (docs/02 §2.6 "Filter injection (W3)").
//!
//! Filters are built only from typed ids. ULIDs render as Crockford base32 and BLAKE3 ids as
//! lowercase hex, so a literal can never contain a quote; [`lit`] still checks the charset and
//! escapes defensively. Selene's audit found string-built `IN (...)` lists with unescaped ids
//! (`indexing.ts:325`); this module is the only place SQL text for Lance is produced.

use sr_common::ids::{ChunkId, CollectionId, FileId, FolderId, TextHash};

use super::lance::{COL_CHUNK_ID, COL_COLLECTION_ID, COL_FILE_ID, COL_FOLDER_ID, COL_TEXT_HASH};

/// A typed predicate over the Lance filter columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    CollectionIn(Vec<CollectionId>),
    FolderIn(Vec<FolderId>),
    FileIn(Vec<FileId>),
    ChunkIn(Vec<ChunkId>),
    TextHashIn(Vec<TextHash>),
    And(Vec<Filter>),
    Or(Vec<Filter>),
}

/// Quote one literal. Panics are impossible: unexpected characters are dropped (and would make
/// the literal match nothing), quotes are doubled.
fn lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '~' => out.push(c),
            _ => {}
        }
    }
    out.push('\'');
    out
}

fn in_list<I: IntoIterator<Item = String>>(col: &str, vals: I) -> String {
    let items: Vec<String> = vals.into_iter().map(|v| lit(&v)).collect();
    match items.len() {
        // `x IN ()` is not valid SQL; an empty set matches nothing.
        0 => "false".to_string(),
        1 => format!("{col} = {}", items[0]),
        _ => format!("{col} IN ({})", items.join(", ")),
    }
}

impl Filter {
    /// Render as a Lance/DataFusion SQL predicate.
    pub fn to_sql(&self) -> String {
        match self {
            Filter::CollectionIn(v) => in_list(COL_COLLECTION_ID, v.iter().map(|x| x.to_string())),
            Filter::FolderIn(v) => in_list(COL_FOLDER_ID, v.iter().map(|x| x.to_string())),
            Filter::FileIn(v) => in_list(COL_FILE_ID, v.iter().map(|x| x.to_string())),
            Filter::ChunkIn(v) => in_list(COL_CHUNK_ID, v.iter().map(|x| x.to_hex())),
            Filter::TextHashIn(v) => in_list(COL_TEXT_HASH, v.iter().map(|x| x.to_hex())),
            Filter::And(parts) => join(parts, " AND ", "true"),
            Filter::Or(parts) => join(parts, " OR ", "false"),
        }
    }
}

fn join(parts: &[Filter], sep: &str, empty: &str) -> String {
    match parts.len() {
        0 => empty.to_string(),
        1 => parts[0].to_sql(),
        _ => parts
            .iter()
            .map(|p| format!("({})", p.to_sql()))
            .collect::<Vec<_>>()
            .join(sep),
    }
}

/// The scope a query may see: always at least one collection, optionally narrowed to folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub collections: Vec<CollectionId>,
    pub folders: Option<Vec<FolderId>>,
}

impl Scope {
    pub fn collection(id: CollectionId) -> Self {
        Self {
            collections: vec![id],
            folders: None,
        }
    }

    pub fn filter(&self) -> Filter {
        let mut parts = vec![Filter::CollectionIn(self.collections.clone())];
        if let Some(f) = &self.folders {
            parts.push(Filter::FolderIn(f.clone()));
        }
        Filter::And(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn renders_typed_lists() {
        let c = CollectionId::new();
        assert_eq!(
            Filter::CollectionIn(vec![c]).to_sql(),
            format!("collection_id = '{c}'")
        );
        assert_eq!(Filter::FileIn(vec![]).to_sql(), "false");
        let (a, b) = (FolderId::new(), FolderId::new());
        let s = Scope {
            collections: vec![c],
            folders: Some(vec![a, b]),
        }
        .filter()
        .to_sql();
        assert_eq!(
            s,
            format!("(collection_id = '{c}') AND (folder_id IN ('{a}', '{b}'))")
        );
    }

    #[test]
    fn literal_escaping_is_defensive() {
        assert_eq!(lit("a'b"), "'a''b'");
        assert_eq!(lit("x); DROP --"), "'xDROP--'");
    }

    proptest! {
        /// Fuzz: any set of typed ids renders to SQL made only of the expected tokens; a quote can
        /// only appear as a literal delimiter.
        #[test]
        fn rendered_sql_has_no_injection_surface(
            n_files in 0usize..20,
            n_chunks in 0usize..20,
            seed in any::<[u8; 32]>(),
        ) {
            let files: Vec<FileId> = (0..n_files).map(|i| {
                let mut b = [0u8; 16];
                b.copy_from_slice(&seed[..16]);
                b[0] ^= i as u8;
                FileId::from_bytes(b)
            }).collect();
            let chunks: Vec<ChunkId> = (0..n_chunks).map(|i| {
                let mut b = seed;
                b[31] ^= i as u8;
                ChunkId::from_bytes(b)
            }).collect();
            let sql = Filter::Or(vec![Filter::FileIn(files), Filter::ChunkIn(chunks)]).to_sql();
            let allowed = |c: char| c.is_ascii_alphanumeric() || " ,'()_=".contains(c);
            prop_assert!(sql.chars().all(allowed), "{}", sql);
            // literals are balanced
            prop_assert_eq!(sql.matches('\'').count() % 2, 0);
        }
    }
}
