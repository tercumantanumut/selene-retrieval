//! `answer` and `enhance_prompt` over the engine (M3-Q; docs/03 synthesizer / prompt-enhancement
//! rows; decision 7 / 0.A Q4 "answer and iterate scope = the queried collections only").
//!
//! * **Scope.** The question's scope is the folders the caller named, else every *indexed* folder
//!   (access-grant folders only when named). The first search, every `read_file` step and every
//!   deeper-answer search are limited to exactly those folders; `read_file` goes through the same
//!   `SandboxFs` checks and secret denylist as app-RPC `files.read`.
//! * **No provider call without evidence.** When the search finds nothing close (R-UX18), the
//!   answer is "Nothing closely matched" and the provider is not called (page 27).
//! * **Consent.** The remote provider's egress gate refuses until the user allowed "answers"
//!   (page 22); the refusal is `PrivacyBlocked` with the AllowRemote action, and the app is told
//!   through `consent.needed`.

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sr_catalog::model::FolderKind;
use sr_catalog::repo::folders;
use sr_common::error::{Error, ErrorCode};
use sr_common::ids::FolderId;
use sr_core::answer::{AlsoAt, AnswerConfig, AnswerTools, FileExcerpt, Source};
pub use sr_core::answer::{AnswerPhase, Citation};
pub use sr_core::enhance::{AgentContext, Enhanced, HistoryMessage, InputType};
use sr_core::enhance::{EnhanceConfig, EnhanceHit, EnhanceInput};
use tokio_util::sync::CancellationToken;

use crate::EngineHandle;
use crate::files::ReadFileRequest;
use crate::handle::{AlsoAtOut, SearchArgs, SearchHitOut};

/// Default `maxSources`: the most passages one answer shows the model, reads and deeper rounds
/// included (the first search takes 9 of them, docs/06 issue 1, picked from a wider search file
/// by file, docs/08).
pub const ANSWER_SOURCES: u32 = 12;

/// Passages one deeper-answer search adds at most (before the `maxSources` total applies).
pub const DEEPER_ROUND_SOURCES: usize = 8;

impl sr_core::query::Passage for SearchHitOut {
    fn passage_folder(&self) -> FolderId {
        self.folder_id
    }
    fn passage_path(&self) -> &str {
        &self.rel_path
    }
    fn passage_score(&self) -> f32 {
        self.score
    }
}

/// `answer` arguments (after MCP parsing).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AnswerArgs {
    pub question: String,
    /// Limit to these folders (`None` = every indexed folder).
    #[serde(default)]
    pub folder_ids: Option<Vec<FolderId>>,
    /// Passages to start from (default 12, max 30).
    #[serde(default)]
    pub max_sources: Option<u32>,
    /// MCP client id (audit only).
    #[serde(default)]
    pub client_id: Option<String>,
}

/// One source as returned to the caller (no full text: the snippet only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceOut {
    pub n: usize,
    pub path: String,
    pub rel_path: String,
    pub folder_name: String,
    /// The folder's id and root (display form), so clients can print the root once and
    /// `read_file` stays unambiguous (docs/06 change 2).
    #[serde(default)]
    pub folder_id: Option<FolderId>,
    #[serde(default)]
    pub folder_root: String,
    pub line_start: u32,
    pub line_end: u32,
    pub snippet: String,
    pub from_read: bool,
    /// Other copies of this exact text (collapsed by the search, docs/06 change 4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_at: Vec<AlsoAtOut>,
}

/// Folder id → display name.
type FolderNames = HashMap<FolderId, String>;

/// A search hit as an answer source (numbered later).
fn hit_source(h: SearchHitOut) -> Source {
    Source {
        n: 0,
        path: h.path,
        rel_path: h.rel_path,
        folder: h.folder_name,
        line_start: h.line_start,
        line_end: h.line_end,
        text: h.snippet,
        from_read: false,
        folder_id: Some(h.folder_id),
        text_hash: h.text_hash,
        also_at: h
            .also_at
            .into_iter()
            .map(|a| AlsoAt {
                folder_id: a.folder_id,
                path: a.rel_path,
                line_start: a.line_start,
                line_end: a.line_end,
            })
            .collect(),
    }
}

