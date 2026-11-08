//! Identifiers (docs/00-review-and-decisions.md §0.A C1).
//!
//! * ULID ids ([`FileId`], [`FolderId`], [`CollectionId`], [`ApprovalId`], [`TraceId`]) serialise as
//!   26-char Crockford strings. `ulid` 3.0.0 has no `serde` feature, so the newtypes implement it.
//! * 32-byte BLAKE3 ids serialise as 64-char lowercase hex:
//!   - [`ContentHash`] = blake3(file bytes)
//!   - [`ChunkCfgFp`]  = blake3(chunker_version ‖ splitter params ‖ tokenizer id ‖ extractor_version)
//!   - [`ChunkId`]     = blake3(file_id ‖ content_hash ‖ chunk_cfg_fp ‖ ord)
//!   - [`TextHash`]    = blake3(NFC(text)), used only for dedupe.
//!
//! Every concatenation uses fixed-width fields or u64-LE length prefixes, so distinct inputs can
//! never produce the same pre-image.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ulid::Ulid;
use unicode_normalization::UnicodeNormalization;

macro_rules! ulid_id {
    ($( $(#[$m:meta])* $name:ident ),+ $(,)?) => {$(
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Ulid);

        impl $name {
            /// A fresh, time-ordered id.
            pub fn new() -> Self { Self(Ulid::generate()) }
            pub const fn from_ulid(u: Ulid) -> Self { Self(u) }
            pub const fn ulid(&self) -> Ulid { self.0 }
            pub const fn to_bytes(&self) -> [u8; 16] { self.0.to_bytes() }
            pub const fn from_bytes(b: [u8; 16]) -> Self { Self(Ulid::from_bytes(b)) }
        }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Display::fmt(&self.0, f) }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
        impl FromStr for $name {
            type Err = ulid::DecodeError;
            fn from_str(s: &str) -> Result<Self, Self::Err> { Ulid::from_string(s).map(Self) }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0.to_string())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = Cow::<str>::deserialize(d)?;
                Ulid::from_string(&s).map(Self).map_err(serde::de::Error::custom)
            }
        }
        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> { Cow::Borrowed(stringify!($name)) }
            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({ "type": "string", "minLength": 26, "maxLength": 26,
                               "pattern": "^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$" })
            }
        }
    )+};
}

ulid_id! {
    /// Stable per-file id; survives renames (matched by volume serial + 128-bit FILE_ID_INFO).
    FileId,
    FolderId,
    CollectionId,
    ApprovalId,
    /// Correlates an error or request across logs and Details.
    TraceId,
    /// Identifies one daemon run's event stream; `seq` values are only comparable within it.
    StreamId,
    /// One vector index (`vector_indexes.id`): a Lance table built by exactly one profile.
    VectorIndexId,
    /// A folder group (M4, page 31): a label over folders that per-app scopes refer to.
    GroupId,
}

/// Error returned when parsing a hex digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HexError;
impl fmt::Display for HexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected 64 hex characters")
    }
}
impl std::error::Error for HexError {}

macro_rules! hash_id {
    ($( $(#[$m:meta])* $name:ident ),+ $(,)?) => {$(
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; 32]);

        impl $name {
            pub const fn from_bytes(b: [u8; 32]) -> Self { Self(b) }
            pub const fn as_bytes(&self) -> &[u8; 32] { &self.0 }
            pub fn to_hex(&self) -> String { blake3::Hash::from_bytes(self.0).to_hex().to_string() }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.to_hex()) }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), &self.to_hex()[..16])
            }
        }
        impl FromStr for $name {
            type Err = HexError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                blake3::Hash::from_hex(s).map(|h| Self(*h.as_bytes())).map_err(|_| HexError)
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_hex())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = Cow::<str>::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> { Cow::Borrowed(stringify!($name)) }
            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({ "type": "string", "pattern": "^[0-9a-f]{64}$" })
            }
        }
    )+};
}

hash_id! {
    /// blake3 of the raw file bytes.
    ContentHash,
    /// Fingerprint of everything that changes chunk boundaries.
    ChunkCfgFp,
    /// Identity of one chunk of one version of one file.
    ChunkId,
    /// blake3 of the NFC-normalised chunk text; dedupe key only (never an identity).
    TextHash,
}

impl ContentHash {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
}

fn update_field(h: &mut blake3::Hasher, field: &[u8]) {
    h.update(&(field.len() as u64).to_le_bytes());
    h.update(field);
}

