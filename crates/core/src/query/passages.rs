//! The passages an answer starts from (docs/08 "broad query ranking").
//!
//! Since `maxSources` became a hard total (docs/06 issue 1) the answer's first search keeps only a
//! few passages (9 of the default 12). Taking the top of the fused list as is let one long file
//! (a vendored source, a provider, a test suite) fill most of them, so the file that implements
//! the topic, whose many passages each ranked a little lower, was cut (the owner's broad question
//! "How does vector indexing work?": `index/lance.rs` first appeared at rank 19, after 6 passages
//! of one vendored file and 2 test files).
//!
//! ```text
//! search wider than the budget ([`passage_width`]: 3 × budget, at least 30, at most 150)
//!   → group the candidates by file (folder + path)
//!   → file bonus = 0.1 × second-best + 0.05 × third-best fused score   (a file with several
//!                                                                    matching passages is about
//!                                                                    the topic)
//!   → tests / fixtures / benches / examples count half (score and bonus), unless the question
//!     asks about them ([`is_support_file`], [`asks_about_support_files`])
//!   → pick greedily by  passage score + its file's bonus − 0.003 × passages already taken from
//!     that file,  until the budget is full
//! ```
//!
//! The repeat-file penalty is soft: a file's second passage still comes in when it clearly beats
//! the other files' best ones (0.003 fused score is about 15 places around rank 15 with RRF
//! k = 60), so a file with several truly relevant passages keeps them, while one long file can't
//! fill the budget. The weights were chosen on the eval sets (docs/08 §8): larger aggregation
//! weights (½, ¼) or strict one-per-file round robin cost 2–4 nDCG points on CQADupStack or at
//! chunk level on selene-src.
//!
//! A lookup (one identifier, path, file name or number as typed, [`QueryShape::is_lookup`])
//! keeps the fused order: there the best passage is the answer, and the exact-term-first order of
//! the search must stand. The search itself (`context_search` tool, eval) is unchanged; this only decides
//! which `budget` of its passages the model receives.
//!
//! **Answer query** ([`answer_query`], docs/08 §9). The answer also searches with a slightly
//! different text than the question as asked (the owner's "How does Selene Retrieval's vector
//! indexing engine work?" on the project's own folder got the README, CLAUDE.md, the roadmap and
//! `crates/engine/*` files, and not one passage of the vector store):
//!
//! ```text
//! 1. the question names a folder of its scope (a name of at least 2 words: "Selene Retrieval",
//!    "selene-retrieval", "SeleneRetrieval")  and  2 or more topic words remain
//!      → the name is taken out, and the search covers the named folder(s) only
//!        (inside its own folder a folder's name says nothing about which passage answers; it
//!        only pulls the README and every page that repeats the name)
//! 2. base forms of the -ing / -ed words are added for the keyword lane, which matches whole
//!    words: "indexing" → "index", "queried" → "query", "embedded" → "embed"
//! ```
//!
//! Lookups and questions of fewer than 3 words get no base forms; one-word folder names
//! (`Documents`, `Taxes`) are never taken out (they are everyday words and often the topic).

use std::collections::HashMap;

use sr_common::ids::FolderId;

use super::{QueryShape, SearchHit};

/// Candidates searched per passage of the budget (docs/08: retrieval width ≠ passage budget).
pub const PASSAGE_WIDTH_FACTOR: usize = 3;
/// Fewest candidates searched for an answer, so a small budget still sees enough files.
pub const PASSAGE_MIN_WIDTH: usize = 30;
/// Most candidates searched (the search's own `maxResults` cap).
pub const PASSAGE_MAX_WIDTH: usize = 150;
/// Weights of a file's second- and third-best passage in its bonus (docs/08 §8).
pub const FILE_BONUS_WEIGHTS: [f32; 2] = [0.1, 0.05];
/// Fused score a passage loses for each passage already taken from its file (docs/08 §8).
pub const REPEAT_FILE_PENALTY: f32 = 0.003;
/// Score factor of tests, fixtures, benches and examples for a question that isn't about them.
pub const SUPPORT_FILE_WEIGHT: f32 = 0.5;

/// What the selection needs from a ranked passage.
pub trait Passage {
    fn passage_folder(&self) -> FolderId;
    /// Folder-relative path, `/`-separated.
    fn passage_path(&self) -> &str;
    /// Fused (RRF) score: higher is better.
    fn passage_score(&self) -> f32;
}

impl Passage for SearchHit {
    fn passage_folder(&self) -> FolderId {
        self.folder_id
    }
    fn passage_path(&self) -> &str {
        &self.path
    }
    fn passage_score(&self) -> f32 {
        self.scores.rrf
    }
}

