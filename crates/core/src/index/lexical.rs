//! tantivy BM25 store (D6): one index per collection (fixes IDF skew; purge of a removed
//! collection is a directory delete).
//!
//! Schema: `chunk_id` (STRING, STORED), `file_id` / `folder_id` / `collection_id` / `lang`
//! (STRING), `body` ([`CodeTokenizer`], freqs + positions, **not stored**: snippets are hydrated
//! from the catalog), `path` (CodeTokenizer, boosted at query time, not stored), `exact`
//! ([`ExactTokenizer`], freqs, **no length norms**: whole identifiers, compound tokens and digit
//! runs from body and path, R-UX33; searched by [`LexicalIndex::search_exact`]).
//!
//! **Schema changes rebuild themselves.** An index written by an older build (other schema) is
//! recreated empty at open ([`LexicalIndex::recreated`]); the engine then re-queues every current
//! file of the collection through the outbox, so search keeps serving the vectors meanwhile and
//! the lexical lane fills in the background ("Catching up").
//!
//! Each commit carries the outbox watermark as its payload (`{"seq":N}`), so data and watermark
//! land atomically. Deletes are by `file_id` term; a folder purge deletes by `folder_id` term and
//! is followed by a merge + garbage collection so the terms leave the segment files.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use sr_common::ids::{ChunkId, CollectionId, FileId, FolderId};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery, TermSetQuery};
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term, doc};

use super::datadir::validate_store_root;
use crate::guard;

/// Registered tokenizer name.
pub const CODE_TOKENIZER: &str = "sr_code";
/// Registered name of the exact-term tokenizer (R-UX33).
pub const EXACT_TOKENIZER: &str = "sr_exact";
/// Tokens longer than this (minified code, base64 blobs) are not indexed.
pub const MAX_TOKEN_BYTES: usize = 64;
/// Query tokens beyond this are ignored.
const MAX_QUERY_TOKENS: usize = 64;

/// Lexical store knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LexConfig {
    /// Writer heap (D6: 128–256 MB).
    pub writer_heap_bytes: usize,
    pub writer_threads: usize,
    /// Query-time boost of the `path` field relative to `body`.
    pub path_boost: f32,
}

/// Minimum length of a digit run inside a word that is indexed as its own exact term
/// (`ticket4711` → `4711`; single digits are too common to help).
pub const EXACT_MIN_DIGIT_RUN: usize = 2;