impl ChunkCfgFp {
    /// `blake3(chunker_version ‖ splitter_params ‖ tokenizer_id ‖ extractor_version)`, each field
    /// length-prefixed. `splitter_params` should be a canonical string (e.g. sorted JSON).
    pub fn compute(
        chunker_version: &str,
        splitter_params: &str,
        tokenizer_id: &str,
        extractor_version: &str,
    ) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(b"sr.chunk_cfg_fp.v1");
        for f in [
            chunker_version,
            splitter_params,
            tokenizer_id,
            extractor_version,
        ] {
            update_field(&mut h, f.as_bytes());
        }
        Self(*h.finalize().as_bytes())
    }
}

impl ChunkId {
    /// `blake3(file_id ‖ content_hash ‖ chunk_cfg_fp ‖ ord)`. All four fields are fixed width
    /// (16 + 32 + 32 + 4 bytes, `ord` little-endian).
    pub fn derive(file_id: FileId, content_hash: &ContentHash, cfg: &ChunkCfgFp, ord: u32) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(&file_id.to_bytes());
        h.update(content_hash.as_bytes());
        h.update(cfg.as_bytes());
        h.update(&ord.to_le_bytes());
        Self(*h.finalize().as_bytes())
    }
}

impl TextHash {
    /// `blake3(NFC(text))`. Composed and decomposed forms of the same text hash equally.
    pub fn of_text(text: &str) -> Self {
        let mut h = blake3::Hasher::new();
        let mut buf = [0u8; 4];
        for ch in text.nfc() {
            h.update(ch.encode_utf8(&mut buf).as_bytes());
        }
        Self(*h.finalize().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn cfg() -> ChunkCfgFp {
        ChunkCfgFp::compute(
            "text-splitter-0.33",
            r#"{"size":512}"#,
            "bge-small-en-v1.5",
            "1",
        )
    }

    #[test]
    fn identical_content_in_two_files_gives_distinct_chunk_ids_but_equal_text_hashes() {
        let content = b"fn main() {}\n// same bytes in both files\n";
        let ch = ContentHash::of_bytes(content);
        let (a, b) = (FileId::new(), FileId::new());
        assert_ne!(a, b);
        let chunks = ["fn main() {}", "// same bytes in both files"];
        let mut ids = HashSet::new();
        for (ord, text) in chunks.iter().enumerate() {
            let ord = u32::try_from(ord).unwrap();
            let ca = ChunkId::derive(a, &ch, &cfg(), ord);
            let cb = ChunkId::derive(b, &ch, &cfg(), ord);
            assert_ne!(ca, cb, "different file_id must give different chunk_id");
            assert!(ids.insert(ca) && ids.insert(cb));
            assert_eq!(TextHash::of_text(text), TextHash::of_text(text));
        }
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn chunk_id_depends_on_every_input() {
        let f = FileId::new();
        let ch = ContentHash::of_bytes(b"x");
        let base = ChunkId::derive(f, &ch, &cfg(), 0);
        assert_eq!(base, ChunkId::derive(f, &ch, &cfg(), 0), "deterministic");
        assert_ne!(base, ChunkId::derive(f, &ch, &cfg(), 1));
        assert_ne!(
            base,
            ChunkId::derive(f, &ContentHash::of_bytes(b"y"), &cfg(), 0)
        );
        let other_cfg = ChunkCfgFp::compute("text-splitter-0.33", r#"{"size":256}"#, "bge", "1");
        assert_ne!(base, ChunkId::derive(f, &ch, &other_cfg, 0));
    }

    #[test]
    fn cfg_fp_fields_are_length_prefixed() {
        assert_ne!(
            ChunkCfgFp::compute("ab", "c", "", ""),
            ChunkCfgFp::compute("a", "bc", "", "")
        );
    }

    #[test]
    fn text_hash_is_nfc_normalised() {
        // "é" precomposed (U+00E9) vs "e" + combining acute (U+0065 U+0301)
        assert_eq!(
            TextHash::of_text("caf\u{e9}"),
            TextHash::of_text("cafe\u{301}")
        );
        assert_ne!(TextHash::of_text("cafe"), TextHash::of_text("caf\u{e9}"));
    }

    #[test]
    fn ids_round_trip_through_serde_as_strings() {
        let f = FileId::new();
        let j = serde_json::to_string(&f).unwrap();
        assert_eq!(j.len(), 28);
        assert_eq!(serde_json::from_str::<FileId>(&j).unwrap(), f);
        let c = ChunkId::derive(f, &ContentHash::of_bytes(b"z"), &cfg(), 7);
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<ChunkId>(&j).unwrap(), c);
        assert!(serde_json::from_str::<FileId>("\"not-a-ulid\"").is_err());
        assert!(serde_json::from_str::<ChunkId>("\"abcd\"").is_err());
    }

    #[test]
    fn ulids_are_time_ordered() {
        let a = FileId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = FileId::new();
        assert!(a < b);
    }
}