/// How many candidates to search for a passage budget of `budget`.
pub fn passage_width(budget: usize) -> usize {
    budget
        .saturating_mul(PASSAGE_WIDTH_FACTOR)
        .clamp(PASSAGE_MIN_WIDTH, PASSAGE_MAX_WIDTH)
        .max(budget)
}

/// Directory names that hold tests, fixtures, benchmarks or examples (compared lower-case).
const SUPPORT_DIRS: &[&str] = &[
    "test",
    "tests",
    "__tests__",
    "__mocks__",
    "testdata",
    "test_data",
    "test-data",
    "fixture",
    "fixtures",
    "spec",
    "specs",
    "e2e",
    "bench",
    "benches",
    "benchmark",
    "benchmarks",
    "example",
    "examples",
];

/// A test, fixture, benchmark or example file, by its folder-relative path: a directory named
/// like [`SUPPORT_DIRS`], or a file named `*_test.*`, `*.test.*`, `*.spec.*`, `*_spec.*`,
/// `test_*.*` or `FooTest.*` / `FooTests.*`. README, CHANGELOG and docs are not support files.
pub fn is_support_file(rel_path: &str) -> bool {
    let mut parts: Vec<&str> = rel_path
        .split(['/', '\\'])
        .filter(|p| !p.is_empty())
        .collect();
    let Some(name) = parts.pop() else {
        return false;
    };
    if parts
        .iter()
        .any(|d| SUPPORT_DIRS.contains(&d.to_ascii_lowercase().as_str()))
    {
        return true;
    }
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    let lower = stem.to_ascii_lowercase();
    if lower.starts_with("test_")
        || [
            "_test", "_tests", ".test", ".tests", "-test", ".spec", "_spec", "-spec",
        ]
        .iter()
        .any(|s| lower.ends_with(s))
    {
        return true;
    }
    // Java / C# style: `ParserTest.java`, `ParserTests.cs` (not `Test.java`, `Latest.rs`).
    ["Test", "Tests"].iter().any(|s| {
        stem.strip_suffix(s)
            .and_then(|head| head.chars().last())
            .is_some_and(|c| c.is_lowercase() || c.is_ascii_digit())
    })
}

/// The question is about tests, fixtures, benchmarks, examples or mocks: support files keep
/// their full weight.
pub fn asks_about_support_files(query: &str) -> bool {
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .any(|w| {
            ["test", "spec", "fixture", "bench", "example", "mock"]
                .iter()
                .any(|p| w.starts_with(p))
        })
}

/// Fewest words a folder name needs before a question naming it counts as naming the folder
/// (`Selene Retrieval`, `selene-retrieval`, `TaxReturns2024`). One-word names (`Documents`,
/// `Notes`, `Taxes`) are everyday words that are often the topic itself, so they stay in the
/// question.
pub const MIN_NAME_WORDS: usize = 2;
/// Fewest topic words that must remain once the folder name is taken out; otherwise the question
/// is kept as asked ("How does Selene Retrieval work?" has nothing else to search for).
pub const MIN_TOPIC_WORDS: usize = 2;

/// Question words, auxiliaries and fillers: not topic words ([`MIN_TOPIC_WORDS`]), and never given
/// base forms.
const NON_TOPIC_WORDS: &[&str] = &[
    "a",
    "about",
    "all",
    "also",
    "an",
    "and",
    "any",
    "anything",
    "are",
    "as",
    "at",
    "be",
    "been",
    "being",
    "but",
    "by",
    "can",
    "could",
    "describe",
    "did",
    "do",
    "does",
    "done",
    "each",
    "everything",
    "explain",
    "for",
    "from",
    "he",
    "here",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "just",
    "may",
    "me",
    "might",
    "must",
    "my",
    "no",
    "not",
    "nothing",
    "of",
    "on",
    "or",
    "our",
    "please",
    "shall",
    "she",
    "should",
    "show",
    "so",
    "some",
    "something",
    "tell",
    "than",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "to",
    "us",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "who",
    "whom",
    "whose",
    "why",
    "will",
    "with",
    "work",
    "working",
    "works",
    "would",
    "you",
    "your",
];

/// The search an answer runs for its question (docs/08 §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerQuery {
    /// The search text: the question without the name of the folder it is about, plus the base
    /// forms of its `-ing` / `-ed` words (`indexing` → `index`, `queried` → `query`).
    pub text: String,
    /// The folders the question names (a subset of the scope it was given), when it names some
    /// and their name was taken out of the text.
    pub folders: Option<Vec<FolderId>>,
}