/// `answer` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnswerOutput {
    pub text: String,
    pub citations: Vec<Citation>,
    pub sources: Vec<SourceOut>,
    /// Nothing in the folders matched: no provider call was made.
    pub no_good_match: bool,
    /// No folder has been added yet.
    pub no_folders: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub rounds: u32,
    pub tool_steps: usize,
    pub files_read: Vec<String>,
    pub structured: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// `enhance_prompt` arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnhanceArgs {
    pub prompt: String,
    /// Caller-supplied memories (markdown bullets).
    #[serde(default)]
    pub memories: Option<String>,
    /// Recent chat messages, oldest → newest (`role` user / assistant).
    #[serde(default)]
    pub history: Vec<HistoryMessage>,
    /// Current composer attachments, formatted (reference only).
    #[serde(default)]
    pub attachments: Option<String>,
    #[serde(default)]
    pub agent: AgentContext,
    #[serde(default)]
    pub folder_ids: Option<Vec<FolderId>>,
    #[serde(default)]
    pub client_id: Option<String>,
}

/// `enhance_prompt` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnhanceOutput {
    #[serde(flatten)]
    pub result: sr_core::enhance::Enhanced,
    pub provider: Option<String>,
    pub model: Option<String>,
}

fn not_configured() -> Error {
    Error::new(ErrorCode::ProviderNotConfigured)
}

/// The answer's tools: reads and searches limited to `scope`.
struct ScopedTools {
    engine: EngineHandle,
    scope: Vec<FolderId>,
    names: FolderNames,
}

impl ScopedTools {
    fn folder_name(&self, id: FolderId) -> String {
        self.names.get(&id).cloned().unwrap_or_default()
    }
}

impl AnswerTools for ScopedTools {
    fn read<'a>(
        &'a self,
        path: &'a str,
        start_line: Option<u32>,
        end_line: Option<u32>,
    ) -> BoxFuture<'a, sr_common::Result<FileExcerpt>> {
        Box::pin(async move {
            let req = ReadFileRequest {
                path: path.to_string(),
                start_line: start_line.map(u64::from),
                end_line: end_line.map(u64::from),
                head: None,
                tail: None,
                folder_ids: Some(self.scope.clone()),
            };
            let r = self.engine.read_file(req).await?;
            if r.is_binary {
                return Err(Error::new(ErrorCode::FileUnreadable).with_name(r.rel_path));
            }
            let u = |x: u64| u32::try_from(x).unwrap_or(u32::MAX);
            Ok(FileExcerpt {
                folder: self.folder_name(r.folder_id),
                folder_id: Some(r.folder_id),
                path: r.path,
                rel_path: r.rel_path,
                line_start: u(r.start_line),
                line_end: u(r.end_line),
                total_lines: u(r.total_lines),
                text: r.content,
            })
        })
    }

    fn search<'a>(&'a self, query: &'a str) -> BoxFuture<'a, sr_common::Result<Vec<Source>>> {
        Box::pin(async move {
            let out = self
                .engine
                .answer_passages(query, DEEPER_ROUND_SOURCES, Some(self.scope.clone()), None)
                .await?;
            Ok(out.hits.into_iter().map(hit_source).collect())
        })
    }
}

