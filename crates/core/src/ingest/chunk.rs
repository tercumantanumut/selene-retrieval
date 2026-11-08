//! Chunking (docs/02 §2.3, D2, Amendment 3.5).
//!
//! * **Fixed 400 tokens / 48 overlap for every tier**, counted with one chunker tokenizer (the
//!   bundled bge-small `tokenizer.json`, bert-base-uncased WordPiece, shared by every tier model),
//!   so a tier or model switch never changes chunk ids.
//! * Plain text uses `text_splitter::TextSplitter`, Markdown `MarkdownSplitter`, and code
//!   `CodeSplitter` with a tree-sitter grammar (Rust, Python, TypeScript/TSX, JavaScript, Go, Java,
//!   C#, C/C++). Other code extensions fall back to the text splitter.
//! * `chunk_cfg_fp = blake3(chunker_version ‖ splitter params ‖ tokenizer id ‖ extractor_version)`;
//!   the splitter params include the content kind (and grammar version), so changing how a file
//!   type is split re-chunks exactly the affected files.
//! * `chunk_id = blake3(file_id ‖ content_hash ‖ chunk_cfg_fp ‖ ord)` and `text_hash = blake3(NFC)`
//!   come from `sr_common::ids`.

use std::collections::HashMap;
use std::sync::Arc;

use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use sr_common::ids::{ChunkCfgFp, ChunkId, ContentHash, FileId, TextHash};
use text_splitter::{ChunkConfig, ChunkSizer, CodeSplitter, MarkdownSplitter, TextSplitter};
use tokenizers::Tokenizer;

use crate::guard;

/// Chunk size in tokens (Amendment 3.5: fixed for every tier).
pub const CHUNK_TOKENS: usize = 400;
/// Overlap between neighbouring chunks, in tokens.
pub const CHUNK_OVERLAP: usize = 48;
/// Most bytes in one chunk (the catalog refuses longer chunk text). Text the tokenizer sees as
/// very few tokens (words glued together by zero-width or bidi controls, one huge combining
/// sequence) could otherwise become a single chunk of several MB, and the whole file failed with
/// "That request wasn't valid" (docs/08 §5). Such a piece is
/// cut into windows of at most this size at character boundaries. Every chunk that could be
/// stored before is unchanged, so `CHUNKER_VERSION` stays.
pub const CHUNK_MAX_BYTES: usize = 1 << 20;
/// Bumped whenever chunk boundaries could change for the same inputs. Includes the splitter and
/// tree-sitter runtime versions (grammar versions are in [`CodeLang::grammar_id`]).
pub const CHUNKER_VERSION: &str = "sr-chunk/1;text-splitter/0.33;tree-sitter/0.27";

/// Languages with a tree-sitter grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeLang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
    Java,
    CSharp,
    Cpp,
}

impl CodeLang {
    pub const ALL: [CodeLang; 9] = [
        CodeLang::Rust,
        CodeLang::Python,
        CodeLang::TypeScript,
        CodeLang::Tsx,
        CodeLang::JavaScript,
        CodeLang::Go,
        CodeLang::Java,
        CodeLang::CSharp,
        CodeLang::Cpp,
    ];

    /// Map a lower-case extension (no dot) to a grammar.
    pub fn from_extension(ext: &str) -> Option<Self> {
        Some(match ext {
            "rs" => Self::Rust,
            "py" | "pyi" | "pyw" => Self::Python,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "go" => Self::Go,
            "java" => Self::Java,
            "cs" => Self::CSharp,
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "inl" => Self::Cpp,
            _ => return None,
        })
    }

    /// Stable short name, also the tantivy `lang` value.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
            Self::Go => "go",
            Self::Java => "java",
            Self::CSharp => "csharp",
            Self::Cpp => "cpp",
        }
    }

    /// Grammar crate + version: part of `chunk_cfg_fp`.
    pub const fn grammar_id(self) -> &'static str {
        match self {
            Self::Rust => "tree-sitter-rust/0.24.2",
            Self::Python => "tree-sitter-python/0.25.0",
            Self::TypeScript | Self::Tsx => "tree-sitter-typescript/0.23.2",
            Self::JavaScript => "tree-sitter-javascript/0.25.0",
            Self::Go => "tree-sitter-go/0.25.0",
            Self::Java => "tree-sitter-java/0.23.5",
            Self::CSharp => "tree-sitter-c-sharp/0.23.5",
            Self::Cpp => "tree-sitter-cpp/0.23.4",
        }
    }

    fn language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        }
    }
}