/// One word of a question: its byte span and lower-case base (`Retrieval's` → `retrieval`).
struct QWord {
    start: usize,
    end: usize,
    base: String,
}

fn is_apostrophe(c: char) -> bool {
    c == '\'' || c == '\u{2019}'
}

/// Words of `text`: runs of letters and digits, with an apostrophe inside a word kept
/// (`Retrieval's`, `don't`).
fn question_words(text: &str) -> Vec<QWord> {
    let mut out = Vec::new();
    let mut it = text.char_indices().peekable();
    while let Some((start, c)) = it.next() {
        if !c.is_alphanumeric() {
            continue;
        }
        let mut end = start + c.len_utf8();
        while let Some(&(j, d)) = it.peek() {
            let inner_apostrophe = is_apostrophe(d)
                && text[j + d.len_utf8()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphabetic);
            if d.is_alphanumeric() || inner_apostrophe {
                end = j + d.len_utf8();
                it.next();
            } else {
                break;
            }
        }
        let lower = text[start..end].to_lowercase();
        let base = ["'s", "\u{2019}s"]
            .iter()
            .find_map(|s| lower.strip_suffix(s))
            .unwrap_or(&lower)
            .to_string();
        out.push(QWord { start, end, base });
    }
    out
}

/// The words of a folder name, lower-case: separators and camelCase both split
/// (`selene-retrieval`, `Selene Retrieval` and `SeleneRetrieval` → `selene`, `retrieval`).
pub fn name_words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|p| !p.is_empty())
    {
        let cs: Vec<char> = part.chars().collect();
        let mut from = 0;
        for i in 1..cs.len() {
            let next_lower = cs.get(i + 1).is_some_and(|c| c.is_lowercase());
            if (cs[i - 1].is_lowercase() && cs[i].is_uppercase())
                || (cs[i - 1].is_uppercase() && cs[i].is_uppercase() && next_lower)
            {
                out.push(cs[from..i].iter().collect::<String>().to_lowercase());
                from = i;
            }
        }
        out.push(cs[from..].iter().collect::<String>().to_lowercase());
    }
    out
}

fn is_topic_word(base: &str) -> bool {
    base.chars().count() >= 2 && !NON_TOPIC_WORDS.contains(&base)
}

fn is_vowel(b: u8) -> bool {
    matches!(b, b'a' | b'e' | b'i' | b'o' | b'u')
}

