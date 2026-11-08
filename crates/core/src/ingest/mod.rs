//! Ingest: extract client (M1-B, `extract_client`), chunking (M1-E), EmbedBatcher (later).

pub mod chunk;
pub mod extract_client; // M1-B

pub use chunk::{
    CHUNK_OVERLAP, CHUNK_TOKENS, CHUNKER_VERSION, Chunk, Chunker, CodeLang, ContentKind,
};