impl EngineHandle {
    /// The passages an answer starts from (docs/08): the question as an answer query
    /// ([`sr_core::query::answer_query`]: the name of the folder it is about taken out, the search
    /// kept to that folder, base forms of its `-ing` / `-ed` words added), a search
    /// [`sr_core::query::passage_width`] wide, then at most `budget` passages picked file by file
    /// ([`sr_core::query::select_passages`]). `min_score` `None` = the search default. The eval
    /// bench calls this too, so the harness measures exactly what the model receives.
    pub async fn answer_passages(
        &self,
        query: &str,
        budget: usize,
        folder_ids: Option<Vec<FolderId>>,
        min_score: Option<f32>,
    ) -> Result<crate::handle::SearchOutput, Error> {
        let width = sr_core::query::passage_width(budget);
        // Each folder of the scope by its display name and its directory name.
        let names: Vec<(FolderId, String)> = self
            .inner()
            .cat
            .read_async(folders::folders)
            .await?
            .into_iter()
            .filter(|f| folder_ids.as_ref().is_none_or(|ids| ids.contains(&f.id)))
            .flat_map(|f| {
                let dir = f
                    .root_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                [(f.id, f.settings.display_name), (f.id, dir)]
            })
            .collect();
        let scope: Vec<(FolderId, &str)> = names.iter().map(|(id, n)| (*id, n.as_str())).collect();
        let aq = sr_core::query::answer_query(query, &scope);
        if aq.text != query.trim() || aq.folders.is_some() {
            tracing::debug!(target: "sr_engine::answer", named_folders = aq.folders.as_ref().map_or(0, Vec::len), "answer query adjusted");
        }
        let mut out = self
            .search(SearchArgs {
                query: aq.text,
                max_results: Some(u32::try_from(width).unwrap_or(u32::MAX)),
                min_score,
                folder_ids: aq.folders.or(folder_ids),
                mode: None,
                collapse_duplicates: None,
            })
            .await?;
        out.hits = sr_core::query::select_passages(std::mem::take(&mut out.hits), budget, query);
        Ok(out)
    }

    /// The folders a question may use: the named ones (they must exist), else every indexed
    /// folder. Access-grant folders are used only when named (0.A Q4).
    async fn answer_scope(
        &self,
        named: Option<&[FolderId]>,
    ) -> Result<(Vec<FolderId>, FolderNames), Error> {
        let all = self.inner().cat.read_async(folders::folders).await?;
        let names = all
            .iter()
            .map(|f| (f.id, f.settings.display_name.clone()))
            .collect();
        let scope = match named {
            Some(ids) => {
                for id in ids {
                    if !all.iter().any(|f| f.id == *id) {
                        return Err(
                            Error::new(ErrorCode::NotFound).with_detail("unknown folder id")
                        );
                    }
                }
                ids.to_vec()
            }
            None => all
                .iter()
                .filter(|f| f.kind == FolderKind::Indexed)
                .map(|f| f.id)
                .collect(),
        };
        Ok((scope, names))
    }