impl Default for LexConfig {
    fn default() -> Self {
        Self {
            writer_heap_bytes: 128 * 1024 * 1024,
            writer_threads: 2,
            path_boost: 2.0,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// CodeTokenizer
// ---------------------------------------------------------------------------------------------

/// Tokenizer for prose and code. A word is a maximal run of alphanumerics and `_`. Each word is
/// indexed lower-cased as a whole (`handle_request`, `httpserver`, `tkn01234`) **and**, at the
/// same position, as its identifier parts split on `_`, camelCase, acronym boundaries and
/// letter/digit changes (`handle`, `request`; `http`, `server`; `tkn`, `01234`). So `HTTPServer`
/// is found by "http server", "HttpServer" and "httpserver".
#[derive(Clone, Copy, Default)]
pub struct CodeTokenizer;

/// Stream over pre-computed tokens.
pub struct CodeTokenStream {
    tokens: Vec<Token>,
    cursor: usize,
    empty: Token,
}

impl Tokenizer for CodeTokenizer {
    type TokenStream<'a> = CodeTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> CodeTokenStream {
        let mut tokens = Vec::new();
        tokenize_into(text, &mut tokens);
        CodeTokenStream {
            tokens,
            cursor: 0,
            empty: Token::default(),
        }
    }
}

impl TokenStream for CodeTokenStream {
    fn advance(&mut self) -> bool {
        if self.cursor < self.tokens.len() {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        self.cursor
            .checked_sub(1)
            .and_then(|i| self.tokens.get(i))
            .unwrap_or(&self.empty)
    }

    fn token_mut(&mut self) -> &mut Token {
        match self.cursor.checked_sub(1) {
            Some(i) if i < self.tokens.len() => &mut self.tokens[i],
            _ => &mut self.empty,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Lower,
    Upper,
    Digit,
    Other,
}

fn class(c: char) -> Class {
    if c.is_numeric() {
        Class::Digit
    } else if c.is_uppercase() {
        Class::Upper
    } else if c.is_lowercase() {
        Class::Lower
    } else {
        Class::Other
    }
}

/// Byte ranges of the identifier parts of `word` (no `_`).
fn identifier_parts(word: &str) -> Vec<(usize, usize)> {
    let mut parts = Vec::new();
    let mut base = 0usize;
    for seg in word.split('_') {
        let chars: Vec<(usize, char)> = seg.char_indices().collect();
        let mut start = 0usize;
        for i in 1..chars.len() {
            let (prev, cur) = (class(chars[i - 1].1), class(chars[i].1));
            let next = chars.get(i + 1).map(|c| class(c.1));
            let boundary = (prev == Class::Lower && cur == Class::Upper)
                || (prev == Class::Upper && cur == Class::Upper && next == Some(Class::Lower))
                || ((prev == Class::Digit) != (cur == Class::Digit)
                    && prev != Class::Other
                    && cur != Class::Other);
            if boundary {
                parts.push((base + start, base + chars[i].0));
                start = chars[i].0;
            }
        }
        if !seg.is_empty() {
            parts.push((base + start, base + seg.len()));
        }
        base += seg.len() + 1;
    }
    parts
}

fn push_token(out: &mut Vec<Token>, text: String, from: usize, to: usize, position: usize) {
    if text.is_empty() || text.len() > MAX_TOKEN_BYTES {
        return;
    }
    out.push(Token {
        offset_from: from,
        offset_to: to,
        position,
        text,
        position_length: 1,
    });
}

fn tokenize_into(text: &str, out: &mut Vec<Token>) {
    let mut position = 0usize;
    let mut run_start: Option<usize> = None;
    let flush = |start: usize, end: usize, out: &mut Vec<Token>, position: &mut usize| {
        let raw = &text[start..end];
        let trimmed = raw.trim_matches('_');
        if trimmed.is_empty() {
            return;
        }
        let off = start + (raw.len() - raw.trim_start_matches('_').len());
        let whole = trimmed.to_lowercase();
        let before = out.len();
        let parts = identifier_parts(trimmed);
        push_token(out, whole.clone(), off, off + trimmed.len(), *position);
        if parts.len() > 1 {
            for (a, b) in parts {
                let p = trimmed[a..b].to_lowercase();
                if p != whole {
                    push_token(out, p, off + a, off + b, *position);
                }
            }
        }
        if out.len() > before {
            *position += 1;
        }
    };
    for (i, c) in text.char_indices() {
        let word = c.is_alphanumeric() || c == '_';
        match (word, run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(s)) => {
                flush(s, i, out, &mut position);
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = run_start {
        flush(s, text.len(), out, &mut position);
    }
}

/// Tokens of `text` as plain strings (tests, query building).
pub fn code_tokens(text: &str) -> Vec<String> {
    let mut v = Vec::new();
    tokenize_into(text, &mut v);
    v.into_iter().map(|t| t.text).collect()
}

// ---------------------------------------------------------------------------------------------
// ExactTokenizer (R-UX33)
// ---------------------------------------------------------------------------------------------

/// Tokenizer for the `exact` field (R-UX33). It emits, lower-cased, only terms a user types to
/// find *one* thing:
///
/// * **identifier-like words** (a maximal run of alphanumerics and `_`) that contain a digit, an
///   inner `_`, or a case change: `savetokedit19q76666`, `max_file_bytes`, `HttpServer`,
///   `887a0005`, `76666`. Plain words (`server`, `Server`, `HTTP`) are left to `body`, so
///   natural-language queries are ranked exactly as before;
/// * **digit runs** of at least [`EXACT_MIN_DIGIT_RUN`] digits inside such a word (`ticket4711` →
///   `4711`), so a number is found inside a longer token;
/// * **compound tokens**: words joined by `.`, `-`, `:` or `::` with no space (`gpt-4o-mini`,
///   `ERR-1234`, `v1.2.3`, `fusion.ts`, `std::io`), when they contain a digit or a `.`/`:` join
///   between parts of two or more characters (so `well-known` and `e.g.` are not exact terms).
///
/// The field has no length norms: an exact identifier scores the same in a 40-line file as in a
/// 2-line note (the M2 tester saw `journal.md` rank 10th behind short notes that only shared
/// sub-tokens).
#[derive(Clone, Copy, Default)]
pub struct ExactTokenizer;

impl Tokenizer for ExactTokenizer {
    type TokenStream<'a> = CodeTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> CodeTokenStream {
        let mut tokens = Vec::new();
        for (i, t) in exact_spans(text).into_iter().enumerate() {
            push_token(&mut tokens, t.text, t.from, t.to, i);
        }
        CodeTokenStream {
            tokens,
            cursor: 0,
            empty: Token::default(),
        }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A word a user would type to find one thing (see [`ExactTokenizer`]).
pub fn identifier_like(word: &str) -> bool {
    let t = word.trim_matches('_');
    if t.is_empty() {
        return false;
    }
    if t.chars().any(char::is_numeric) || t.contains('_') {
        return true;
    }
    let cs: Vec<char> = t.chars().collect();
    cs.windows(2)
        .any(|w| w[0].is_lowercase() && w[1].is_uppercase())
        || cs
            .windows(3)
            .any(|w| w[0].is_uppercase() && w[1].is_uppercase() && w[2].is_lowercase())
}

/// Digit runs of at least [`EXACT_MIN_DIGIT_RUN`] digits inside `word`: `(byte_from, byte_to)`.
fn digit_runs(word: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut count = 0usize;
    for (i, c) in word.char_indices() {
        if c.is_numeric() {
            if start.is_none() {
                start = Some(i);
                count = 0;
            }
            count += 1;
        } else if let Some(s) = start.take()
            && count >= EXACT_MIN_DIGIT_RUN
        {
            out.push((s, i));
        }
    }
    if let Some(s) = start
        && count >= EXACT_MIN_DIGIT_RUN
    {
        out.push((s, word.len()));
    }
    out
}

/// One compound run: its words (byte ranges) and the joins between them.
struct Compound {
    words: Vec<(usize, usize)>,
    /// `true` for every `.` / `:` / `::` join, `false` for `-`.
    code_join: Vec<bool>,
}

/// Split `text` into compound runs of words joined by `.`, `-`, `:` or `::` (no spaces).
fn compounds(text: &str) -> Vec<Compound> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out: Vec<Compound> = Vec::new();
    let mut i = 0usize;
    let mut cur: Option<Compound> = None;
    while i < chars.len() {
        let (b, c) = chars[i];
        if is_word_char(c) {
            let start = b;
            let mut j = i;
            while j < chars.len() && is_word_char(chars[j].1) {
                j += 1;
            }
            let end = chars.get(j).map_or(text.len(), |x| x.0);
            cur.get_or_insert_with(|| Compound {
                words: Vec::new(),
                code_join: Vec::new(),
            })
            .words
            .push((start, end));
            i = j;
            // A join continues the compound only when a word follows it directly.
            let sep_len = match (chars.get(i).map(|x| x.1), chars.get(i + 1).map(|x| x.1)) {
                (Some(':'), Some(':')) => 2,
                (Some('.' | '-' | ':'), _) => 1,
                _ => 0,
            };
            if sep_len > 0 && chars.get(i + sep_len).is_some_and(|x| is_word_char(x.1)) {
                if let Some(c) = cur.as_mut() {
                    c.code_join.push(chars[i].1 != '-');
                }
                i += sep_len;
                continue;
            }
            if let Some(c) = cur.take() {
                out.push(c);
            }
        } else {
            if let Some(c) = cur.take() {
                out.push(c);
            }
            i += 1;
        }
    }
    if let Some(c) = cur.take() {
        out.push(c);
    }
    out
}

/// One exact term with its byte offsets.
struct ExactSpan {
    text: String,
    from: usize,
    to: usize,
    /// A whole word or compound as written (not a digit run cut out of a longer word).
    whole: bool,
}

/// Exact terms of `text` with their byte offsets (see [`ExactTokenizer`]), lower-cased.
fn exact_spans(text: &str) -> Vec<ExactSpan> {
    let mut out: Vec<ExactSpan> = Vec::new();
    let mut push = |text: String, from: usize, to: usize, whole: bool| {
        out.push(ExactSpan {
            text,
            from,
            to,
            whole,
        });
    };
    for c in compounds(text) {
        // A compound's own words are exact terms too (they rank), but when the compound itself
        // is a term only it vouches for a match: `ERR-1234` must not count any `1234`.
        let mut compound_emitted = false;
        if c.words.len() > 1 {
            let (from, to) = (c.words[0].0, c.words[c.words.len() - 1].1);
            let whole = &text[from..to];
            let has_digit = whole.chars().any(char::is_numeric);
            let code_parts = c.code_join.iter().any(|j| *j)
                && c.words
                    .iter()
                    .all(|(a, b)| text[*a..*b].chars().count() >= 2);
            if has_digit || code_parts {
                let lw = whole.to_lowercase();
                compound_emitted = lw.len() <= MAX_TOKEN_BYTES;
                push(lw, from, to, true);
            }
        }
        for (a, b) in &c.words {
            let raw = &text[*a..*b];
            let lead = raw.len() - raw.trim_start_matches('_').len();
            let word = raw.trim_matches('_');
            // A lone single digit ("step 2", the "2" of "v1.2") is too common to help.
            let short_number =
                word.chars().all(char::is_numeric) && word.chars().count() < EXACT_MIN_DIGIT_RUN;
            if !identifier_like(word) || short_number {
                continue;
            }
            let off = a + lead;
            push(
                word.to_lowercase(),
                off,
                off + word.len(),
                !compound_emitted,
            );
            for (x, y) in digit_runs(word) {
                if y - x < word.len() {
                    push(word[x..y].to_string(), off + x, off + y, false);
                }
            }
        }
    }
    out.retain(|t| !t.text.is_empty() && t.text.len() <= MAX_TOKEN_BYTES);
    out
}

/// The exact terms of `text` (indexing and query side; tests).
pub fn exact_tokens(text: &str) -> Vec<String> {
    exact_spans(text).into_iter().map(|t| t.text).collect()
}

/// How a query reads, for the exact lane and the "no good match" evidence (R-UX33).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryShape {
    /// Exact terms of the query (deduplicated, at most 64): the exact lane.
    pub exact_terms: Vec<String>,
    /// The whole identifier-like words, numbers and compounds as typed (no digit runs cut out of
    /// a longer word): a hit on one of these is a match on its own. `887a0099` ranks files with
    /// `887` in the lane, but only `887a0099` itself vouches for a result.
    pub evidence_terms: Vec<String>,
    /// Every word of the query is digits (`76666`, `2026-10-19`): only an exact match counts.
    pub digits_only: bool,
    /// The query is one plain word (`zebrapathtoken`): any keyword hit counts as a match.
    pub single_plain_word: bool,
    /// The whole query is one identifier, number or compound as typed (`Wan99VideoResult`,
    /// `0x80070057`, `v9.81.4`): like a digits-only query, only that exact term counts (its
    /// sub-tokens and a similar-looking embedding are not a match). M3 tester, R-UX33 negatives.
    pub single_exact_term: bool,
    /// Whitespace-separated words as typed (a path or identifier is one word) (M5-E).
    pub typed_words: usize,
    /// An exact term holds a digit run of at least [`LANE_STRONG_DIGIT_RUN`] digits (a ticket
    /// number, a date, an error code): rare enough to rank by inside a sentence (M5-E).
    pub strong_exact: bool,
}

/// M5-E (R-UX37): queries of at most this many typed words always rank by the exact lane.
pub const LANE_MAX_PLAIN_WORDS: usize = 2;
/// M5-E (R-UX37): inside a longer query, only an exact term with a digit run this long ranks by
/// the exact lane (`135000`, `887A0005`, `claude-3-7-sonnet-20250219`). Brand names and plurals
/// (`WordPress`, `URLs`), short numbers (`404`) and versions (`v2`, `HTML5`) in a sentence don't.
pub const LANE_STRONG_DIGIT_RUN: usize = 4;

impl QueryShape {
    pub fn of(query: &str) -> Self {
        let spans = exact_spans(query);
        let mut exact_terms: Vec<String> = spans.iter().map(|t| t.text.clone()).collect();
        exact_terms.sort();
        exact_terms.dedup();
        exact_terms.truncate(MAX_QUERY_TOKENS);
        let mut evidence_terms: Vec<String> = spans
            .into_iter()
            .filter(|t| t.whole)
            .map(|t| t.text)
            .collect();
        evidence_terms.sort();
        evidence_terms.dedup();
        evidence_terms.truncate(MAX_QUERY_TOKENS);
        let words: Vec<&str> = query
            .split(|c: char| !is_word_char(c))
            .map(|w| w.trim_matches('_'))
            .filter(|w| !w.is_empty())
            .collect();
        let digits_only =
            !words.is_empty() && words.iter().all(|w| w.chars().all(char::is_numeric));
        let single_plain_word = words.len() == 1 && !identifier_like(words[0]);
        let typed = query.trim();
        let single_exact_term = evidence_terms.len() == 1
            && !typed.contains(char::is_whitespace)
            && evidence_terms[0] == typed.to_lowercase();
        let typed_words = typed
            .split_whitespace()
            .filter(|w| w.chars().any(is_word_char))
            .count();
        let strong_exact = exact_terms.iter().any(|t| {
            t.split(|c: char| !c.is_numeric())
                .any(|run| run.chars().count() >= LANE_STRONG_DIGIT_RUN)
        });
        Self {
            exact_terms,
            evidence_terms,
            digits_only,
            single_plain_word,
            single_exact_term,
            typed_words,
            strong_exact,
        }
    }

    /// M5-E (R-UX37): whether the exact lane joins the ranking (RRF) for this query. It always
    /// still supplies evidence (no-good-match cut-off, `minScore` pass, whole-term-first order),
    /// so exact positives and negatives behave as before; only sentences whose exact terms are
    /// common words (`WordPress`, `URLs`, `404`, `v2`) stop pulling unrelated chunks up.
    pub fn exact_lane_ranks(&self) -> bool {
        !self.exact_terms.is_empty()
            && (self.typed_words <= LANE_MAX_PLAIN_WORDS || self.strong_exact)
    }

    /// M5-E: the query is one identifier, number, path or file name as typed (`getEmbeddingModel`,
    /// `lib/a/b.ts`, `76666`). A cross-encoder reads such queries as text and ranks them badly
    /// (M3: path queries 0.959 → 0.656), so the reranker leaves them alone.
    pub fn is_lookup(&self) -> bool {
        self.typed_words == 1 && (!self.exact_terms.is_empty() || !self.single_plain_word)
    }
}

// ---------------------------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------------------------

/// One document (= one chunk).
#[derive(Debug, Clone, PartialEq)]
pub struct LexDoc {
    pub chunk_id: ChunkId,
    pub file_id: FileId,
    pub folder_id: FolderId,
    pub collection_id: CollectionId,
    pub body: String,
    /// Root-relative path (ranking signal only; never returned, see report §3 on renames).
    pub path: String,
    pub lang: String,
}

/// One atomic lexical write (one tantivy commit).
#[derive(Debug, Clone, Default)]
pub struct LexBatch {
    pub docs: Vec<LexDoc>,
    pub replace_files: Vec<FileId>,
    pub purge_folders: Vec<FolderId>,
    pub seq: i64,
}

/// One BM25 hit.
#[derive(Debug, Clone, PartialEq)]
pub struct LexHit {
    pub chunk_id: ChunkId,
    pub score: f32,
}

#[derive(Debug, Clone, Copy)]
struct Fields {
    chunk_id: Field,
    file_id: Field,
    folder_id: Field,
    collection_id: Field,
    body: Field,
    path: Field,
    lang: Field,
    /// Exact terms of body and path (R-UX33), no length norms.
    exact: Field,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let text = |rec: IndexRecordOption| {
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(CODE_TOKENIZER)
                .set_index_option(rec),
        )
    };
    let f = Fields {
        chunk_id: b.add_text_field("chunk_id", STRING | STORED),
        file_id: b.add_text_field("file_id", STRING),
        folder_id: b.add_text_field("folder_id", STRING),
        collection_id: b.add_text_field("collection_id", STRING),
        body: b.add_text_field("body", text(IndexRecordOption::WithFreqsAndPositions)),
        path: b.add_text_field("path", text(IndexRecordOption::WithFreqs)),
        lang: b.add_text_field("lang", STRING),
        exact: b.add_text_field(
            "exact",
            TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer(EXACT_TOKENIZER)
                    .set_index_option(IndexRecordOption::WithFreqs)
                    .set_fieldnorms(false),
            ),
        ),
    };
    (b.build(), f)
}

fn tv(what: &str, e: impl std::fmt::Display) -> Error {
    guard::store_error(what, e)
}

/// Delete everything inside `dir` (the index files), retrying briefly: antivirus can hold a file
/// for a moment after it was closed (U20).
fn clear_dir(dir: &Path) -> Result<()> {
    let mut last = None;
    for attempt in 0..5u64 {
        let entries = std::fs::read_dir(dir).map_err(|e| tv("read tantivy dir", e))?;
        let mut failed = None;
        for e in entries.flatten() {
            let p = e.path();
            let r = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
            if let Err(err) = r {
                failed = Some(err);
            }
        }
        match failed {
            None => return Ok(()),
            Some(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(100 * (attempt + 1)));
    }
    Err(tv(
        "clear old tantivy index",
        last.map_or_else(|| "unknown".to_string(), |e| e.to_string()),
    ))
}

/// A collection's tantivy index. One writer, held for the index's lifetime (single IndexWriter
/// task, D3).
pub struct LexicalIndex {
    dir: PathBuf,
    collection_id: CollectionId,
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    fields: Fields,
    cfg: LexConfig,
    /// The index on disk had another schema (older build) and was recreated empty at open.
    recreated: bool,
}

impl std::fmt::Debug for LexicalIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LexicalIndex")
            .field("dir", &self.dir)
            .field("collection_id", &self.collection_id)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct Payload {
    seq: i64,
}

impl LexicalIndex {
    /// Open or create the index for `collection_id` in `<root>/<collection_id>`.
    pub fn open(root: &Path, collection_id: CollectionId, cfg: LexConfig) -> Result<Self> {
        let root = validate_store_root(root)?;
        let dir = root.join(collection_id.to_string());
        guard::blocking("tantivy open", || {
            std::fs::create_dir_all(&dir).map_err(|e| tv("create tantivy dir", e))?;
            let (schema, fields) = schema();
            let mut recreated = false;
            let index = if dir.join("meta.json").exists() {
                let idx = Index::open_in_dir(&dir).map_err(|e| tv("tantivy open", e))?;
                if idx.schema() == schema {
                    idx
                } else {
                    // Written by an older build (M2: no `exact` field). The keyword index is
                    // derived data: recreate it empty; the engine re-queues the collection's
                    // files (outbox) and the lane fills in the background.
                    tracing::warn!(target: "sr_core::lexical", dir = %dir.display(), "keyword index has an older layout; rebuilding it");
                    drop(idx);
                    clear_dir(&dir)?;
                    recreated = true;
                    Index::create_in_dir(&dir, schema).map_err(|e| tv("tantivy create", e))?
                }
            } else {
                Index::create_in_dir(&dir, schema).map_err(|e| tv("tantivy create", e))?
            };
            index
                .tokenizers()
                .register(CODE_TOKENIZER, TextAnalyzer::from(CodeTokenizer));
            index
                .tokenizers()
                .register(EXACT_TOKENIZER, TextAnalyzer::from(ExactTokenizer));
            let writer = index
                .writer_with_num_threads(cfg.writer_threads.max(1), cfg.writer_heap_bytes)
                .map_err(|e| tv("tantivy writer", e))?;
            let reader = index
                .reader_builder()
                .reload_policy(ReloadPolicy::Manual)
                .try_into()
                .map_err(|e| tv("tantivy reader", e))?;
            Ok(Self {
                dir: dir.clone(),
                collection_id,
                index,
                reader,
                writer: Mutex::new(writer),
                fields,
                cfg: cfg.clone(),
                recreated,
            })
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The index on disk had an older schema and was recreated empty by [`Self::open`]: the
    /// caller must re-queue the collection's current files (sr-engine does, at open).
    pub fn recreated(&self) -> bool {
        self.recreated
    }

    pub fn collection_id(&self) -> CollectionId {
        self.collection_id
    }

    /// The watermark committed with the data (commit payload); 0 for a fresh index.
    pub fn watermark(&self) -> Result<i64> {
        guard::blocking("tantivy watermark", || {
            let metas = self
                .index
                .load_metas()
                .map_err(|e| tv("tantivy metas", e))?;
            Ok(metas
                .payload
                .as_deref()
                .and_then(|p| serde_json::from_str::<Payload>(p).ok())
                .map_or(0, |p| p.seq))
        })
    }

    /// Apply one batch atomically: delete the docs of `replace_files` and `purge_folders`, add
    /// `docs`, commit with payload `{"seq": batch.seq}`, reload the reader.
    pub fn apply(&self, batch: &LexBatch) -> Result<()> {
        guard::blocking("tantivy apply", || {
            let mut w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
            for f in &batch.replace_files {
                w.delete_term(Term::from_field_text(self.fields.file_id, &f.to_string()));
            }
            for f in &batch.purge_folders {
                w.delete_term(Term::from_field_text(self.fields.folder_id, &f.to_string()));
            }
            for d in &batch.docs {
                w.add_document(doc!(
                    self.fields.chunk_id => d.chunk_id.to_hex(),
                    self.fields.file_id => d.file_id.to_string(),
                    self.fields.folder_id => d.folder_id.to_string(),
                    self.fields.collection_id => d.collection_id.to_string(),
                    self.fields.body => d.body.as_str(),
                    self.fields.path => d.path.as_str(),
                    self.fields.lang => d.lang.as_str(),
                    self.fields.exact => d.body.as_str(),
                    self.fields.exact => d.path.as_str(),
                ))
                .map_err(|e| tv("tantivy add", e))?;
            }
            let payload = serde_json::to_string(&Payload { seq: batch.seq })
                .map_err(|e| tv("tantivy payload", e))?;
            let mut pc = w
                .prepare_commit()
                .map_err(|e| tv("tantivy prepare_commit", e))?;
            pc.set_payload(&payload);
            pc.commit().map_err(|e| tv("tantivy commit", e))?;
            drop(w);
            self.reader.reload().map_err(|e| tv("tantivy reload", e))
        })
    }

    /// Number of live documents.
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// BM25 search over `body` and boosted `path`, optionally narrowed to `folders`.
    pub fn search(
        &self,
        query: &str,
        folders: Option<&[FolderId]>,
        k: usize,
    ) -> Result<Vec<LexHit>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        guard::blocking("tantivy search", || {
            let mut terms = code_tokens(query);
            terms.sort();
            terms.dedup();
            terms.truncate(MAX_QUERY_TOKENS);
            if terms.is_empty() {
                return Ok(Vec::new());
            }
            let mut should: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(terms.len() * 2);
            for t in &terms {
                should.push((
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.body, t),
                        IndexRecordOption::WithFreqs,
                    )),
                ));
                should.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.path, t),
                            IndexRecordOption::WithFreqs,
                        )),
                        self.cfg.path_boost,
                    )),
                ));
            }
            self.run(Box::new(BooleanQuery::new(should)), folders, k)
        })
    }

    /// The exact lane (R-UX33): chunks whose body or path holds one of `terms` (from
    /// [`QueryShape::exact_terms`]) as an exact term, ranked by BM25 without length norms.
    /// Empty for no terms.
    pub fn search_exact(
        &self,
        terms: &[String],
        folders: Option<&[FolderId]>,
        k: usize,
    ) -> Result<Vec<LexHit>> {
        if k == 0 || terms.is_empty() {
            return Ok(Vec::new());
        }
        guard::blocking("tantivy exact search", || {
            let should: Vec<(Occur, Box<dyn Query>)> = terms
                .iter()
                .take(MAX_QUERY_TOKENS)
                .map(|t| {
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.exact, t),
                            IndexRecordOption::WithFreqs,
                        )) as Box<dyn Query>,
                    )
                })
                .collect();
            self.run(Box::new(BooleanQuery::new(should)), folders, k)
        })
    }

    /// Run a text query, optionally narrowed to `folders`, and map the hits to chunk ids.
    fn run(
        &self,
        text_q: Box<dyn Query>,
        folders: Option<&[FolderId]>,
        k: usize,
    ) -> Result<Vec<LexHit>> {
        let q: Box<dyn Query> = match folders {
            Some([]) => return Ok(Vec::new()),
            Some(fs) => {
                let set = TermSetQuery::new(
                    fs.iter()
                        .map(|f| Term::from_field_text(self.fields.folder_id, &f.to_string())),
                );
                Box::new(BooleanQuery::new(vec![
                    (Occur::Must, text_q),
                    (Occur::Must, Box::new(set) as Box<dyn Query>),
                ]))
            }
            None => text_q,
        };
        let searcher = self.reader.searcher();
        let top = searcher
            .search(&q, &TopDocs::with_limit(k).order_by_score())
            .map_err(|e| tv("tantivy search", e))?;
        let mut out = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let d: TantivyDocument = searcher.doc(addr).map_err(|e| tv("tantivy doc", e))?;
            if let Some(id) = d
                .get_first(self.fields.chunk_id)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<ChunkId>().ok())
            {
                out.push(LexHit {
                    chunk_id: id,
                    score,
                });
            }
        }
        Ok(out)
    }

    /// Physical purge (M9): merge every segment (drops deleted docs and their terms) and garbage
    /// collect the old segment files. AV can hold files briefly (U20): retried with backoff.
    pub fn merge_and_gc(&self) -> Result<()> {
        guard::blocking("tantivy merge_and_gc", || {
            let mut w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
            for attempt in 0..3u64 {
                let ids = self
                    .index
                    .searchable_segment_ids()
                    .map_err(|e| tv("tantivy segments", e))?;
                if ids.is_empty() {
                    break;
                }
                // A single segment with deletes is rewritten too.
                match w.merge(&ids).wait() {
                    Ok(_) => break,
                    // Background merges can race an explicit merge ("segments could not be
                    // found", M0-B test 7): re-read segment ids and retry.
                    Err(e) if attempt < 2 => {
                        tracing::debug!(target: "sr_core::lexical", error = %e, "merge retry");
                        std::thread::sleep(Duration::from_millis(50 * (attempt + 1)));
                    }
                    Err(e) => return Err(tv("tantivy merge", e)),
                }
            }
            // Keep the payload: re-commit the current watermark (merge does not change it).
            let seq = self
                .index
                .load_metas()
                .ok()
                .and_then(|m| m.payload)
                .unwrap_or_else(|| "{\"seq\":0}".to_string());
            let mut pc = w
                .prepare_commit()
                .map_err(|e| tv("tantivy prepare_commit", e))?;
            pc.set_payload(&seq);
            pc.commit().map_err(|e| tv("tantivy commit", e))?;
            self.reader.reload().map_err(|e| tv("tantivy reload", e))?;
            for attempt in 0..5u64 {
                let r = w
                    .garbage_collect_files()
                    .wait()
                    .map_err(|e| tv("tantivy gc", e))?;
                if r.failed_to_delete_files.is_empty() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100 * (attempt + 1)));
            }
            Err(Error::new(ErrorCode::FileLocked)
                .with_detail("tantivy GC could not delete old segment files"))
        })
    }

    /// Chunk ids of every live document (verification, tests).
    pub fn all_chunk_ids(&self) -> Result<Vec<ChunkId>> {
        guard::blocking("tantivy scan", || {
            let searcher = self.reader.searcher();
            let mut out = Vec::new();
            for (ord, seg) in searcher.segment_readers().iter().enumerate() {
                let store = seg
                    .get_store_reader(64)
                    .map_err(|e| tv("tantivy store", e))?;
                for doc_id in seg.doc_ids_alive() {
                    let d: TantivyDocument = store.get(doc_id).map_err(|e| tv("tantivy doc", e))?;
                    if let Some(id) = d
                        .get_first(self.fields.chunk_id)
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<ChunkId>().ok())
                    {
                        out.push(id);
                    }
                }
                let _ = ord;
            }
            Ok(out)
        })
    }

    /// Whether `token` is still in any segment's `body` term dictionary (even with only deleted
    /// docs). After a purge's merge + GC it must be gone: physical removal, not just a tombstone.
    pub fn term_in_dictionary(&self, token: &str) -> Result<bool> {
        guard::blocking("tantivy term lookup", || {
            let term = Term::from_field_text(self.fields.body, token);
            let searcher = self.reader.searcher();
            for seg in searcher.segment_readers() {
                let inv = seg
                    .inverted_index(self.fields.body)
                    .map_err(|e| tv("tantivy inverted index", e))?;
                if inv
                    .get_term_info(&term)
                    .map_err(|e| tv("tantivy term info", e))?
                    .is_some()
                {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }

    /// Number of segment files on disk (tests).
    pub fn segment_count(&self) -> Result<usize> {
        self.index
            .searchable_segment_ids()
            .map(|v| v.len())
            .map_err(|e| tv("tantivy segments", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_tokenizer_splits_identifiers_and_keeps_wholes() {
        assert_eq!(
            code_tokens("HTTPServer handle_request fooBar TKN01234"),
            vec![
                "httpserver",
                "http",
                "server",
                "handle_request",
                "handle",
                "request",
                "foobar",
                "foo",
                "bar",
                "tkn01234",
                "tkn",
                "01234"
            ]
        );
        assert_eq!(
            code_tokens("a.b(c) -- é_x"),
            vec!["a", "b", "c", "é_x", "é", "x"]
        );
        assert!(
            code_tokens(&"x".repeat(100)).is_empty(),
            "long tokens dropped"
        );
        assert!(code_tokens("___ ... ").is_empty());
    }

    #[test]
    fn exact_tokens_keep_identifiers_numbers_and_compounds() {
        // Identifier-like words whole, plus their digit runs.
        assert_eq!(
            exact_tokens("savetokedit19q76666 in journal"),
            vec!["savetokedit19q76666", "19", "76666"]
        );
        assert_eq!(exact_tokens("76666"), vec!["76666"]);
        assert_eq!(
            exact_tokens("MAX_FILE_BYTES HttpServer HTTPServer"),
            vec!["max_file_bytes", "httpserver", "httpserver"]
        );
        // Plain words are not exact terms (natural language ranks as before).
        assert!(exact_tokens("how are dense and keyword results merged").is_empty());
        assert!(exact_tokens("Server HTTP well-known e.g. i.e.").is_empty());
        // Compounds with a digit, or code joins between real parts.
        assert_eq!(
            exact_tokens("use gpt-4o-mini; see ERR-1234"),
            vec!["gpt-4o-mini", "4o", "err-1234", "1234"]
        );
        assert_eq!(exact_tokens("v1.2.3"), vec!["v1.2.3", "v1"]);
        assert_eq!(exact_tokens("import './fusion.ts'"), vec!["fusion.ts"]);
        assert_eq!(exact_tokens("std::io::Read"), vec!["std::io::read"]);
        assert_eq!(
            exact_tokens("error 0x887A0005"),
            vec!["0x887a0005", "887", "0005"]
        );
        assert_eq!(exact_tokens("ticket4711"), vec!["ticket4711", "4711"]);
        assert!(
            exact_tokens("gemma4").len() == 1,
            "single digits are not split out"
        );
        assert!(exact_tokens(&format!("a{}", "1".repeat(80))).is_empty());
    }

    /// M5-E (R-UX37): the exact lane ranks for short queries and for sentences with a long digit
    /// run; sentences whose only exact terms are brand names, acronym plurals, short numbers or
    /// versions keep the plain hybrid ranking. Cases from both eval corpora.
    #[test]
    fn exact_lane_gate_follows_query_shape() {
        for q in [
            "getEmbeddingModel",
            "lib/ai/vector-search/v2/fusion.ts",
            "76666",
            "o200k_base",
            "claude-3-7-sonnet-20250219",
            "which timeout is set to 135000",
            "oauth callback port 36742",
            "887A0005 device removed error",
            "token threshold 140000 for compaction",
            "invoice number 48213907 from the supplier",
            "HTML5 validator",
        ] {
            assert!(QueryShape::of(q).exact_lane_ranks(), "{q}");
        }
        for q in [
            "How to fix soft 404 errors in GWT?",
            "Do WordPress websites get indexed quicker by search engines than a regular website?",
            "Are shorter URIs better from an SEO perspective?",
            "default ignore patterns like node_modules and .git",
            "L2 normalize an embedding vector to unit length",
            "Changing nameservers for a domain on GoDaddy",
            // No exact terms at all: nothing to rank by.
            "chocolate layer cake recipe",
            "quokka",
        ] {
            assert!(!QueryShape::of(q).exact_lane_ranks(), "{q}");
        }
        assert_eq!(QueryShape::of("  a  b   c ").typed_words, 3);
        assert!(QueryShape::of("ticket 4711").strong_exact);
        assert!(!QueryShape::of("error 404 page").strong_exact);
    }

    /// M5-E (R-UX36): one identifier, path or number as typed is a lookup (the reranker leaves it
    /// alone); a plain word or a sentence is not.
    #[test]
    fn lookups_are_single_identifiers_paths_and_numbers() {
        for q in [
            "getEmbeddingModel",
            "lib/a/b.ts",
            "76666",
            "v9.81.4",
            "LEX_DIM",
        ] {
            assert!(QueryShape::of(q).is_lookup(), "{q}");
        }
        for q in [
            "quokka",
            "how does fusion work",
            "where is getEmbeddingModel",
        ] {
            assert!(!QueryShape::of(q).is_lookup(), "{q}");
        }
    }

    #[test]
    fn query_shape_reads_numbers_identifiers_and_single_words() {
        let s = QueryShape::of("76666");
        assert!(s.digits_only && !s.single_plain_word);
        assert_eq!(s.exact_terms, vec!["76666"]);
        assert!(QueryShape::of("2026-10-19").digits_only);
        let s = QueryShape::of("zebrapathtoken");
        assert!(s.single_plain_word && !s.digits_only && s.exact_terms.is_empty());
        assert!(!s.single_exact_term);
        for q in [
            "Wan99VideoResult",
            "0x80070057",
            "v9.81.4",
            "MAX_RETRY_7731_MS",
            " decodeBase91Url ",
        ] {
            assert!(QueryShape::of(q).single_exact_term, "{q}");
        }
        for q in [
            "where is Wan99VideoResult",
            "lib/vectordb/file-watcher.ts",
            "chocolate",
        ] {
            assert!(!QueryShape::of(q).single_exact_term, "{q}");
        }
        let s = QueryShape::of("where is savetokedit19q76666 used");
        assert!(!s.single_plain_word && !s.digits_only);
        assert_eq!(s.exact_terms, vec!["19", "76666", "savetokedit19q76666"]);
        assert_eq!(s.evidence_terms, vec!["savetokedit19q76666"]);
        let s = QueryShape::of("887a0099");
        assert_eq!(s.exact_terms, vec!["0099", "887", "887a0099"]);
        assert_eq!(s.evidence_terms, vec!["887a0099"]);
        // Only the compound as typed vouches, not its parts.
        let s = QueryShape::of("ERR-1234");
        assert_eq!(s.evidence_terms, vec!["err-1234"]);
        assert!(s.exact_terms.contains(&"1234".to_string()));
        assert_eq!(
            QueryShape::of("2026-10-19").evidence_terms,
            vec!["2026-10-19"]
        );
        // A one-digit query has nothing exact to hold against.
        let s = QueryShape::of("7");
        assert!(s.digits_only && s.exact_terms.is_empty());
        // A sentence: nothing exact; only the word count (M5-E lane gate) is set.
        assert_eq!(
            QueryShape::of("chocolate cake recipe"),
            QueryShape {
                typed_words: 3,
                ..QueryShape::default()
            }
        );
    }

    #[test]
    fn exact_lane_ignores_document_length_and_finds_digit_runs() {
        let dir = tempfile::tempdir().unwrap();
        let coll = CollectionId::from_bytes([1; 16]);
        let idx = LexicalIndex::open(dir.path(), coll, LexConfig::default()).unwrap();
        assert!(!idx.recreated());
        let folder = FolderId::from_bytes([2; 16]);
        let mk = |n: u8, body: String, path: &str| LexDoc {
            chunk_id: ChunkId::from_bytes([n; 32]),
            file_id: FileId::from_bytes([n; 16]),
            folder_id: folder,
            collection_id: coll,
            body,
            path: path.into(),
            lang: "text".into(),
        };
        let long: String = (0..400)
            .map(|i| format!("journal line {i} about the weather\n"))
            .collect::<String>()
            + "savetokedit19q76666\n";
        let mut docs = vec![mk(1, long, "journal.md")];
        for n in 2..20u8 {
            docs.push(mk(n, format!("savetoknew19q{n}"), &format!("note{n}.md")));
        }
        idx.apply(&LexBatch {
            docs,
            seq: 1,
            ..Default::default()
        })
        .unwrap();
        let journal = ChunkId::from_bytes([1; 32]);
        let terms = QueryShape::of("savetokedit19q76666").exact_terms;
        let hits = idx.search_exact(&terms, None, 10).unwrap();
        assert_eq!(hits.first().map(|h| h.chunk_id), Some(journal));
        // A digits-only query finds the number inside the longer token.
        let hits = idx
            .search_exact(&QueryShape::of("76666").exact_terms, None, 10)
            .unwrap();
        assert_eq!(
            hits.iter().map(|h| h.chunk_id).collect::<Vec<_>>(),
            vec![journal]
        );
        // A number that is nowhere: nothing.
        assert!(
            idx.search_exact(&QueryShape::of("76667").exact_terms, None, 10)
                .unwrap()
                .is_empty()
        );
        // File names are exact terms too (path field).
        let hits = idx
            .search_exact(&QueryShape::of("note7.md").exact_terms, None, 10)
            .unwrap();
        assert_eq!(
            hits.first().map(|h| h.chunk_id),
            Some(ChunkId::from_bytes([7; 32]))
        );
    }

    #[test]
    fn an_older_schema_is_recreated_empty() {
        let dir = tempfile::tempdir().unwrap();
        let coll = CollectionId::from_bytes([3; 16]);
        let sub = dir.path().join(coll.to_string());
        std::fs::create_dir_all(&sub).unwrap();
        // An M2-style index: same fields minus `exact`.
        let mut b = Schema::builder();
        b.add_text_field("chunk_id", STRING | STORED);
        b.add_text_field("body", tantivy::schema::TEXT);
        let old = Index::create_in_dir(&sub, b.build()).unwrap();
        let mut w: IndexWriter = old.writer_with_num_threads(1, 20_000_000).unwrap();
        w.commit().unwrap();
        drop(w);
        drop(old);
        let idx = LexicalIndex::open(dir.path(), coll, LexConfig::default()).unwrap();
        assert!(idx.recreated());
        assert_eq!(idx.num_docs(), 0);
        assert_eq!(idx.watermark().unwrap(), 0);
        drop(idx);
        let again = LexicalIndex::open(dir.path(), coll, LexConfig::default()).unwrap();
        assert!(!again.recreated(), "the new layout is kept");
    }

    #[test]
    fn same_position_for_parts() {
        let mut t = Vec::new();
        tokenize_into("getUser id", &mut t);
        let pos: Vec<(String, usize)> = t.into_iter().map(|t| (t.text, t.position)).collect();
        assert_eq!(
            pos,
            vec![
                ("getuser".into(), 0),
                ("get".into(), 0),
                ("user".into(), 0),
                ("id".into(), 1)
            ]
        );
    }
}