/// How a file's text is split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentKind {
    PlainText,
    Markdown,
    Code(CodeLang),
}

impl ContentKind {
    /// Kind for a file extension (case-insensitive, with or without the dot).
    pub fn from_extension(ext: &str) -> Self {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        match ext.as_str() {
            "md" | "markdown" | "mdx" | "mkd" => Self::Markdown,
            e => CodeLang::from_extension(e).map_or(Self::PlainText, Self::Code),
        }
    }

    /// Canonical id used in `chunk_cfg_fp`.
    pub fn cfg_id(self) -> String {
        match self {
            Self::PlainText => "text".into(),
            Self::Markdown => "markdown".into(),
            Self::Code(l) => format!("code:{}@{}", l.name(), l.grammar_id()),
        }
    }

    /// The `lang` value stored in the lexical index.
    pub fn lang(self) -> &'static str {
        match self {
            Self::PlainText => "text",
            Self::Markdown => "markdown",
            Self::Code(l) => l.name(),
        }
    }
}

/// One chunk of one version of one file.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub chunk_id: ChunkId,
    pub ord: u32,
    /// Byte range in the extracted UTF-8 text.
    pub byte_start: u64,
    pub byte_end: u64,
    /// 1-based, inclusive.
    pub line_start: u32,
    pub line_end: u32,
    pub tokens: u32,
    pub text_hash: TextHash,
    /// The chunk text (goes to `chunk_text`, zstd, refcounted; the derived stores never store it).
    pub text: String,
}

/// Token counter for the splitters. Unlike text-splitter's own `impl ChunkSizer for Tokenizer`, it
/// never panics: a tokenizer error counts the chunk as one token per byte (always splits further).
#[derive(Clone)]
struct TokenSizer(Arc<Tokenizer>);

impl ChunkSizer for TokenSizer {
    fn size(&self, chunk: &str) -> usize {
        match self.0.encode_fast(chunk, false) {
            Ok(enc) => enc.get_ids().len(),
            Err(_) => chunk.len(),
        }
    }
}

/// The chunker. Cheap to share (`Arc`); every splitter is built once.
pub struct Chunker {
    sizer: TokenSizer,
    tokenizer_id: String,
    text: TextSplitter<TokenSizer>,
    markdown: MarkdownSplitter<TokenSizer>,
    code: HashMap<CodeLang, CodeSplitter<TokenSizer>>,
}

impl std::fmt::Debug for Chunker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chunker")
            .field("tokenizer_id", &self.tokenizer_id)
            .finish_non_exhaustive()
    }
}

fn internal(detail: impl Into<String>) -> Error {
    Error::new(ErrorCode::Internal).with_detail(detail)
}

impl Chunker {
    /// Build from the bundled bge-small `tokenizer.json` bytes. Truncation and padding are turned
    /// off so token counts reflect the whole text.
    pub fn from_tokenizer_json(bytes: &[u8]) -> Result<Self> {
        let mut tok = Tokenizer::from_bytes(bytes)
            .map_err(|e| internal(format!("chunker tokenizer: {e}")))?;
        tok.with_truncation(None)
            .map_err(|e| internal(format!("chunker tokenizer truncation: {e}")))?;
        tok.with_padding(None);
        let tokenizer_id = format!("wordpiece:{}", &ContentHash::of_bytes(bytes).to_hex()[..16]);
        Self::new(Arc::new(tok), tokenizer_id)
    }