    /// `answer`: a written answer with citations from the user's folders. `progress` gets the
    /// phases ("Reading your files…", "Writing the answer…", "Searching again (round n of m)…").
    pub async fn answer(
        &self,
        args: AnswerArgs,
        progress: Arc<dyn Fn(AnswerPhase) + Send + Sync>,
        cancel: &CancellationToken,
    ) -> Result<AnswerOutput, Error> {
        let question = args.question.trim().to_string();
        if question.is_empty() {
            return Err(Error::new(ErrorCode::InvalidInput).with_detail("empty question"));
        }
        let slot = self.llm_slot().ok_or_else(not_configured)?;
        let s = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !s.answers.answers_enabled {
            return Err(not_configured().with_detail("answers are turned off in Selene Retrieval"));
        }
        let (scope, names) = self.answer_scope(args.folder_ids.as_deref()).await?;
        // Folder roots (display form) for the sources' `folder_root` (docs/06 change 2).
        let roots: HashMap<FolderId, String> = self
            .inner()
            .cat
            .read_async(folders::folders)
            .await?
            .into_iter()
            .map(|f| (f.id, f.root_path.to_string_lossy().into_owned()))
            .collect();
        let empty = |no_folders: bool| AnswerOutput {
            text: String::new(),
            citations: Vec::new(),
            sources: Vec::new(),
            no_good_match: !no_folders,
            no_folders,
            provider: Some(slot.display_name.clone()),
            model: None,
            rounds: 0,
            tool_steps: 0,
            files_read: Vec::new(),
            structured: false,
            input_tokens: 0,
            output_tokens: 0,
        };
        if scope.is_empty() {
            return Ok(empty(true));
        }
        // docs/06 issue 1: `maxSources` is the total the model sees; the first search leaves a
        // quarter of it for `read_file` excerpts and deeper rounds.
        let max = args.max_sources.unwrap_or(ANSWER_SOURCES).clamp(1, 30);
        // docs/08: the first search looks wider than its share and picks file by file.
        let first = sr_core::answer::first_search_sources(max as usize);
        let found = self
            .answer_passages(&question, first, Some(scope.clone()), None)
            .await?;
        if found.no_folders {
            return Ok(empty(true));
        }
        if found.hits.is_empty() {
            // R-UX18 / page 27: nothing to answer from, so the provider is not called.
            return Ok(empty(false));
        }
        let sources: Vec<Source> = found.hits.into_iter().map(hit_source).collect();
        let cfg = AnswerConfig {
            model: slot.model.clone(),
            deeper: s.answers.deeper_answers,
            max_rounds: s.answers.max_rounds.clamp(1, 5),
            max_sources: max as usize,
            egress: sr_providers::EgressCtx {
                collection_id: Some(self.inner().default_collection.to_string()),
                client_id: args.client_id.clone(),
            },
            ..AnswerConfig::default()
        };
        let tools = ScopedTools {
            engine: self.clone(),
            scope,
            names: names.clone(),
        };
        let p = progress.clone();
        let report = move |ph: AnswerPhase| p(ph);
        let a = sr_core::answer::answer(
            &*slot.llm, &cfg, &question, sources, &tools, &report, cancel,
        )
        .await?;
        tracing::info!(target: "sr_engine::answer", rounds = a.rounds, steps = a.tool_steps, sources = a.sources.len(), structured = a.structured, "answer written");
        Ok(AnswerOutput {
            text: a.text,
            citations: a.citations,
            sources: a
                .sources
                .into_iter()
                .map(|s| SourceOut {
                    snippet: s.text.chars().take(600).collect(),
                    n: s.n,
                    path: s.path,
                    rel_path: s.rel_path,
                    folder_name: s.folder,
                    folder_root: s
                        .folder_id
                        .and_then(|f| roots.get(&f).cloned())
                        .unwrap_or_default(),
                    folder_id: s.folder_id,
                    line_start: s.line_start,
                    line_end: s.line_end,
                    from_read: s.from_read,
                    also_at: s
                        .also_at
                        .into_iter()
                        .map(|a| AlsoAtOut {
                            folder_name: names.get(&a.folder_id).cloned().unwrap_or_default(),
                            folder_root: roots.get(&a.folder_id).cloned().unwrap_or_default(),
                            folder_id: a.folder_id,
                            rel_path: a.path,
                            line_start: a.line_start,
                            line_end: a.line_end,
                        })
                        .collect(),
                })
                .collect(),
            no_good_match: false,
            no_folders: false,
            provider: Some(slot.display_name.clone()),
            model: (!slot.model.is_empty()).then(|| slot.model.clone()),
            rounds: a.rounds,
            tool_steps: a.tool_steps,
            files_read: a.files_read,
            structured: a.structured,
            input_tokens: a.input_tokens,
            output_tokens: a.output_tokens,
        })
    }

    /// `enhance_prompt`: Selene's V2 enhancer over the user's folders (hybrid top 25, minScore
    /// 0.05, file tree, caller-supplied memories and history; 0.3 / 3000 tokens / 135 s).
    pub async fn enhance_prompt(
        &self,
        args: EnhanceArgs,
        cancel: &CancellationToken,
    ) -> Result<EnhanceOutput, Error> {
        self.enhance_prompt_with_progress(args, None, cancel).await
    }

