//! Shared fixtures for sr-catalog integration tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use sr_catalog::repo::chunks::{FileChunks, NewChunk, T1Outcome};
use sr_catalog::repo::files::FileObservation;
use sr_catalog::repo::folders::NewFolder;
use sr_catalog::repo::profiles::{self, ProfileSpec};
use sr_catalog::repo::{chunks, files, folders};
use sr_catalog::{Catalog, FolderKind, ProfileId};
use sr_common::config::FolderSettings;
use sr_common::ids::{ChunkCfgFp, CollectionId, ContentHash, FileId, FolderId};
use sr_common::tier::{MINIMUM, TierProfile};

pub const NOW: i64 = 1_800_000_000_000;

/// `cat` is declared first so it is dropped (writer joined) before the temp dir is removed.
pub struct Fx {
    pub cat: Catalog,
    pub collection: CollectionId,
    pub profile: ProfileId,
    pub path: PathBuf,
    pub dir: tempfile::TempDir,
}

pub fn minimum_spec() -> ProfileSpec {
    let p: &TierProfile = &MINIMUM;
    ProfileSpec::local(p, &p.cpu_file)
}

pub fn setup() -> Fx {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("catalog.db");
    let cat = Catalog::open(&path).expect("open");
    let (profile, collection) = cat
        .write_blocking(|c| {
            let p = profiles::ensure_profile(c, &minimum_spec(), NOW)?;
            let col = profiles::ensure_default_collection(c, p, NOW)?;
            Ok((p, col))
        })
        .expect("seed");
    Fx {
        cat,
        collection,
        profile,
        path,
        dir,
    }
}

impl Fx {
    /// Re-open from disk (simulates a daemon restart / crash after the last commit).
    pub fn reopen(&mut self) {
        let fresh = Catalog::open(&self.path).expect("reopen");
        self.cat = fresh;
    }

    pub fn root(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

pub fn add_folder(cat: &Catalog, collection: CollectionId, root: &Path) -> FolderId {
    let f = NewFolder {
        collection_id: collection,
        kind: FolderKind::Indexed,
        root_path: root.to_path_buf(),
        volume_serial: Some(7),
        is_primary: false,
        settings: FolderSettings::default(),
    };
    cat.write_blocking(move |c| folders::insert_folder(c, &f, NOW))
        .expect("insert folder")
}

pub fn obs(rel: &str, ntfs: u8) -> FileObservation {
    FileObservation {
        rel_path: rel.to_string(),
        path_key: sr_common::sandbox::path_key(rel),
        ext: rel.rsplit_once('.').map_or("", |(_, e)| e).to_lowercase(),
        volume_serial: Some(7),
        ntfs_file_id: Some([ntfs; 16]),
        size: 10,
        mtime_ns: 1,
    }
}

/// Observe one file in a fresh scan.
pub fn add_file(cat: &Catalog, folder: FolderId, rel: &str, ntfs: u8) -> FileId {
    let o = obs(rel, ntfs);
    cat.write_blocking(move |c| {
        let scan = files::begin_scan(c, folder, NOW)?;
        Ok(files::observe(c, folder, scan, &o, NOW)?.file_id)
    })
    .expect("observe")
}

pub fn cfg() -> ChunkCfgFp {
    ChunkCfgFp::compute(
        "text-splitter-0.33",
        r#"{"overlap":48,"tokens":400}"#,
        sr_common::tier::CHUNKER_TOKENIZER_ID,
        "1",
    )
}

/// Chunks = lines of `content`.
pub fn file_chunks(file: FileId, content: &str) -> FileChunks {
    let mut off = 0u64;
    let chunks = content
        .lines()
        .enumerate()
        .map(|(i, l)| {
            let start = off;
            off += l.len() as u64 + 1;
            NewChunk {
                ord: u32::try_from(i).unwrap_or(u32::MAX),
                byte_start: start,
                byte_end: start + l.len() as u64,
                line_start: u32::try_from(i + 1).unwrap_or(u32::MAX),
                line_end: u32::try_from(i + 1).unwrap_or(u32::MAX),
                tokens: u32::try_from(l.split_whitespace().count()).unwrap_or(0),
                text: l.to_string(),
            }
        })
        .collect();
    FileChunks {
        file_id: file,
        content_hash: ContentHash::of_bytes(content.as_bytes()),
        chunk_cfg_fp: cfg(),
        extractor_version: "1".into(),
        chunks,
    }
}

pub fn t1(cat: &Catalog, file: FileId, content: &str) -> T1Outcome {
    let fc = file_chunks(file, content);
    cat.write_blocking(move |c| chunks::write_file_chunks(c, &fc, NOW))
        .expect("T1")
}