    fn new(tok: Arc<Tokenizer>, tokenizer_id: String) -> Result<Self> {
        let sizer = TokenSizer(tok);
        let cfg = || -> Result<ChunkConfig<TokenSizer>> {
            ChunkConfig::new(CHUNK_TOKENS)
                .with_overlap(CHUNK_OVERLAP)
                .map(|c| c.with_sizer(sizer.clone()))
                .map_err(|e| internal(format!("chunk config: {e}")))
        };
        let mut code = HashMap::new();
        for lang in CodeLang::ALL {
            let splitter = CodeSplitter::new(lang.language(), cfg()?)
                .map_err(|e| internal(format!("grammar {}: {e}", lang.name())))?;
            code.insert(lang, splitter);
        }
        Ok(Self {
            text: TextSplitter::new(cfg()?),
            markdown: MarkdownSplitter::new(cfg()?),
            code,
            sizer,
            tokenizer_id,
        })
    }

    /// Identifies the chunker tokenizer (hash of the tokenizer file).
    pub fn tokenizer_id(&self) -> &str {
        &self.tokenizer_id
    }

    /// `chunk_cfg_fp` for a file of `kind` extracted by `extractor_version`.
    pub fn cfg_fp(&self, kind: ContentKind, extractor_version: &str) -> ChunkCfgFp {
        let params = format!(
            "{{\"capacity\":{CHUNK_TOKENS},\"kind\":\"{}\",\"overlap\":{CHUNK_OVERLAP},\"trim\":true}}",
            kind.cfg_id()
        );
        ChunkCfgFp::compute(
            CHUNKER_VERSION,
            &params,
            &self.tokenizer_id,
            extractor_version,
        )
    }

    /// Token count of `text` with the chunker tokenizer.
    pub fn count_tokens(&self, text: &str) -> usize {
        self.sizer.size(text)
    }

    /// Split one file's extracted text. Deterministic: the same inputs always give the same
    /// chunks, ids and hashes. Whitespace-only text gives no chunks.
    pub fn chunk(
        &self,
        text: &str,
        kind: ContentKind,
        file_id: FileId,
        content_hash: &ContentHash,
        extractor_version: &str,
    ) -> Result<Vec<Chunk>> {
        let cfg = self.cfg_fp(kind, extractor_version);
        guard::blocking("chunk", || {
            let pieces: Vec<(usize, &str)> = match kind {
                ContentKind::PlainText => self.text.chunk_indices(text).collect(),
                ContentKind::Markdown => self.markdown.chunk_indices(text).collect(),
                ContentKind::Code(lang) => match self.code.get(&lang) {
                    Some(s) => s.chunk_indices(text).collect(),
                    None => self.text.chunk_indices(text).collect(),
                },
            };
            let lines = LineIndex::new(text);
            let mut out = Vec::with_capacity(pieces.len());
            let pieces = pieces
                .into_iter()
                .flat_map(|(offset, piece)| byte_windows(offset, piece, CHUNK_MAX_BYTES));
            for (offset, piece) in pieces {
                if piece.trim().is_empty() {
                    continue;
                }
                let ord = u32::try_from(out.len()).map_err(|_| internal("too many chunks"))?;
                let end = offset + piece.len();
                out.push(Chunk {
                    chunk_id: ChunkId::derive(file_id, content_hash, &cfg, ord),
                    ord,
                    byte_start: offset as u64,
                    byte_end: end as u64,
                    line_start: lines.line_of(offset),
                    line_end: lines.line_of(end.saturating_sub(1).max(offset)),
                    tokens: u32::try_from(self.sizer.size(piece)).unwrap_or(u32::MAX),
                    text_hash: TextHash::of_text(piece),
                    text: piece.to_string(),
                });
            }
            Ok(out)
        })
    }
}

/// `piece` (at `offset` in the text) as consecutive windows of at most `max` bytes, cut at
/// character boundaries (one window when it already fits). A character longer than `max` (never
/// with `max` ≥ 4) stays whole.
fn byte_windows(offset: usize, piece: &str, max: usize) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut rest = piece;
    let mut at = offset;
    while rest.len() > max {
        let mut cut = max;
        while cut > 0 && !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut == 0 {
            cut = rest.char_indices().nth(1).map_or(rest.len(), |(i, _)| i);
        }
        let (head, tail) = rest.split_at(cut);
        out.push((at, head));
        at += cut;
        rest = tail;
    }
    if !rest.is_empty() || out.is_empty() {
        out.push((at, rest));
    }
    out
}

