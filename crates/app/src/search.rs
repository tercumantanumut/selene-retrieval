//! Search, Ask and Prompt Enhancer (pages 26–28) over the app-RPC `search`, `answer`,
//! `enhance_prompt` and `requests.cancel` (contract §7). Plain logic over [`Rpc`].
//!
//! * Results are grouped **by file** in rank order of each file's best hit (page 26).
//! * Snippets, answers and prompts are untrusted text: they cross as plain strings and the UI
//!   renders them as text, never markup (02 §2.6).
//! * Ask and Enhance carry a client-chosen `request_id`, so [Stop] can cancel them
//!   (`requests.cancel`; nothing more is billed) and `request.progress` lines can be matched.
#![allow(clippy::result_large_err)] // `UiError` is the Tauri IPC error (see commands.rs).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use specta::Type;
use sr_common::error::{Error, ErrorCode};

use crate::daemon::{Rpc, method};
use crate::dto::UiError;

/// Results per page (page 26 "Show more results" asks for more).
pub const PAGE: u32 = 50;
/// The engine's cap on `max_results`.
pub const MAX_RESULTS: u32 = 150;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct Scores {
    pub rrf: f32,
    pub dense: Option<f32>,
    pub bm25: Option<f32>,
    pub rerank: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct HitView {
    pub line_start: u32,
    pub line_end: u32,
    /// Plain text (≤ 4000 chars); never markup.
    pub snippet: String,
    pub scores: Scores,
}

/// One result card: a file with its hits, best first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ResultFile {
    /// Absolute path ([Copy path], [Show in folder]).
    pub path: String,
    pub rel_path: String,
    pub file_name: String,
    pub folder_id: String,
    pub folder_name: String,
    pub hits: Vec<HitView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SearchView {
    pub files: Vec<ResultFile>,
    /// Chunk-level hits returned (for "Show more results": more may exist when = asked).
    pub hits: u32,
    pub asked: u32,
    /// Distinct folders among the results ("{n} results in {m} folders").
    pub folders: u32,
    pub no_folders: bool,
    /// "Nothing closely matched …" (R-UX18): `files` is empty on purpose.
    pub no_good_match: bool,
}

fn file_name(rel: &str) -> String {
    rel.rsplit(['/', '\\']).next().unwrap_or(rel).to_owned()
}

#[derive(Deserialize)]
struct RawScores {
    rrf: f32,
    dense: Option<f32>,
    bm25: Option<f32>,
    rerank: Option<f32>,
}

#[derive(Deserialize)]
struct RawHit {
    path: String,
    rel_path: String,
    folder_id: String,
    folder_name: String,
    line_start: u32,
    line_end: u32,
    snippet: String,
    scores: RawScores,
}

/// `SearchOutput` → page 26 cards (grouped by file, rank order of the best hit).
pub fn search_view(v: &Value, asked: u32) -> SearchView {
    let hits: Vec<RawHit> = v
        .get("hits")
        .and_then(|h| serde_json::from_value(h.clone()).ok())
        .unwrap_or_default();
    let n = u32::try_from(hits.len()).unwrap_or(u32::MAX);
    let mut files: Vec<ResultFile> = Vec::new();
    for h in hits {
        let hit = HitView {
            line_start: h.line_start,
            line_end: h.line_end,
            snippet: h.snippet,
            scores: Scores {
                rrf: h.scores.rrf,
                dense: h.scores.dense,
                bm25: h.scores.bm25,
                rerank: h.scores.rerank,
            },
        };
        match files.iter_mut().find(|f| f.path == h.path) {
            Some(f) => f.hits.push(hit),
            None => files.push(ResultFile {
                file_name: file_name(&h.rel_path),
                path: h.path,
                rel_path: h.rel_path,
                folder_id: h.folder_id,
                folder_name: h.folder_name,
                hits: vec![hit],
            }),
        }
    }
    let mut folders: Vec<&str> = files.iter().map(|f| f.folder_id.as_str()).collect();
    folders.sort_unstable();
    folders.dedup();
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    SearchView {
        folders: u32::try_from(folders.len()).unwrap_or(u32::MAX),
        files,
        hits: n,
        asked,
        no_folders: flag("no_folders"),
        no_good_match: flag("no_good_match"),
    }
}

pub async fn search<R: Rpc>(
    rpc: &R,
    query: &str,
    folder_ids: Option<Vec<String>>,
    max_results: Option<u32>,
) -> Result<SearchView, UiError> {
    let q = query.trim();
    if q.is_empty() {
        return Err(UiError::from(
            Error::new(ErrorCode::InvalidInput).with_detail("empty query"),
        ));
    }
    let asked = max_results.unwrap_or(PAGE).clamp(1, MAX_RESULTS);
    let v = rpc
        .call(
            method::SEARCH,
            json!({ "query": q, "max_results": asked, "folder_ids": folder_ids }),
        )
        .await
        .map_err(UiError::from)?;
    Ok(search_view(&v, asked))
}

/// A client-chosen request id: 1–64 of `[A-Za-z0-9_-]` (contract §7).
pub fn valid_request_id(id: &str) -> bool {
    sr_common::api::is_valid_request_id(id)
}

fn bad_request_id() -> UiError {
    UiError::from(Error::new(ErrorCode::InvalidInput).with_detail("bad request id"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SourceView {
    /// The number used in the text's `[n]` markers.
    pub n: u32,
    pub path: String,
    pub rel_path: String,
    pub file_name: String,
    pub folder_name: String,
    pub line_start: u32,
    pub line_end: u32,
}

/// Page 27. `text` is plain text with `[n]` markers that point at `sources[].n`; markers that
/// don't match a cited source are left as text by the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct AnswerView {
    pub text: String,
    /// The sources the answer cites, in the order of first citation (the "Sources" list).
    pub sources: Vec<SourceView>,
    pub provider_name: Option<String>,
    pub model: Option<String>,
    /// Extra search rounds used (deeper answers).
    pub rounds: u32,
    /// Nothing matched: no provider call was made (page 27 no-match line).
    pub no_good_match: bool,
    pub no_folders: bool,
}

/// `AnswerOutput` → page 27. Only sources that a citation points at are listed (every citation
/// was validated by the engine against the shown sources, Amendment 7.7).
pub fn answer_view(v: &Value) -> AnswerView {
    let sources: Vec<Value> = v
        .get("sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let cited: Vec<u64> = v
        .get("citations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("source").and_then(Value::as_u64))
        .collect();
    let mut out: Vec<SourceView> = Vec::new();
    for n in cited {
        if out.iter().any(|s| u64::from(s.n) == n) {
            continue;
        }
        let Some(s) = sources
            .iter()
            .find(|s| s.get("n").and_then(Value::as_u64) == Some(n))
        else {
            continue;
        };
        let txt = |k: &str| {
            s.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let line = |k: &str| {
            s.get(k)
                .and_then(Value::as_u64)
                .and_then(|x| u32::try_from(x).ok())
                .unwrap_or(0)
        };
        let rel = txt("rel_path");
        out.push(SourceView {
            n: u32::try_from(n).unwrap_or(0),
            path: txt("path"),
            file_name: file_name(&rel),
            rel_path: rel,
            folder_name: txt("folder_name"),
            line_start: line("line_start"),
            line_end: line("line_end"),
        });
    }
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|x| !x.is_empty())
    };
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    AnswerView {
        text: v
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        sources: out,
        provider_name: s("provider").map(|p| crate::providers::display_name(&p)),
        model: s("model"),
        rounds: v
            .get("rounds")
            .and_then(Value::as_u64)
            .and_then(|r| u32::try_from(r).ok())
            .unwrap_or(0),
        no_good_match: flag("no_good_match"),
        no_folders: flag("no_folders"),
    }
}

/// [Ask]: a written answer from the folders in scope (the consent is checked by the service;
/// a missing one comes back as code 26 with [Allow]).
pub async fn ask<R: Rpc>(
    rpc: &R,
    question: &str,
    folder_ids: Option<Vec<String>>,
    request_id: &str,
) -> Result<AnswerView, UiError> {
    if !valid_request_id(request_id) {
        return Err(bad_request_id());
    }
    let q = question.trim();
    if q.is_empty() {
        return Err(UiError::from(
            Error::new(ErrorCode::InvalidInput).with_detail("empty question"),
        ));
    }
    let v = rpc
        .call(
            method::ANSWER,
            json!({ "question": q, "folder_ids": folder_ids, "request_id": request_id }),
        )
        .await
        .map_err(UiError::from)?;
    Ok(answer_view(&v))
}

/// [Stop]: cancels a running Ask / Enhance. `false` = it had already finished.
pub async fn cancel_request<R: Rpc>(rpc: &R, request_id: &str) -> Result<bool, UiError> {
    if !valid_request_id(request_id) {
        return Err(bad_request_id());
    }
    let v = rpc
        .call(method::REQUESTS_CANCEL, json!({ "request_id": request_id }))
        .await
        .map_err(UiError::from)?;
    Ok(v.get("cancelled").and_then(Value::as_bool).unwrap_or(false))
}

/// Page 28 result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct EnhanceView {
    /// The improved prompt (plain text).
    pub prompt: String,
    /// The prompt changed.
    pub enhanced: bool,
    /// The heuristic fallback wrote it: "Improved without AI this time ({provider} didn't respond)."
    pub fallback: bool,
    pub provider_name: Option<String>,
    /// File names of the context used ("Used context from {n} files").
    pub files: Vec<String>,
}

pub fn enhance_view(v: &Value) -> EnhanceView {
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|x| !x.is_empty())
    };
    EnhanceView {
        prompt: s("prompt").unwrap_or_default(),
        enhanced: v.get("enhanced").and_then(Value::as_bool).unwrap_or(false),
        fallback: v.get("fallback").and_then(Value::as_bool).unwrap_or(false),
        provider_name: s("provider").map(|p| crate::providers::display_name(&p)),
        files: v
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(file_name)
            .collect(),
    }
}

/// [Improve prompt]: only the prompt and the scope are sent (OQ-43; memories and history are for
/// MCP callers).
pub async fn enhance<R: Rpc>(
    rpc: &R,
    prompt: &str,
    folder_ids: Option<Vec<String>>,
    request_id: &str,
) -> Result<EnhanceView, UiError> {
    if !valid_request_id(request_id) {
        return Err(bad_request_id());
    }
    if prompt.trim().is_empty() {
        return Err(UiError::from(
            Error::new(ErrorCode::InvalidInput).with_detail("empty prompt"),
        ));
    }
    let v = rpc
        .call(
            method::ENHANCE_PROMPT,
            json!({ "prompt": prompt, "folder_ids": folder_ids, "request_id": request_id }),
        )
        .await
        .map_err(UiError::from)?;
    Ok(enhance_view(&v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake::FakeRpc;

    fn hit(path: &str, folder: &str, l: u32, rrf: f32) -> Value {
        json!({"path": path, "rel_path": path.trim_start_matches("C:\\D\\").replace('\\', "/"),
            "folder_id": folder, "folder_name": "Docs", "line_start": l, "line_end": l + 2,
            "snippet": "<b>not markup</b>", "score": rrf,
            "scores": {"rrf": rrf, "dense": 0.5, "bm25": null, "rerank": null}})
    }

    #[test]
    fn hits_group_by_file_in_rank_order() {
        let v = json!({"hits": [
            hit("C:\\D\\a\\one.md", "F1", 1, 0.9),
            hit("C:\\D\\two.txt", "F2", 5, 0.8),
            hit("C:\\D\\a\\one.md", "F1", 40, 0.7)
        ], "total_ms": 3.0, "dropped_stale": 0, "no_folders": false, "no_good_match": false});
        let s = search_view(&v, 50);
        assert_eq!(s.files.len(), 2);
        assert_eq!(s.files[0].file_name, "one.md");
        assert_eq!(s.files[0].hits.len(), 2);
        assert_eq!(s.files[0].hits[1].line_start, 40);
        assert_eq!(s.files[1].file_name, "two.txt");
        assert_eq!((s.hits, s.folders), (3, 2));
        assert_eq!(
            s.files[0].hits[0].snippet, "<b>not markup</b>",
            "kept as text"
        );
    }

    #[test]
    fn no_good_match_is_honest() {
        let s = search_view(
            &json!({"hits": [], "no_good_match": true, "no_folders": false}),
            50,
        );
        assert!(s.no_good_match && s.files.is_empty());
    }

    #[tokio::test]
    async fn search_forwards_scope_and_caps_results() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(json!({"hits": []})));
        search(&rpc, "  lease  ", Some(vec!["F1".into()]), Some(9_999))
            .await
            .unwrap();
        assert_eq!(
            rpc.calls()[0],
            (
                "search".to_owned(),
                json!({"query": "lease", "max_results": 150, "folder_ids": ["F1"]})
            )
        );
        assert!(search(&rpc, "   ", None, None).await.is_err());
        assert_eq!(
            rpc.calls().len(),
            1,
            "empty queries never reach the service"
        );
    }

    #[test]
    fn answer_lists_only_cited_sources_once() {
        let v = json!({
            "text": "The lease ends in May [2]. See also [1] and [2].",
            "citations": [
                {"source": 2, "path": "C:\\D\\lease.pdf", "line_start": 3, "line_end": 9},
                {"source": 1, "path": "C:\\D\\notes.md", "line_start": 1, "line_end": 2},
                {"source": 2, "path": "C:\\D\\lease.pdf", "line_start": 3, "line_end": 9}
            ],
            "sources": [
                {"n": 1, "path": "C:\\D\\notes.md", "rel_path": "notes.md", "folder_name": "Docs",
                 "line_start": 1, "line_end": 2, "snippet": "x", "from_read": false},
                {"n": 2, "path": "C:\\D\\lease.pdf", "rel_path": "c/lease.pdf", "folder_name": "Docs",
                 "line_start": 3, "line_end": 9, "snippet": "y", "from_read": true},
                {"n": 3, "path": "C:\\D\\unused.md", "rel_path": "unused.md", "folder_name": "Docs",
                 "line_start": 1, "line_end": 1, "snippet": "z", "from_read": false}
            ],
            "no_good_match": false, "no_folders": false, "provider": "made-up-id", "model": "m-1",
            "rounds": 0, "tool_steps": 1, "files_read": [], "structured": true,
            "input_tokens": 1, "output_tokens": 1
        });
        let a = answer_view(&v);
        assert_eq!(a.sources.iter().map(|s| s.n).collect::<Vec<_>>(), [2, 1]);
        assert_eq!(a.sources[0].file_name, "lease.pdf");
        assert_eq!(
            a.provider_name.as_deref(),
            Some("made-up-id"),
            "unknown id stays the id"
        );
        assert_eq!(a.model.as_deref(), Some("m-1"));
    }

    #[tokio::test]
    async fn ask_and_cancel_carry_the_request_id() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(
            json!({"text": "", "citations": [], "sources": [], "no_good_match": true}),
        ));
        let a = ask(&rpc, "When does the lease end?", None, "req-1")
            .await
            .unwrap();
        assert!(a.no_good_match);
        assert_eq!(rpc.calls()[0].1["request_id"], "req-1");
        rpc.reply(Ok(json!({"cancelled": true})));
        assert!(cancel_request(&rpc, "req-1").await.unwrap());
        assert_eq!(
            rpc.calls()[1],
            ("requests.cancel".to_owned(), json!({"request_id": "req-1"}))
        );
        // Bad ids and empty questions never reach the service.
        assert!(ask(&rpc, "q", None, "../../x").await.is_err());
        assert!(cancel_request(&rpc, "").await.is_err());
        assert!(ask(&rpc, "  ", None, "ok").await.is_err());
        assert_eq!(rpc.calls().len(), 2);
        // A stopped request: the plain code-18 text.
        rpc.reply(Err(Error::new(ErrorCode::Cancelled)));
        let e = ask(&rpc, "q", None, "req-2").await.unwrap_err();
        assert_eq!(e.body, "The operation was stopped.");
    }

    #[tokio::test]
    async fn enhance_sends_only_prompt_and_scope() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(
            json!({"enhanced": true, "prompt": "Better", "original": "b",
            "used_llm": false, "fallback": true, "input_type": "question",
            "files": ["C:\\D\\a.md", "sub/b.txt"], "chunks_retrieved": 2, "provider": "p"}),
        ));
        let e = enhance(&rpc, "rough prompt", Some(vec!["F".into()]), "r1")
            .await
            .unwrap();
        assert!(e.fallback);
        assert_eq!(e.files, ["a.md", "b.txt"]);
        let p = &rpc.calls()[0].1;
        assert_eq!(
            p,
            &json!({"prompt": "rough prompt", "folder_ids": ["F"], "request_id": "r1"})
        );
    }
}