/// Base forms of an English `-ing` / `-ed` word, for the keyword lane, which matches whole words
/// (`indexing` → `index`, `embedded` → `embed`, `storing` → `stor`, `store`, `queried` →
/// `query`). Plain lower-case ASCII words of five letters or more only; plurals are left alone
/// (measured: they cost Q&A text more than they gain, docs/08 §9).
pub fn base_forms(word: &str) -> Vec<String> {
    if word.len() < 5 || !word.bytes().all(|b| b.is_ascii_lowercase()) {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(s) = word.strip_suffix("ied") {
        out.push(format!("{s}y"));
    } else if let Some(s) = word
        .strip_suffix("ing")
        .or_else(|| word.strip_suffix("ed"))
        .filter(|s| s.len() >= 4)
    {
        let b = s.as_bytes();
        let n = b.len();
        // `running` → `run`, `embedded` → `embed` (not `ll`, `ss`, `zz`: `installing`).
        if b[n - 1] == b[n - 2] && !is_vowel(b[n - 1]) && !b"lsz".contains(&b[n - 1]) {
            out.push(s[..n - 1].to_string());
        } else {
            out.push(s.to_string());
            // A silent `e` dropped before the ending: `storing` → `store` (consonant, vowel,
            // consonant; not after w, x or y: `indexing`, `showing`, `playing`).
            if !is_vowel(b[n - 1])
                && !b"wxy".contains(&b[n - 1])
                && is_vowel(b[n - 2])
                && !is_vowel(b[n - 3])
            {
                out.push(format!("{s}e"));
            }
        }
    }
    out.retain(|f| f.len() >= 3);
    out
}

/// The search an answer runs for `question` over folders named `scope` (`(folder, name)`: each
/// folder's display name and its directory name). See the module docs, "Answer query".
pub fn answer_query(question: &str, scope: &[(FolderId, &str)]) -> AnswerQuery {
    let asked = AnswerQuery {
        text: question.trim().to_string(),
        folders: None,
    };
    if QueryShape::of(question).is_lookup() {
        return asked;
    }
    let words = question_words(question);
    // 1. The folder the question is about, by name: its words say nothing about which passage of
    //    that folder answers, but they pull its README and every page that repeats the name.
    let mut dropped = vec![false; words.len()];
    let mut named: Vec<FolderId> = Vec::new();
    for (folder, name) in scope {
        let nw = name_words(name);
        if nw.len() < MIN_NAME_WORDS || nw.len() > words.len() {
            continue;
        }
        for i in 0..=words.len() - nw.len() {
            if nw.iter().zip(&words[i..]).all(|(n, w)| *n == w.base) {
                dropped[i..i + nw.len()].fill(true);
                if !named.contains(folder) {
                    named.push(*folder);
                }
            }
        }
    }
    let topic = words
        .iter()
        .zip(&dropped)
        .filter(|(w, d)| !**d && is_topic_word(&w.base))
        .count();
    let mut out = asked;
    if !named.is_empty() && topic >= MIN_TOPIC_WORDS {
        let mut text = String::new();
        let mut last = 0;
        for (w, _) in words.iter().zip(&dropped).filter(|(_, d)| **d) {
            text.push_str(&question[last..w.start]);
            last = w.end;
        }
        text.push_str(&question[last..]);
        out.text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        named.sort();
        out.folders = Some(named);
    }
    // 2. Base forms of its `-ing` / `-ed` words: code and notes say `index`, `embed`, `query`
    //    where the question says `indexing`, `embedded`, `queried`.
    let words = question_words(&out.text);
    if words.len() >= 3 {
        let mut add: Vec<String> = Vec::new();
        for w in words.iter().filter(|w| is_topic_word(&w.base)) {
            for f in base_forms(&w.base) {
                if !add.contains(&f) && !words.iter().any(|x| x.base == f) {
                    add.push(f);
                }
            }
        }
        if !add.is_empty() {
            out.text = format!("{} {}", out.text, add.join(" "));
        }
    }
    out
}

/// Picks at most `budget` passages of `hits` (in rank order) for an answer (see the module docs).
/// The result is in the order the model sees the sources.
pub fn select_passages<T: Passage>(hits: Vec<T>, budget: usize, query: &str) -> Vec<T> {
    let budget = budget.min(hits.len());
    if budget == 0 {
        return Vec::new();
    }
    if QueryShape::of(query).is_lookup() {
        return hits.into_iter().take(budget).collect();
    }
    let support_weight = if asks_about_support_files(query) {
        1.0
    } else {
        SUPPORT_FILE_WEIGHT
    };
    // Files in order of their first (best-ranked) passage, with their role weight.
    let mut index: HashMap<(FolderId, &str), usize> = HashMap::new();
    let mut file_of: Vec<usize> = Vec::with_capacity(hits.len());
    let mut members: Vec<Vec<f32>> = Vec::new();
    let mut weight: Vec<f32> = Vec::new();
    for h in &hits {
        let k = (h.passage_folder(), h.passage_path());
        let at = *index.entry(k).or_insert_with(|| {
            members.push(Vec::new());
            weight.push(if is_support_file(h.passage_path()) {
                support_weight
            } else {
                1.0
            });
            members.len() - 1
        });
        members[at].push(h.passage_score());
        file_of.push(at);
    }
    // Per file: the bonus from its second and third best passages.
    let bonus: Vec<f32> = members
        .iter_mut()
        .zip(&weight)
        .map(|(s, w)| {
            s.retain(|x| x.is_finite());
            s.sort_by(|a, b| b.total_cmp(a));
            s.iter()
                .skip(1)
                .zip(FILE_BONUS_WEIGHTS)
                .map(|(x, k)| x * k)
                .sum::<f32>()
                * w
        })
        .collect();
    let mut taken = vec![0u16; members.len()];
    let mut left: Vec<usize> = (0..hits.len()).collect();
    let mut picked: Vec<usize> = Vec::with_capacity(budget);
    while picked.len() < budget {
        // The best key; on a tie the better-ranked passage (`left` stays in rank order).
        let mut best: Option<(usize, f32)> = None;
        for (pos, i) in left.iter().enumerate() {
            let f = file_of[*i];
            let s = hits[*i].passage_score();
            let s = if s.is_finite() { s } else { 0.0 };
            let key = s * weight[f] + bonus[f] - REPEAT_FILE_PENALTY * f32::from(taken[f]);
            if best.is_none_or(|(_, k)| key > k) {
                best = Some((pos, key));
            }
        }
        let Some((pos, _)) = best else { break };
        let i = left.remove(pos);
        taken[file_of[i]] = taken[file_of[i]].saturating_add(1);
        picked.push(i);
    }
    let mut slots: Vec<Option<T>> = hits.into_iter().map(Some).collect();
    picked
        .into_iter()
        .filter_map(|i| slots.get_mut(i).and_then(Option::take))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct P {
        folder: FolderId,
        path: &'static str,
        score: f32,
        line: u32,
    }

    impl Passage for P {
        fn passage_folder(&self) -> FolderId {
            self.folder
        }
        fn passage_path(&self) -> &str {
            self.path
        }
        fn passage_score(&self) -> f32 {
            self.score
        }
    }

    fn folder() -> FolderId {
        FolderId::from_bytes([7; 16])
    }

    /// A fused list in rank order: `(path, line)`, scores falling like RRF ranks (1 / (60 + r)).
    fn ranked(rows: &[(&'static str, u32)]) -> Vec<P> {
        rows.iter()
            .enumerate()
            .map(|(r, (path, line))| P {
                folder: folder(),
                path,
                score: 1.0 / (61.0 + r as f32),
                line: *line,
            })
            .collect()
    }

    fn paths(v: &[P]) -> Vec<&str> {
        v.iter().map(|p| p.path).collect()
    }

    /// The owner's broad question, as measured on this repository (docs/08 §2): the fused list
    /// opens with test files, one long vendored file, eval scripts and a provider, and the file
    /// that implements the vector index first appears at rank 19 (of 20). Cutting the list to
    /// the first-search budget (9 of the default 12, 15 of 20) loses it; the selection keeps it.
    #[test]
    fn broad_question_reaches_the_core_file_within_the_budget() {
        let q = "How does vector indexing work?";
        let hits = ranked(&[
            ("crates/providers/tests/local_cpu.rs", 60),
            ("spikes/gpu-tier/stlshim/vector_algorithms.cpp", 950),
            ("eval/v2-baseline/v2_search.py", 1),
            ("crates/providers/src/local.rs", 52),
            ("crates/catalog/src/schema.rs", 67),
            ("eval/v2-baseline/verify_port.mjs", 40),
            ("crates/providers/src/embed.rs", 139),
            ("spikes/gpu-tier/stlshim/vector_algorithms.cpp", 3297),
            ("spikes/gpu-tier/stlshim/vector_algorithms.cpp", 2892),
            ("crates/providers/tests/tier_models.rs", 39),
            ("spikes/gpu-tier/stlshim/vector_algorithms.cpp", 3025),
            ("crates/core/src/index/writer.rs", 206),
            ("spikes/gpu-tier/stlshim/vector_algorithms.cpp", 11748),
            ("spikes/gpu-tier/stlshim/vector_algorithms.cpp", 3052),
            ("README.md", 1),
            ("crates/core/src/index/lexical.rs", 1140),
            ("crates/catalog/src/repo/profiles.rs", 326),
            ("crates/common/src/events.rs", 342),
            ("crates/core/src/index/lance.rs", 699),
            ("crates/catalog/src/repo/profiles.rs", 281),
            ("crates/core/src/index/lance.rs", 622),
            ("crates/catalog/src/repo/profiles.rs", 241),
            ("crates/core/src/index/lance.rs", 729),
            ("crates/core/src/index/lance.rs", 653),
            ("crates/providers/src/embed.rs", 118),
        ]);
        let lance = "crates/core/src/index/lance.rs";
        let stl = "spikes/gpu-tier/stlshim/vector_algorithms.cpp";
        for budget in [9usize, 15] {
            assert!(!paths(&hits[..budget]).contains(&lance));
            let plain_stl = paths(&hits[..budget]).iter().filter(|p| **p == stl).count();
            assert_eq!(plain_stl, if budget == 9 { 3 } else { 6 });
            let after = select_passages(hits.clone(), budget, q);
            assert_eq!(after.len(), budget);
            let got = paths(&after);
            assert!(got.contains(&lance), "budget {budget}: {got:?}");
            // One long file no longer fills the budget, and tests give way to the code.
            assert!(got.iter().filter(|p| **p == stl).count() <= 2, "{got:?}");
            assert!(
                !got.contains(&"crates/providers/tests/local_cpu.rs"),
                "{got:?}"
            );
        }
        // The core file comes in with its best passage.
        let after = select_passages(hits, 9, q);
        let first = after.iter().find(|p| p.path == lance).map(|p| p.line);
        assert_eq!(first, Some(699));
    }

    /// A fused list as measured (path, first line, fused score), in rank order.
    fn measured(rows: &[(&'static str, u32, f32)]) -> Vec<P> {
        rows.iter()
            .map(|(path, line, score)| P {
                folder: folder(),
                path,
                score: *score,
                line: *line,
            })
            .collect()
    }

    /// The owner's question of 2026-10-20, measured on a copy of this repository (bundled
    /// bge-small, docs/08 §9): the fused top 25 for the question as asked, and for the answer
    /// query ("How does vector indexing engine work? index", the folder's name taken out).
    /// As asked, the 15 passages of `maxSources` 20 are the README, CLAUDE.md, the roadmap, the
    /// engine crate and eval scripts (the four files the owner's answer cited among them) and no
    /// vector-store passage; with the answer query the vector store, the keyword index and the
    /// store registry come in, from the default budget on.
    #[test]
    fn owner_question_reaches_the_vector_store_and_a_query_path() {
        let q = "How does Selene Retrieval's vector indexing engine work?";
        let asked = measured(&[
            ("crates/core/src/query/passages.rs", 279, 0.03252),
            ("crates/engine/tests/answers.rs", 458, 0.03154),
            ("README.md", 1, 0.03151),
            ("docs/08-broad-query-ranking.md", 1, 0.02764),
            ("crates/core/src/query/passages.rs", 1, 0.02742),
            ("crates/engine/src/rerank.rs", 13, 0.02731),
            ("crates/engine/tests/answers.rs", 499, 0.02725),
            ("eval/v2-baseline/v2_search.py", 1, 0.02686),
            ("crates/engine/src/reembed.rs", 49, 0.02449),
            ("crates/providers/tests/tier_models.rs", 39, 0.02228),
            ("CLAUDE.md", 1, 0.02184),
            ("crates/engine/src/reembed.rs", 186, 0.02172),
            ("crates/providers/src/local.rs", 52, 0.02123),
            ("crates/engine/tests/default_folder.rs", 1, 0.01969),
            ("eval/v2-baseline/v2_prep.mjs", 1, 0.01861),
            ("crates/mcp-server/src/tools.rs", 265, 0.01856),
            ("crates/mcp-server/src/tools.rs", 894, 0.01781),
            ("docs/04-roadmap.md", 13, 0.01622),
            ("ui/index.html", 1, 0.01562),
            ("crates/engine/tests/answers.rs", 482, 0.01515),
            ("crates/core/src/query/mod.rs", 1, 0.01493),
            ("crates/core/src/query/passages.rs", 367, 0.01493),
            ("crates/core/src/index/lance.rs", 699, 0.01471),
            ("crates/core/src/query/passages.rs", 346, 0.01471),
            ("crates/core/src/query/passages.rs", 408, 0.01449),
        ]);
        let adjusted = measured(&[
            ("crates/core/src/query/passages.rs", 279, 0.03252),
            ("crates/engine/tests/answers.rs", 458, 0.03252),
            ("docs/08-broad-query-ranking.md", 1, 0.03150),
            ("crates/engine/tests/answers.rs", 499, 0.02971),
            ("crates/engine/src/reembed.rs", 186, 0.02683),
            ("crates/engine/tests/answers.rs", 482, 0.02637),
            ("crates/core/src/index/lance.rs", 283, 0.02571),
            ("crates/core/src/index/lance.rs", 699, 0.02529),
            ("crates/providers/tests/local_cpu.rs", 60, 0.02486),
            ("crates/core/src/query/passages.rs", 1, 0.02444),
            ("crates/core/src/index/lance.rs", 729, 0.02413),
            ("crates/engine/src/reembed.rs", 49, 0.02412),
            ("crates/core/src/index/lexical.rs", 1140, 0.02222),
            ("crates/engine/tests/reembed.rs", 320, 0.02211),
            ("crates/engine/tests/engine.rs", 94, 0.02142),
            ("crates/core/src/index/lance.rs", 955, 0.02128),
            ("crates/core/src/index/mod.rs", 1, 0.02051),
            ("crates/core/src/index/lance.rs", 188, 0.01976),
            ("crates/core/src/index/lance.rs", 887, 0.01941),
            ("crates/engine/src/reembed.rs", 570, 0.01914),
            ("crates/core/src/index/lance.rs", 244, 0.01885),
            ("crates/catalog/src/schema.rs", 67, 0.01742),
            ("crates/core/src/index/writer.rs", 206, 0.01709),
            ("crates/engine/src/bin/sr-bench-engine.rs", 1, 0.01642),
            ("crates/catalog/src/repo/profiles.rs", 326, 0.01587),
        ]);
        let lance = "crates/core/src/index/lance.rs";
        let query_path = [
            "crates/core/src/index/lexical.rs",
            "crates/core/src/query/mod.rs",
            "crates/core/src/query/fusion.rs",
        ];
        // maxSources 20 → 15 passages from the first search; the default 12 → 9.
        let before = select_passages(asked, 15, q);
        let got = paths(&before);
        assert!(!got.contains(&lance), "{got:?}");
        for f in ["README.md", "CLAUDE.md", "docs/04-roadmap.md"] {
            assert!(got.contains(&f), "{f}: {got:?}");
        }
        for budget in [9usize, 15] {
            let after = select_passages(adjusted.clone(), budget, q);
            let got = paths(&after);
            assert!(got.contains(&lance), "budget {budget}: {got:?}");
            assert!(
                got.iter().any(|p| query_path.contains(p)),
                "budget {budget}: {got:?}"
            );
            assert!(!got.contains(&"README.md") && !got.contains(&"CLAUDE.md"));
        }
        let after = select_passages(adjusted, 15, q);
        let lance_lines: Vec<u32> = after
            .iter()
            .filter(|p| p.path == lance)
            .map(|p| p.line)
            .collect();
        // Table and index creation (283), and the vector query path (729).
        assert!(
            lance_lines.contains(&283) && lance_lines.contains(&729),
            "{lance_lines:?}"
        );
        assert!(paths(&after).contains(&"crates/core/src/index/mod.rs"));
    }

    #[test]
    fn several_matching_passages_lift_a_file() {
        // `b.rs` has three good passages; `a.rs` and `c.rs` one each, ranked higher.
        let hits = ranked(&[
            ("a.rs", 1),
            ("c.rs", 1),
            ("b.rs", 1),
            ("b.rs", 50),
            ("b.rs", 90),
        ]);
        let out = select_passages(hits, 3, "how are passages chosen for an answer");
        assert_eq!(paths(&out), ["b.rs", "a.rs", "c.rs"]);
    }

    #[test]
    fn a_second_passage_of_a_file_needs_a_clear_lead() {
        // b.rs's only passage is close behind a.rs's second one: b.rs comes first.
        let hits = ranked(&[("a.rs", 1), ("a.rs", 2), ("a.rs", 3), ("b.rs", 1)]);
        let out = select_passages(hits.clone(), 3, "how does it work");
        assert_eq!(
            out.iter().map(|p| (p.path, p.line)).collect::<Vec<_>>(),
            [("a.rs", 1), ("b.rs", 1), ("a.rs", 2)]
        );
        // Far behind (rank 30): a.rs keeps its strong second passage.
        let mut far = ranked(&[("a.rs", 1), ("a.rs", 2)]);
        far.push(P {
            folder: folder(),
            path: "b.rs",
            score: 1.0 / 90.0,
            line: 1,
        });
        let out = select_passages(far, 2, "how does it work");
        assert_eq!(
            out.iter().map(|p| (p.path, p.line)).collect::<Vec<_>>(),
            [("a.rs", 1), ("a.rs", 2)]
        );
        // A budget larger than the list keeps every passage.
        assert_eq!(
            select_passages(hits.clone(), 10, "how does it work").len(),
            4
        );
        assert!(select_passages(hits, 0, "how does it work").is_empty());
    }

    #[test]
    fn tests_rank_below_the_code_unless_the_question_is_about_tests() {
        let hits = ranked(&[("tests/sync.rs", 1), ("src/sync.rs", 1)]);
        let out = select_passages(hits.clone(), 1, "how does folder sync work");
        assert_eq!(paths(&out), ["src/sync.rs"]);
        let out = select_passages(hits, 1, "how is folder sync tested");
        assert_eq!(paths(&out), ["tests/sync.rs"]);
    }

    #[test]
    fn lookups_keep_the_fused_order() {
        let hits = ranked(&[("a.rs", 1), ("a.rs", 2), ("b.rs", 1)]);
        for q in ["mergeInsert", "src/index/lance.rs", "76666"] {
            let out = select_passages(hits.clone(), 2, q);
            assert_eq!(
                out.iter().map(|p| (p.path, p.line)).collect::<Vec<_>>(),
                [("a.rs", 1), ("a.rs", 2)],
                "{q}"
            );
        }
    }

    #[test]
    fn same_path_in_two_folders_is_two_files() {
        let mut hits = ranked(&[("notes.md", 1), ("notes.md", 9), ("other.md", 1)]);
        hits[1].folder = FolderId::from_bytes([9; 16]);
        let out = select_passages(hits, 2, "what do the notes say");
        assert_eq!(
            out.iter().map(|p| (p.path, p.line)).collect::<Vec<_>>(),
            [("notes.md", 1), ("notes.md", 9)]
        );
    }

    #[test]
    fn support_files_by_path() {
        for p in [
            "tests/a.rs",
            "crates/x/tests/local_cpu.rs",
            "src/__tests__/a.ts",
            "pkg/testdata/in.txt",
            "spikes/x/fixtures/zero.pdf",
            "crates/core/benches/hybrid_1m.rs",
            "examples/basic.rs",
            "src/parser_test.go",
            "src/parser.test.ts",
            "src/parser.spec.ts",
            "test_parser.py",
            "src/ParserTest.java",
            "src/ParserTests.cs",
        ] {
            assert!(is_support_file(p), "{p}");
        }
        for p in [
            "README.md",
            "CHANGELOG.md",
            "docs/02-architecture.md",
            "crates/core/src/index/lance.rs",
            "crates/providers/src/testing.rs",
            "src/Test.java",
            "src/latest.rs",
            "src/contest/entry.rs",
        ] {
            assert!(!is_support_file(p), "{p}");
        }
        assert!(asks_about_support_files("how are the Tests organised?"));
        assert!(asks_about_support_files("which benchmarks exist"));
        assert!(!asks_about_support_files("how does vector indexing work?"));
    }

    /// The owner's question of 2026-10-20 on the project's own folder (docs/08 §9).
    #[test]
    fn the_folder_name_leaves_the_question_and_verbs_get_base_forms() {
        let (repo, docs) = (folder(), FolderId::from_bytes([9; 16]));
        let scope = [
            (repo, "selene-retrieval"),
            (repo, "selene-retrieval"),
            (docs, "Documents"),
        ];
        let q = answer_query(
            "How does Selene Retrieval's vector indexing engine work?",
            &scope,
        );
        assert_eq!(q.text, "How does vector indexing engine work? index");
        assert_eq!(q.folders, Some(vec![repo]));
        // Display name or directory name, any case, separators or camelCase.
        for name in ["Selene Retrieval", "SELENE_retrieval", "SeleneRetrieval"] {
            let q = answer_query(
                "How does Selene Retrieval build and query its vector index?",
                &[(repo, name)],
            );
            assert_eq!(
                q.text, "How does build and query its vector index?",
                "{name}"
            );
            assert_eq!(q.folders, Some(vec![repo]));
        }
        // Nothing left to search for without the name: kept as asked, the scope too.
        let q = answer_query("How does Selene Retrieval work?", &scope);
        assert_eq!(q.text, "How does Selene Retrieval work?");
        assert_eq!(q.folders, None);
        // A one-word name stays (it is often the topic itself).
        let q = answer_query("Which documents mention the lease renewal?", &scope);
        assert_eq!(q.text, "Which documents mention the lease renewal?");
        assert_eq!(q.folders, None);
        // Part of a name isn't the name.
        let q = answer_query("How does Selene index files?", &scope);
        assert_eq!(q.folders, None);
        assert!(q.text.starts_with("How does Selene index files?"));
        // Base forms only; lookups and short questions are left alone.
        let q = answer_query("Explain how the vector index is built and queried", &scope);
        assert_eq!(
            q.text,
            "Explain how the vector index is built and queried query"
        );
        for lookup in ["getEmbeddingModel", "src/index/lance.rs", "76666"] {
            assert_eq!(answer_query(lookup, &scope).text, lookup);
        }
        assert_eq!(
            answer_query("indexing pipeline", &scope).text,
            "indexing pipeline"
        );
    }

    #[test]
    fn base_forms_of_ing_and_ed_words() {
        let f = |w: &str| base_forms(w);
        assert_eq!(f("indexing"), ["index"]);
        assert_eq!(f("indexed"), ["index"]);
        assert_eq!(f("queried"), ["query"]);
        assert_eq!(f("embedded"), ["embed"]);
        assert_eq!(f("running"), ["run"]);
        assert_eq!(f("storing"), ["stor", "store"]);
        assert_eq!(f("installing"), ["install"]);
        assert_eq!(f("chunking"), ["chunk"]);
        assert_eq!(f("syncing"), ["sync"]);
        for none in [
            "string",
            "need",
            "files",
            "Indexing",
            "naïveing",
            "sing",
            "during",
        ] {
            assert!(f(none).is_empty(), "{none}");
        }
        assert_eq!(
            name_words("HTTPServer-v2 notes"),
            ["http", "server", "v2", "notes"]
        );
    }

    #[test]
    fn width_is_three_times_the_budget_within_bounds() {
        assert_eq!(passage_width(1), 30);
        assert_eq!(passage_width(9), 30);
        assert_eq!(passage_width(15), 45);
        assert_eq!(passage_width(30), 90);
        assert_eq!(passage_width(60), 150);
        assert_eq!(passage_width(200), 200);
    }
}