/// Byte offset → 1-based line number.
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { starts }
    }

    fn line_of(&self, byte: usize) -> u32 {
        let idx = match self.starts.binary_search(&byte) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        u32::try_from(idx + 1).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The bundled bge-small tokenizer (MIT, BAAI), copied as a test fixture.
    pub(crate) const TOKENIZER_JSON: &[u8] =
        include_bytes!("../../testdata/bge-small-tokenizer.json");

    pub(crate) fn chunker() -> Chunker {
        Chunker::from_tokenizer_json(TOKENIZER_JSON).unwrap()
    }

    fn long_prose(paragraphs: usize) -> String {
        (0..paragraphs)
            .map(|p| {
                (0..12)
                    .map(|s| {
                        format!(
                            "Paragraph {p} sentence {s} talks about indexing, search quality and \
                             the way retrieval should feel instant for everyone."
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            // One long paragraph: text-splitter overlaps in whole units of the semantic level
            // it split at, so sentence-level splitting is what exercises the 48-token overlap.
            .join(" ")
    }

    #[test]
    fn chunks_respect_the_fixed_token_budget_and_overlap() {
        let c = chunker();
        let text = long_prose(30);
        let f = FileId::new();
        let h = ContentHash::of_bytes(text.as_bytes());
        let chunks = c.chunk(&text, ContentKind::PlainText, f, &h, "1").unwrap();
        assert!(chunks.len() > 5, "{} chunks", chunks.len());
        for (i, ch) in chunks.iter().enumerate() {
            assert!(
                ch.tokens as usize <= CHUNK_TOKENS,
                "chunk {i}: {} tokens",
                ch.tokens
            );
            assert_eq!(ch.ord as usize, i);
            assert_eq!(
                &text[ch.byte_start as usize..ch.byte_end as usize],
                ch.text,
                "byte range matches text"
            );
            assert_eq!(ch.text_hash, TextHash::of_text(&ch.text));
        }
        // overlap: consecutive chunks share text
        let overlapping = chunks
            .windows(2)
            .filter(|w| w[1].byte_start < w[0].byte_end)
            .count();
        assert!(overlapping > 0, "expected overlapping neighbours");
    }

    /// docs/08 §5: text the tokenizer reads as a handful of tokens (the extract-sandbox
    /// fixtures `zalgo.txt` and `bidi-zw.txt`) became one multi-MB chunk, which the catalog
    /// refuses, so the whole file failed. It is now cut into windows the catalog accepts.
    #[test]
    fn text_with_almost_no_tokens_is_cut_into_storable_windows() {
        let c = chunker();
        for text in [
            format!("a{}", "\u{0301}".repeat(600_000)),
            "\u{202e}\u{200b}\u{2066}evil\u{2069}\u{feff}".repeat(70_000),
        ] {
            let h = ContentHash::of_bytes(text.as_bytes());
            let chunks = c
                .chunk(&text, ContentKind::PlainText, FileId::new(), &h, "1")
                .unwrap();
            assert!(chunks.len() >= 2, "{} chunks", chunks.len());
            let mut end = 0u64;
            for ch in &chunks {
                assert!(ch.text.len() <= CHUNK_MAX_BYTES, "{} bytes", ch.text.len());
                assert_eq!(&text[ch.byte_start as usize..ch.byte_end as usize], ch.text);
                end = end.max(ch.byte_end);
            }
            assert_eq!(end as usize, text.len(), "nothing is dropped");
        }
    }

    #[test]
    fn byte_windows_cut_at_character_boundaries() {
        assert_eq!(byte_windows(5, "abc", 8), [(5, "abc")]);
        assert_eq!(
            byte_windows(0, "abcdefg", 3),
            [(0, "abc"), (3, "def"), (6, "g")]
        );
        // "é" is 2 bytes: a window never splits it.
        assert_eq!(byte_windows(0, "aéé", 2), [(0, "a"), (1, "é"), (3, "é")]);
        // A character longer than the window stays whole.
        assert_eq!(byte_windows(0, "€x", 2), [(0, "€"), (3, "x")]);
    }

    #[test]
    fn chunking_is_deterministic_and_ids_depend_on_file_identity() {
        let c = chunker();
        let text = long_prose(8);
        let h = ContentHash::of_bytes(text.as_bytes());
        let (a, b) = (FileId::new(), FileId::new());
        let ca = c.chunk(&text, ContentKind::PlainText, a, &h, "1").unwrap();
        let ca2 = c.chunk(&text, ContentKind::PlainText, a, &h, "1").unwrap();
        let cb = c.chunk(&text, ContentKind::PlainText, b, &h, "1").unwrap();
        assert_eq!(ca, ca2);
        assert_eq!(ca.len(), cb.len());
        for (x, y) in ca.iter().zip(&cb) {
            assert_ne!(
                x.chunk_id, y.chunk_id,
                "duplicate files must not share chunk ids"
            );
            assert_eq!(x.text_hash, y.text_hash, "but share text hashes for dedupe");
        }
    }

    #[test]
    fn cfg_fp_changes_with_kind_and_extractor_but_not_with_anything_else() {
        let c = chunker();
        let t = c.cfg_fp(ContentKind::PlainText, "1");
        assert_eq!(t, chunker().cfg_fp(ContentKind::PlainText, "1"));
        assert_ne!(t, c.cfg_fp(ContentKind::Markdown, "1"));
        assert_ne!(t, c.cfg_fp(ContentKind::PlainText, "2"));
        assert_ne!(
            c.cfg_fp(ContentKind::Code(CodeLang::Rust), "1"),
            c.cfg_fp(ContentKind::Code(CodeLang::Python), "1")
        );
    }

    #[test]
    fn code_is_split_on_syntax_boundaries() {
        let c = chunker();
        let mut src = String::new();
        for i in 0..60 {
            src.push_str(&format!(
                "/// Doc for function {i}.\npub fn handler_{i}(input: &str) -> usize {{\n    \
                 let trimmed = input.trim();\n    let words = trimmed.split_whitespace().count();\n    \
                 words * {i} + trimmed.len()\n}}\n\n"
            ));
        }
        let h = ContentHash::of_bytes(src.as_bytes());
        let kind = ContentKind::from_extension("RS");
        assert_eq!(kind, ContentKind::Code(CodeLang::Rust));
        let chunks = c.chunk(&src, kind, FileId::new(), &h, "1").unwrap();
        assert!(chunks.len() >= 2);
        for ch in &chunks {
            assert!(ch.tokens as usize <= CHUNK_TOKENS);
            // a chunk never starts in the middle of a function body
            let first = ch.text.lines().next().unwrap_or("");
            assert!(
                first.starts_with("///") || first.starts_with("pub fn") || first.starts_with('}'),
                "chunk starts mid-item: {first:?}"
            );
        }
        assert_eq!(chunks[0].line_start, 1);
        assert!(chunks.last().unwrap().line_end >= 400);
    }

    #[test]
    fn markdown_and_empty_inputs() {
        let c = chunker();
        let md = "# Title\n\nSome intro.\n\n## Section\n\n- a\n- b\n";
        let h = ContentHash::of_bytes(md.as_bytes());
        let chunks = c
            .chunk(
                md,
                ContentKind::from_extension(".md"),
                FileId::new(),
                &h,
                "1",
            )
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!((chunks[0].line_start, chunks[0].line_end), (1, 8));
        let empty = c
            .chunk("  \n\t\n", ContentKind::PlainText, FileId::new(), &h, "1")
            .unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn line_index_maps_offsets() {
        let li = LineIndex::new("a\nbc\n\nd");
        assert_eq!(li.line_of(0), 1);
        assert_eq!(li.line_of(1), 1);
        assert_eq!(li.line_of(2), 2);
        assert_eq!(li.line_of(5), 3);
        assert_eq!(li.line_of(6), 4);
    }
}