    /// [`Self::enhance_prompt`] reporting its phases (M4 app `request.progress`): `Searching`
    /// before the folder search, `Improving` before the provider request.
    pub async fn enhance_prompt_with_progress(
        &self,
        args: EnhanceArgs,
        progress: Option<&(dyn Fn(sr_common::api::RequestPhase) + Send + Sync)>,
        cancel: &CancellationToken,
    ) -> Result<EnhanceOutput, Error> {
        let slot = self.llm_slot().ok_or_else(not_configured)?;
        let (scope, names) = self.answer_scope(args.folder_ids.as_deref()).await?;
        if let Some(p) = progress {
            p(sr_common::api::RequestPhase::Searching);
        }
        let mut hits = Vec::new();
        if sr_core::enhance::can_enhance(&args.prompt) && !scope.is_empty() {
            let found = self
                .search(SearchArgs {
                    query: args.prompt.trim().to_string(),
                    max_results: Some(sr_core::enhance::ENHANCE_MAX_RESULTS),
                    min_score: Some(sr_core::enhance::ENHANCE_MIN_SCORE),
                    folder_ids: Some(scope.clone()),
                    mode: None,
                    collapse_duplicates: None,
                })
                .await?;
            hits = found
                .hits
                .into_iter()
                .map(|h| EnhanceHit {
                    rel_path: h.rel_path,
                    line_start: h.line_start,
                    text: h.snippet,
                })
                .collect();
        }
        let tree = self.tree_summaries(&scope, &names).await?;
        let input = EnhanceInput {
            prompt: args.prompt,
            searched: !scope.is_empty(),
            hits,
            tree,
            memories: args.memories,
            history: args.history,
            attachments: args.attachments,
            agent: args.agent,
        };
        let cfg = EnhanceConfig {
            model: slot.model.clone(),
            egress: sr_providers::EgressCtx {
                collection_id: Some(self.inner().default_collection.to_string()),
                client_id: args.client_id,
            },
            ..EnhanceConfig::default()
        };
        if let Some(p) = progress {
            p(sr_common::api::RequestPhase::Improving);
        }
        let r = sr_core::enhance::enhance(&*slot.llm, &cfg, &input, cancel).await?;
        tracing::info!(target: "sr_engine::enhance", used_llm = r.used_llm, fallback = r.fallback, files = r.files.len(), "prompt enhanced");
        Ok(EnhanceOutput {
            result: r,
            provider: Some(slot.display_name.clone()),
            model: (!slot.model.is_empty()).then(|| slot.model.clone()),
        })
    }

    /// `<file_tree>` for the enhancer (Selene `formatFileTreeCompact` inputs): per folder, its
    /// file count, directory count (to depth 3) and top-level directories, from the catalog.
    async fn tree_summaries(
        &self,
        scope: &[FolderId],
        names: &FolderNames,
    ) -> Result<Vec<sr_core::enhance::TreeSummary>, Error> {
        let mut out = Vec::new();
        for id in scope {
            let fid = *id;
            let paths: Vec<String> = self
                .inner()
                .cat
                .read_async(move |c| {
                    let mut st = c
                        .prepare(
                            "SELECT rel_path FROM files WHERE folder_id = ?1 AND state != 'dead'
                             ORDER BY rel_path LIMIT 20000",
                        )
                        .map_err(sr_catalog::map_db_err)?;
                    let v = st
                        .query_map([fid.to_bytes()], |r| r.get::<_, String>(0))
                        .map_err(sr_catalog::map_db_err)?
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map_err(sr_catalog::map_db_err)?;
                    Ok(v)
                })
                .await?;
            let mut dirs = std::collections::BTreeSet::new();
            let mut top = Vec::<String>::new();
            for p in &paths {
                let parts: Vec<&str> = p.split(['/', '\\']).collect();
                for d in 1..parts
                    .len()
                    .min(sr_core::enhance::ENHANCE_TREE_MAX_DEPTH + 1)
                {
                    dirs.insert(parts[..d].join("/"));
                }
                if parts.len() > 1 && !top.iter().any(|t| t == parts[0]) {
                    top.push(parts[0].to_string());
                }
            }
            out.push(sr_core::enhance::TreeSummary {
                display_name: names.get(id).cloned().unwrap_or_default(),
                total_files: paths.len() as u64,
                total_directories: dirs.len() as u64,
                top_dirs: top,
            });
        }
        Ok(out)
    }
}
