//! MCP tool contracts (docs/03 search / list_folders rows; Amendment 16 names and opt-in tools).
//!
//! * `context_search`: `query`; `maxResults` (default 50, cap 150; aliases `limit`, `topK`,
//!   `max_results`); `minScore` (0..1, default 0.1, §2.5 semantics); `folderIds` (ULIDs); `mode`
//!   (`hybrid` | `dense_only`); `debug`. Result: `structuredContent.folders[]` (`root`, `name`,
//!   `id` of each folder in the result) and `hits[]` with folder-relative `path`, `line_start`,
//!   `line_end`, `snippet`, `f` (1-based folder index, only when more than one folder appears)
//!   and `also_at` (identical text elsewhere, `path:a-b`). Flags only when `true`
//!   (`no_good_match`, `no_folders`, `no_access`).
//! * `list_folders`.
//! * `sync_folder` (`folderId`, optional). Listed only when "Let connected apps check folders for
//!   changes" is on (off by default, Amendment 16).
//! * `add_folder` (`path`). Listed only when "Let connected apps ask to add folders" is on (off
//!   by default, P1.2).
//! * `context_answer` (M3-Q): `question`; `folderIds`; `maxSources` (the most passages the model
//!   sees in total); `sources` (`cited` default | `all` | `none`); `debug`. Listed only when an AI
//!   provider is connected **and** "Let connected apps ask for answers" is on (U17). Result:
//!   `text` with `[n]` citations, `folders[]`, `sources[]` (one entry per cited source in order of
//!   first citation, with its original `n` and the **cited** lines: `line_start`/`line_end`, or
//!   `lines: ["a-b", …]` for several ranges), or `no_good_match` / `no_folders`.
//!
//! **Output efficiency (docs/06).** `false` flags, empty lists, `null`s, scores and diagnostics
//! (`provider`, `model`, `rounds`, `from_read`, `count`, `took_ms`, `score`, `scores`) are left
//! out unless `debug: true`, which returns everything (plus absolute paths). Error results are
//! unchanged (`isError` + `{code, retryable, retry_after}`).
//!
//! Bad arguments are JSON-RPC `-32602` (§2.5); engine failures are `isError` results with the one
//! plain message renderer.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use sr_common::ids::FolderId;
use sr_engine::{FolderInfo, SearchArgs, SearchMode, SearchOutput};

pub const CONTEXT_SEARCH: &str = "context_search";
pub const LIST_FOLDERS: &str = "list_folders";
/// Listed only when the user turned it on (Advanced → integrations; off by default, Amendment 16).
pub const SYNC_FOLDER: &str = "sync_folder";
/// Listed only when the user turned it on (Advanced → integrations; off by default, P1.2).
pub const ADD_FOLDER: &str = "add_folder";
/// Listed only with a connected provider and answers turned on (U17).
pub const CONTEXT_ANSWER: &str = "context_answer";

/// Tools every session lists (the others are opt-in, see [`crate::server`]).
pub const TOOLS: &[&str] = &[CONTEXT_SEARCH, LIST_FOLDERS];

/// Hard cap on `maxResults` (docs/03).
pub const MAX_RESULTS_CAP: u32 = 150;

/// Parse tool arguments; the error text goes into the `-32602` message.
pub fn parse<T: DeserializeOwned>(args: Option<Map<String, Value>>) -> Result<T, String> {
    serde_json::from_value(Value::Object(args.unwrap_or_default())).map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchParams {
    pub query: String,
    #[serde(
        default,
        alias = "max_results",
        alias = "limit",
        alias = "topK",
        alias = "top_k"
    )]
    pub max_results: Option<u32>,
    #[serde(default, alias = "min_score")]
    pub min_score: Option<f32>,
    #[serde(default, alias = "folder_ids", alias = "folders")]
    pub folder_ids: Option<Vec<FolderId>>,
    #[serde(default)]
    pub mode: Option<sr_core_mode::Mode>,
    /// Everything (scores, absolute paths, timings): for diagnosis, not for agents.
    #[serde(default)]
    pub debug: bool,
}

/// `mode` values accepted on the wire.
pub mod sr_core_mode {
    use serde::Deserialize;

    #[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum Mode {
        Hybrid,
        #[serde(alias = "dense")]
        DenseOnly,
    }
}

impl SearchParams {
    pub fn validate(self) -> Result<SearchArgs, String> {
        if self.query.trim().is_empty() {
            return Err("query must not be empty".into());
        }
        if self.query.len() > 8192 {
            return Err("query is too long".into());
        }
        if let Some(n) = self.max_results
            && n == 0
        {
            return Err("maxResults must be at least 1".into());
        }
        if let Some(s) = self.min_score
            && !(0.0..=1.0).contains(&s)
        {
            return Err("minScore must be between 0 and 1".into());
        }
        Ok(SearchArgs {
            query: self.query,
            max_results: self.max_results.map(|n| n.min(MAX_RESULTS_CAP)),
            min_score: self.min_score,
            folder_ids: self.folder_ids,
            mode: self.mode.map(|m| match m {
                sr_core_mode::Mode::Hybrid => SearchMode::Hybrid,
                sr_core_mode::Mode::DenseOnly => SearchMode::DenseOnly,
            }),
            collapse_duplicates: None,
        })
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncParams {
    #[serde(default, alias = "folder_id")]
    pub folder_id: Option<FolderId>,
}

#[derive(Debug, Deserialize, Default)]
pub struct NoParams {}

// ------------------------------------------------------------------------------------- schemas

fn obj(props: Value, required: &[&str]) -> Map<String, Value> {
    let v = json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": true,
    });
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

const ULID: &str = "^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$";

/// One tool's definition.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub schema: Map<String, Value>,
    pub read_only: bool,
}

/// The tools listed for every session, in listing order.
pub fn definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: CONTEXT_SEARCH,
            title: "Search your folders",
            description: "Search the user's indexed folders by meaning and by exact words (hybrid semantic + keyword search). \
             Returns the best matching passages with file path and line range.",
            schema: obj(
                json!({
                    "query": {"type": "string", "description": "What to look for: a question, a description, an identifier or a file name."},
                    "maxResults": {"type": "integer", "minimum": 1, "maximum": MAX_RESULTS_CAP, "default": 50, "description": "Maximum number of results (aliases: limit, topK)."},
                    "minScore": {"type": "number", "minimum": 0, "maximum": 1, "default": 0.1, "description": "Drop weak semantic matches below this similarity; exact keyword top matches always pass."},
                    "folderIds": {"type": "array", "items": {"type": "string", "pattern": ULID}, "description": "Only search these folders (ids from list_folders)."},
                    "mode": {"type": "string", "enum": ["hybrid", "dense_only"], "default": "hybrid"},
                    "debug": {"type": "boolean", "default": false, "description": "Include scores, absolute paths and timings (diagnosis only)."}
                }),
                &["query"],
            ),
            read_only: true,
        },
        ToolDef {
            name: LIST_FOLDERS,
            title: "List folders",
            description: "List the folders the user has added to Selene Retrieval, with their ids, paths and indexing status.",
            schema: obj(json!({}), &[]),
            read_only: true,
        },
    ]
}

/// `sync_folder` (only listed when turned on): checks a folder for changes now.
pub fn sync_folder_definition() -> ToolDef {
    ToolDef {
        name: SYNC_FOLDER,
        title: "Check a folder for changes",
        description: "Look for new, changed or deleted files in one folder (or all folders) and update the index now. \
         Folders usually update on their own; a folder set to manual checks updates only when checked. \
         Send a progress token to receive progress until it finishes.",
        schema: obj(
            json!({"folderId": {"type": "string", "pattern": ULID, "description": "Folder id from list_folders; omit for all folders."}}),
            &[],
        ),
        read_only: false,
    }
}

/// `add_folder` (only listed when turned on): asks the user to approve a folder in the app.
pub fn add_folder_definition() -> ToolDef {
    ToolDef {
        name: ADD_FOLDER,
        title: "Ask to add a folder",
        description: "Ask the user to add a folder to Selene Retrieval. Nothing is added until the user approves it in \
         the Selene Retrieval app (requests expire after 10 minutes); tell the user to look there.",
        schema: obj(
            json!({"path": {"type": "string", "description": "Absolute path of the folder."}}),
            &["path"],
        ),
        read_only: false,
    }
}

/// `add_folder` arguments.
#[derive(Debug, Deserialize)]
pub struct AddFolderParams {
    pub path: String,
}

/// `context_answer` (listed only when a provider is connected and answers are on).
pub fn answer_definition() -> ToolDef {
    ToolDef {
        name: CONTEXT_ANSWER,
        title: "Answer from your files",
        description: "Write a short answer to a question using only the user's indexed folders, with numbered citations \
         ([1], [2]) that point to file paths and line ranges. Uses the user's AI provider (it may take up to a minute). \
         If nothing in the folders matches, says so instead of guessing.",
        schema: obj(
            json!({
                "question": {"type": "string", "description": "The question, in the user's words."},
                "folderIds": {"type": "array", "items": {"type": "string", "pattern": ULID}, "description": "Only use these folders (ids from list_folders)."},
                "maxSources": {"type": "integer", "minimum": 1, "maximum": 30, "default": 12, "description": "The most passages the answer may use in total."},
                "sources": {"type": "string", "enum": ["cited", "all", "none"], "default": "cited", "description": "Which sources to list: the cited ones, all passages used, or none."},
                "debug": {"type": "boolean", "default": false, "description": "Include provider, model, rounds and every source (diagnosis only)."}
            }),
            &["question"],
        ),
        read_only: true,
    }
}

/// `context_answer` arguments.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerParams {
    #[serde(alias = "query")]
    pub question: String,
    #[serde(default, alias = "folder_ids")]
    pub folder_ids: Option<Vec<FolderId>>,
    #[serde(default, alias = "max_sources")]
    pub max_sources: Option<u32>,
    #[serde(default)]
    pub sources: SourcesMode,
    #[serde(default)]
    pub debug: bool,
}

/// `context_answer` `sources`: which sources the result lists (docs/06 change 1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourcesMode {
    /// The cited sources, in order of first citation, with the cited lines.
    #[default]
    Cited,
    /// Every source the model was shown (uncited ones marked `"cited": false`).
    All,
    /// None: the text only.
    #[serde(rename = "none")]
    NoSources,
}

impl AnswerParams {
    pub fn validate(self, client_id: &str) -> Result<sr_engine::answers::AnswerArgs, String> {
        if self.question.trim().is_empty() {
            return Err("question must not be empty".into());
        }
        if self.question.len() > 8192 {
            return Err("question is too long".into());
        }
        if self.max_sources == Some(0) {
            return Err("maxSources must be at least 1".into());
        }
        Ok(sr_engine::answers::AnswerArgs {
            question: self.question,
            folder_ids: self.folder_ids,
            max_sources: self.max_sources.map(|n| n.min(30)),
            client_id: Some(client_id.to_string()),
        })
    }
}

// ------------------------------------------------------------ locations (docs/06 changes 2–4)

/// The `folders` table of one response: every folder that appears in it, in order of first
/// appearance, with `root`, `name` and `id` (so a location stays unambiguous: `root` + `path`,
/// or `path` + `folderIds: [id]`). Rows carry folder-relative paths, and a 1-based `f` only when
/// more than one folder appears.
#[derive(Debug, Default)]
struct FolderTable {
    /// `(id, root, name)`.
    list: Vec<(String, String, String)>,
}

impl FolderTable {
    /// Adds the folder on first use.
    fn add(&mut self, id: &str, root: &str, name: &str) {
        if !self.list.iter().any(|f| f.0 == id) {
            self.list
                .push((id.to_string(), root.to_string(), name.to_string()));
        }
    }

    /// 1-based index of a folder already added.
    fn index(&self, id: &str) -> usize {
        self.list.iter().position(|f| f.0 == id).unwrap_or(0) + 1
    }

    fn multi(&self) -> bool {
        self.list.len() > 1
    }

    fn json(&self) -> Value {
        Value::Array(
            self.list
                .iter()
                .map(|(id, root, name)| {
                    let mut m = Map::new();
                    m.insert("root".into(), json!(root));
                    m.insert("name".into(), json!(name));
                    if !id.is_empty() {
                        m.insert("id".into(), json!(id));
                    }
                    Value::Object(m)
                })
                .collect(),
        )
    }

    /// Text heading suffix: `in C:\root` for one folder, else the numbered legend.
    fn heading(&self) -> String {
        match self.list.as_slice() {
            [one] => format!("in {}", one.1),
            many => {
                let legend: Vec<String> = many
                    .iter()
                    .enumerate()
                    .map(|(i, f)| format!("{} = {}", i + 1, f.1))
                    .collect();
                format!("folders: {}", legend.join(", "))
            }
        }
    }

    /// A row's location: `rel:a-b`, prefixed `k:` with several folders (a folder-relative path
    /// never contains `:`, so the string parses unambiguously).
    fn loc(&self, id: &str, rel: &str, ranges: &[(u32, u32)]) -> String {
        let r: Vec<String> = ranges.iter().map(|(a, b)| format!("{a}-{b}")).collect();
        if self.multi() {
            format!("{}:{rel}:{}", self.index(id), r.join(", "))
        } else {
            format!("{rel}:{}", r.join(", "))
        }
    }

    fn also_at(&self, also: &[sr_engine::AlsoAtOut]) -> Vec<String> {
        also.iter()
            .map(|a| {
                self.loc(
                    &a.folder_id.to_string(),
                    &a.rel_path,
                    &[(a.line_start, a.line_end)],
                )
            })
            .collect()
    }
}

/// Adds a row's fields shared by `context_search` hits and `context_answer` sources: `f`
/// (several folders only) and `also_at` (identical text elsewhere, only when there is some).
fn put_place(m: &mut Map<String, Value>, t: &FolderTable, id: &str, also: &[sr_engine::AlsoAtOut]) {
    if t.multi() {
        m.insert("f".into(), json!(t.index(id)));
    }
    if !also.is_empty() {
        m.insert("also_at".into(), json!(t.also_at(also)));
    }
}

/// One listed `context_answer` source: the cited ranges (in order of first citation), or the
/// source's own range for an uncited one (`"all"` / debug).
struct AnswerRow<'a> {
    src: &'a sr_engine::answers::SourceOut,
    ranges: Vec<(u32, u32)>,
    cited: bool,
}

fn source_folder_key(s: &sr_engine::answers::SourceOut) -> String {
    s.folder_id.map(|f| f.to_string()).unwrap_or_default()
}

/// The sources an answer lists (docs/06 change 1): cited ones in order of first citation with
/// their original `n` and cited lines; with `all` (or `debug`) the uncited ones follow by `n`.
fn answer_rows(
    a: &sr_engine::answers::AnswerOutput,
    mode: SourcesMode,
    debug: bool,
) -> Vec<AnswerRow<'_>> {
    if mode == SourcesMode::NoSources {
        return Vec::new();
    }
    let mut rows: Vec<AnswerRow<'_>> = Vec::new();
    for c in &a.citations {
        let Some(src) = a.sources.iter().find(|s| s.n == c.source) else {
            continue;
        };
        let range = (c.line_start, c.line_end);
        match rows.iter_mut().find(|r| r.src.n == c.source) {
            Some(r) if !r.ranges.contains(&range) => r.ranges.push(range),
            Some(_) => {}
            None => rows.push(AnswerRow {
                src,
                ranges: vec![range],
                cited: true,
            }),
        }
    }
    if mode == SourcesMode::All || debug {
        let mut rest: Vec<&sr_engine::answers::SourceOut> = a
            .sources
            .iter()
            .filter(|s| !rows.iter().any(|r| r.src.n == s.n))
            .collect();
        rest.sort_by_key(|s| s.n);
        rows.extend(rest.into_iter().map(|src| AnswerRow {
            src,
            ranges: vec![(src.line_start, src.line_end)],
            cited: false,
        }));
    }
    rows
}

fn answer_table(rows: &[AnswerRow<'_>]) -> FolderTable {
    let mut t = FolderTable::default();
    for r in rows {
        t.add(
            &source_folder_key(r.src),
            &r.src.folder_root,
            &r.src.folder_name,
        );
        for x in &r.src.also_at {
            t.add(&x.folder_id.to_string(), &x.folder_root, &x.folder_name);
        }
    }
    t
}

/// `structuredContent` of an answer (docs/06 changes 1–3): `text`, the `folders` table and the
/// listed `sources`; flags only when `true`; diagnostics only with `debug`.
pub fn answer_structured(
    a: &sr_engine::answers::AnswerOutput,
    mode: SourcesMode,
    debug: bool,
) -> Value {
    let mut m = Map::new();
    if !a.text.is_empty() || debug {
        m.insert("text".into(), json!(a.text));
    }
    let rows = answer_rows(a, mode, debug);
    let t = answer_table(&rows);
    if !rows.is_empty() {
        m.insert("folders".into(), t.json());
        let sources: Vec<Value> = rows
            .iter()
            .map(|r| {
                let s = r.src;
                let id = source_folder_key(s);
                let mut e = Map::new();
                e.insert("n".into(), json!(s.n));
                e.insert("path".into(), json!(s.rel_path));
                if let [(a, b)] = r.ranges.as_slice() {
                    e.insert("line_start".into(), json!(a));
                    e.insert("line_end".into(), json!(b));
                } else {
                    let lines: Vec<String> =
                        r.ranges.iter().map(|(a, b)| format!("{a}-{b}")).collect();
                    e.insert("lines".into(), json!(lines));
                }
                put_place(&mut e, &t, &id, &s.also_at);
                if !r.cited {
                    e.insert("cited".into(), json!(false));
                } else if mode == SourcesMode::All || debug {
                    // The whole passage the model was shown (decision 3: `all` / debug only).
                    e.insert(
                        "source_lines".into(),
                        json!(format!("{}-{}", s.line_start, s.line_end)),
                    );
                }
                if debug {
                    e.insert("from_read".into(), json!(s.from_read));
                    e.insert("abs_path".into(), json!(s.path));
                    e.insert("folder".into(), json!(s.folder_name));
                }
                Value::Object(e)
            })
            .collect();
        m.insert("sources".into(), Value::Array(sources));
    }
    if a.no_good_match || debug {
        m.insert("no_good_match".into(), json!(a.no_good_match));
    }
    if a.no_folders || debug {
        m.insert("no_folders".into(), json!(a.no_folders));
    }
    if debug {
        m.insert("provider".into(), json!(a.provider));
        m.insert("model".into(), json!(a.model));
        m.insert("rounds".into(), json!(a.rounds));
        m.insert(
            "citations".into(),
            json!(
                a.citations
                    .iter()
                    .map(|c| json!({"source": c.source, "line_start": c.line_start, "line_end": c.line_end}))
                    .collect::<Vec<_>>()
            ),
        );
    }
    Value::Object(m)
}

/// Readable answer: the text, then the listed sources as `[n] rel:a-b` under the folder root
/// (printed once), then who wrote it.
pub fn answer_text(
    a: &sr_engine::answers::AnswerOutput,
    question: &str,
    mode: SourcesMode,
) -> String {
    if a.no_folders {
        return "No folders have been added to Selene Retrieval yet. Ask the user to add a folder in Selene Retrieval.".into();
    }
    if a.no_good_match {
        let q: String = question.trim().chars().take(120).collect();
        return format!(
            "Nothing closely matched \u{201c}{q}\u{201d} in the user's folders, so there is nothing to answer from."
        );
    }
    let mut t = a.text.trim().to_string();
    let rows = answer_rows(a, mode, false);
    if !rows.is_empty() {
        let table = answer_table(&rows);
        t.push_str(&format!("\n\nSources ({}):", table.heading()));
        for r in &rows {
            let s = r.src;
            t.push_str(&format!(
                "\n[{}] {}{}",
                s.n,
                table.loc(&source_folder_key(s), &s.rel_path, &r.ranges),
                if r.cited { "" } else { " (not cited)" }
            ));
            if !s.also_at.is_empty() {
                t.push_str(&format!(
                    "\n    same text at: {}",
                    table.also_at(&s.also_at).join(", ")
                ));
            }
        }
    }
    if let Some(p) = &a.provider {
        t.push_str(&format!(
            "\n\nWritten by {p} from the user's files. Check the sources."
        ));
    }
    t
}

// ---------------------------------------------------------------------------- result shaping

fn search_table(out: &SearchOutput) -> FolderTable {
    let mut t = FolderTable::default();
    for h in &out.hits {
        t.add(&h.folder_id.to_string(), &h.folder_root, &h.folder_name);
        for x in &h.also_at {
            t.add(&x.folder_id.to_string(), &x.folder_root, &x.folder_name);
        }
    }
    t
}

/// `structuredContent` of a search (docs/06 changes 2–4). The xtask `mcp:` adapter reads
/// `hits[].path/line_start/line_end`; `path` is folder-relative, the root is in `folders`.
pub fn search_structured(out: &SearchOutput, debug: bool) -> Value {
    let t = search_table(out);
    let hits: Vec<Value> = out
        .hits
        .iter()
        .map(|h| {
            let id = h.folder_id.to_string();
            let mut m = Map::new();
            m.insert("path".into(), json!(h.rel_path));
            m.insert("line_start".into(), json!(h.line_start));
            m.insert("line_end".into(), json!(h.line_end));
            m.insert("snippet".into(), json!(h.snippet));
            put_place(&mut m, &t, &id, &h.also_at);
            if debug {
                m.insert("abs_path".into(), json!(h.path));
                m.insert("folder_id".into(), json!(id));
                m.insert("folder".into(), json!(h.folder_name));
                m.insert("score".into(), json!(h.score));
                m.insert("scores".into(), json!(h.scores));
            }
            Value::Object(m)
        })
        .collect();
    let mut m = Map::new();
    if !t.list.is_empty() {
        m.insert("folders".into(), t.json());
    }
    m.insert("hits".into(), Value::Array(hits));
    for (k, v) in [
        ("no_good_match", out.no_good_match),
        ("no_folders", out.no_folders),
        ("no_access", out.no_access),
    ] {
        if v || debug {
            m.insert(k.into(), json!(v));
        }
    }
    if debug {
        m.insert("count".into(), json!(out.hits.len()));
        m.insert(
            "took_ms".into(),
            json!((out.total_ms * 10.0).round() / 10.0),
        );
        m.insert("dropped_stale".into(), json!(out.dropped_stale));
        m.insert("hydrate_capped".into(), json!(out.hydrate_capped));
    }
    Value::Object(m)
}

/// Readable text for agents that ignore `structuredContent`: the folder root once, then
/// `rel:a-b` and the start of the passage per hit.
pub fn search_text(out: &SearchOutput, query: &str) -> String {
    if out.no_access {
        // M4 (contract §4): this app's scope names no folder.
        return sr_engine::clients::NO_ACCESS_TEXT.into();
    }
    if out.no_folders {
        return "No folders have been added to Selene Retrieval yet. Ask the user to add a folder in Selene Retrieval.".into();
    }
    if out.no_good_match {
        // R-UX18: one plain line, zero hits (never unrelated passages presented as matches).
        let q: String = query.trim().chars().take(120).collect();
        return format!("Nothing closely matched \u{201c}{q}\u{201d} in the user's folders.");
    }
    if out.hits.is_empty() {
        return "No matching passages were found in the user's folders.".into();
    }
    let table = search_table(out);
    let mut t = format!("{} results ({}):\n", out.hits.len(), table.heading());
    for (i, h) in out.hits.iter().enumerate() {
        let snippet: String = h
            .snippet
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let snippet: String = snippet.chars().take(300).collect();
        t.push_str(&format!(
            "\n{}. {}\n   {}\n",
            i + 1,
            table.loc(
                &h.folder_id.to_string(),
                &h.rel_path,
                &[(h.line_start, h.line_end)]
            ),
            snippet
        ));
        if !h.also_at.is_empty() {
            t.push_str(&format!(
                "   same text at: {}\n",
                table.also_at(&h.also_at).join(", ")
            ));
        }
    }
    t
}

pub fn folders_text(f: &[FolderInfo]) -> String {
    if f.is_empty() {
        return "No folders have been added to Selene Retrieval yet.".into();
    }
    let mut t = String::new();
    for x in f {
        t.push_str(&format!(
            "- {} ({}) id={} status={:?}, {} of {} files indexed{}\n",
            x.name,
            x.path,
            x.folder_id,
            x.status,
            x.files_indexed,
            x.files_total,
            if x.files_skipped > 0 {
                format!(", {} couldn't be read", x.files_skipped)
            } else {
                String::new()
            }
        ));
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Option<Map<String, Value>> {
        v.as_object().cloned()
    }

    #[test]
    fn no_good_match_is_one_plain_line_with_zero_hits() {
        let out = SearchOutput {
            hits: Vec::new(),
            total_ms: 1.0,
            dropped_stale: 0,
            no_folders: false,
            no_good_match: true,
            top_dense: Some(0.5),
            top_bm25: None,
            no_access: false,
            hydrate_capped: false,
        };
        assert_eq!(
            search_text(&out, "  sourdough starter  "),
            "Nothing closely matched \u{201c}sourdough starter\u{201d} in the user's folders."
        );
        // docs/06 change 3: exactly the signal, nothing else.
        let v = search_structured(&out, false);
        assert_eq!(v, json!({"hits": [], "no_good_match": true}));
        assert!(v.get("count").is_none());
        let d = search_structured(&out, true);
        assert_eq!(
            (&d["count"], &d["no_good_match"], &d["no_access"]),
            (&json!(0), &json!(true), &json!(false))
        );
    }

    // ---------------------------------------------------------------- docs/06 result shapes

    const ROOT_A: &str = r"C:\Users\me\proj";
    const ROOT_B: &str = r"D:\notes";

    fn folder_a() -> FolderId {
        "01M5AQA1XSM1H9M41KECSCPK00".parse().unwrap()
    }

    fn folder_b() -> FolderId {
        "01M5AQA1XSM1H9M41KECSCPK01".parse().unwrap()
    }

    fn root_of(f: FolderId) -> (&'static str, &'static str) {
        if f == folder_a() {
            (ROOT_A, "proj")
        } else {
            (ROOT_B, "notes")
        }
    }

    fn hit(f: FolderId, rel: &str, a: u32, b: u32) -> sr_engine::SearchHitOut {
        let (root, name) = root_of(f);
        sr_engine::SearchHitOut {
            path: format!("{root}\\{}", rel.replace('/', "\\")),
            rel_path: rel.into(),
            folder_id: f,
            folder_name: name.into(),
            folder_root: root.into(),
            line_start: a,
            line_end: b,
            snippet: format!("text of {rel}"),
            score: 0.03,
            scores: sr_engine::Scores {
                rrf: 0.03,
                dense: Some(0.8),
                bm25: None,
                rerank: None,
            },
            also_at: Vec::new(),
            text_hash: None,
        }
    }

    fn also(f: FolderId, rel: &str, a: u32, b: u32) -> sr_engine::AlsoAtOut {
        let (root, name) = root_of(f);
        sr_engine::AlsoAtOut {
            folder_id: f,
            folder_name: name.into(),
            folder_root: root.into(),
            rel_path: rel.into(),
            line_start: a,
            line_end: b,
        }
    }

    fn found(hits: Vec<sr_engine::SearchHitOut>) -> SearchOutput {
        SearchOutput {
            hits,
            total_ms: 12.34,
            dropped_stale: 0,
            no_folders: false,
            no_good_match: false,
            top_dense: Some(0.8),
            top_bm25: None,
            no_access: false,
            hydrate_capped: false,
        }
    }

    #[test]
    fn search_prints_the_folder_once_and_relative_paths() {
        let mut h = hit(folder_a(), "patches/merge.rs", 200, 232);
        h.also_at = vec![also(folder_a(), "spikes/x/patches/merge.rs", 200, 232)];
        let out = found(vec![h, hit(folder_a(), "crates/sync/src/walker.rs", 1, 22)]);
        let v = search_structured(&out, false);
        assert_eq!(
            v["folders"],
            json!([{"root": ROOT_A, "name": "proj", "id": folder_a().to_string()}])
        );
        assert_eq!(
            v["hits"][0],
            json!({"path": "patches/merge.rs", "line_start": 200, "line_end": 232,
                   "snippet": "text of patches/merge.rs",
                   "also_at": ["spikes/x/patches/merge.rs:200-232"]})
        );
        // One folder: no per-row folder index; no scores, counts or timings.
        assert_eq!(
            v["hits"][1].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["path", "line_start", "line_end", "snippet"]
        );
        for k in [
            "count",
            "took_ms",
            "no_good_match",
            "no_access",
            "no_folders",
        ] {
            assert!(v.get(k).is_none(), "{k}");
        }
        let t = search_text(&out, "q");
        assert!(t.starts_with(&format!("2 results (in {ROOT_A}):\n")), "{t}");
        assert!(t.contains("\n1. patches/merge.rs:200-232\n"), "{t}");
        assert!(
            t.contains("same text at: spikes/x/patches/merge.rs:200-232"),
            "{t}"
        );
        assert!(
            !t.contains(r"C:\Users\me\proj\crates"),
            "root printed once: {t}"
        );
        // debug: today's fields come back.
        let d = search_structured(&out, true);
        assert_eq!(d["count"], 2);
        assert_eq!(d["took_ms"], 12.3);
        assert_eq!(
            d["hits"][1]["abs_path"],
            r"C:\Users\me\proj\crates\sync\src\walker.rs"
        );
        assert_eq!(d["hits"][1]["folder_id"], folder_a().to_string());
        assert!(d["hits"][1]["score"].is_number() && d["hits"][1]["scores"]["dense"].is_number());
        assert_eq!(d["no_good_match"], false);
    }

    #[test]
    fn search_over_two_folders_numbers_them() {
        let mut h = hit(folder_b(), "README.md", 1, 9);
        h.also_at = vec![also(folder_a(), "docs/README.md", 3, 11)];
        let out = found(vec![hit(folder_a(), "README.md", 1, 9), h]);
        let v = search_structured(&out, false);
        assert_eq!(v["folders"].as_array().unwrap().len(), 2);
        assert_eq!(v["folders"][1]["root"], ROOT_B);
        assert_eq!(v["hits"][0]["f"], 1);
        assert_eq!(v["hits"][1]["f"], 2);
        assert_eq!(v["hits"][1]["also_at"], json!(["1:docs/README.md:3-11"]));
        let t = search_text(&out, "q");
        assert!(
            t.starts_with(&format!(
                "2 results (folders: 1 = {ROOT_A}, 2 = {ROOT_B}):\n"
            )),
            "{t}"
        );
        assert!(t.contains("\n2. 2:README.md:1-9\n"), "{t}");
    }

    fn source(n: usize, f: FolderId, rel: &str, a: u32, b: u32) -> sr_engine::answers::SourceOut {
        let (root, name) = root_of(f);
        sr_engine::answers::SourceOut {
            n,
            path: format!("{root}\\{}", rel.replace('/', "\\")),
            rel_path: rel.into(),
            folder_name: name.into(),
            folder_id: Some(f),
            folder_root: root.into(),
            line_start: a,
            line_end: b,
            snippet: String::new(),
            from_read: n == 9,
            also_at: Vec::new(),
        }
    }

    fn cite(n: usize, a: u32, b: u32) -> sr_engine::answers::Citation {
        sr_engine::answers::Citation {
            source: n,
            path: String::new(),
            line_start: a,
            line_end: b,
        }
    }

    /// A D1-like answer: 9 sources, sources 7 and 3 cited (7 twice, with two ranges).
    fn d1() -> sr_engine::answers::AnswerOutput {
        sr_engine::answers::AnswerOutput {
            text: "merge.rs does it [7], see also [3] and [7].".into(),
            citations: vec![cite(7, 208, 216), cite(3, 5, 9), cite(7, 220, 224)],
            sources: (1..=9)
                .map(|n| {
                    source(
                        n,
                        folder_a(),
                        &format!("src/f{n}.rs"),
                        1,
                        if n == 7 { 240 } else { 17 },
                    )
                })
                .collect(),
            no_good_match: false,
            no_folders: false,
            provider: Some("Ollama".into()),
            model: Some("stub-model".into()),
            rounds: 2,
            tool_steps: 1,
            files_read: Vec::new(),
            structured: true,
            input_tokens: 10,
            output_tokens: 5,
        }
    }

    #[test]
    fn answer_lists_cited_sources_once_with_original_numbers() {
        let a = d1();
        let v = answer_structured(&a, SourcesMode::Cited, false);
        // Snapshot of the D1-like default shape (docs/06 §4.1 "After").
        assert_eq!(
            v,
            json!({
                "text": "merge.rs does it [7], see also [3] and [7].",
                "folders": [{"root": ROOT_A, "name": "proj", "id": folder_a().to_string()}],
                "sources": [
                    {"n": 7, "path": "src/f7.rs", "lines": ["208-216", "220-224"]},
                    {"n": 3, "path": "src/f3.rs", "line_start": 5, "line_end": 9}
                ]
            })
        );
        let t = answer_text(&a, "q", SourcesMode::Cited);
        assert_eq!(
            t,
            format!(
                "merge.rs does it [7], see also [3] and [7].\n\nSources (in {ROOT_A}):\n[7] src/f7.rs:208-216, 220-224\n[3] src/f3.rs:5-9\n\nWritten by Ollama from the user's files. Check the sources."
            )
        );
    }

    #[test]
    fn answer_all_none_and_debug() {
        let a = d1();
        let v = answer_structured(&a, SourcesMode::All, false);
        let s = v["sources"].as_array().unwrap();
        assert_eq!(s.len(), 9);
        assert_eq!(
            s.iter()
                .map(|x| x["n"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [7, 3, 1, 2, 4, 5, 6, 8, 9]
        );
        assert_eq!(s[0]["source_lines"], "1-240");
        assert_eq!(s[2]["cited"], false);
        assert_eq!(
            (&s[2]["line_start"], &s[2]["line_end"]),
            (&json!(1), &json!(17))
        );
        assert!(s[0].get("cited").is_none());
        assert!(
            answer_text(&a, "q", SourcesMode::All).contains("\n[1] src/f1.rs:1-17 (not cited)")
        );
        // none: the text only.
        let v = answer_structured(&a, SourcesMode::NoSources, false);
        assert_eq!(
            v,
            json!({"text": "merge.rs does it [7], see also [3] and [7]."})
        );
        assert!(!answer_text(&a, "q", SourcesMode::NoSources).contains("Sources"));
        // debug: provider, model, rounds, flags, citations, every source with from_read.
        let d = answer_structured(&a, SourcesMode::Cited, true);
        assert_eq!(
            (&d["provider"], &d["model"], &d["rounds"]),
            (&json!("Ollama"), &json!("stub-model"), &json!(2))
        );
        assert_eq!(
            (&d["no_good_match"], &d["no_folders"]),
            (&json!(false), &json!(false))
        );
        assert_eq!(d["citations"].as_array().unwrap().len(), 3);
        let ds = d["sources"].as_array().unwrap();
        assert_eq!(ds.len(), 9);
        assert_eq!(ds.last().unwrap()["from_read"], true);
        // none + debug: debug adds its fields, the source list stays off.
        let d = answer_structured(&a, SourcesMode::NoSources, true);
        assert!(d.get("sources").is_none() && d["provider"] == "Ollama");
    }

    #[test]
    fn answer_flags_only_when_true() {
        let mut a = d1();
        a.text.clear();
        a.citations.clear();
        a.sources.clear();
        a.no_good_match = true;
        assert_eq!(
            answer_structured(&a, SourcesMode::Cited, false),
            json!({"no_good_match": true})
        );
        assert!(answer_text(&a, "why?", SourcesMode::Cited).starts_with("Nothing closely matched"));
        a.no_good_match = false;
        a.no_folders = true;
        assert_eq!(
            answer_structured(&a, SourcesMode::Cited, false),
            json!({"no_folders": true})
        );
    }

    #[test]
    fn answer_sources_from_two_folders_and_copies() {
        let mut a = d1();
        a.sources[2] = source(3, folder_b(), "notes.md", 1, 30);
        a.sources[6].also_at = vec![also(folder_b(), "copy/f7.rs", 1, 240)];
        let v = answer_structured(&a, SourcesMode::Cited, false);
        assert_eq!(v["folders"].as_array().unwrap().len(), 2);
        assert_eq!(v["sources"][0]["f"], 1);
        assert_eq!(v["sources"][0]["also_at"], json!(["2:copy/f7.rs:1-240"]));
        assert_eq!(v["sources"][1]["f"], 2);
        let t = answer_text(&a, "q", SourcesMode::Cited);
        assert!(
            t.contains(&format!("Sources (folders: 1 = {ROOT_A}, 2 = {ROOT_B}):")),
            "{t}"
        );
        assert!(t.contains("\n[3] 2:notes.md:5-9"), "{t}");
        assert!(t.contains("same text at: 2:copy/f7.rs:1-240"), "{t}");
    }

    #[test]
    fn answer_sources_argument() {
        let p: AnswerParams = parse(args(json!({"question": "q"}))).unwrap();
        assert_eq!((p.sources, p.debug), (SourcesMode::Cited, false));
        let p: AnswerParams = parse(args(
            json!({"question": "q", "sources": "none", "debug": true}),
        ))
        .unwrap();
        assert_eq!((p.sources, p.debug), (SourcesMode::NoSources, true));
        let p: AnswerParams = parse(args(json!({"question": "q", "sources": "all"}))).unwrap();
        assert_eq!(p.sources, SourcesMode::All);
        // An unknown value is a bad argument (-32602 at the server).
        assert!(parse::<AnswerParams>(args(json!({"question": "q", "sources": "some"}))).is_err());
    }

    #[test]
    fn search_arguments_aliases_caps_and_validation() {
        let p: SearchParams = parse(args(json!({"query": "x", "limit": 500}))).unwrap();
        assert_eq!(p.validate().unwrap().max_results, Some(MAX_RESULTS_CAP));
        let p: SearchParams =
            parse(args(json!({"query": "x", "topK": 7, "minScore": 0.2}))).unwrap();
        let a = p.validate().unwrap();
        assert_eq!((a.max_results, a.min_score), (Some(7), Some(0.2)));
        let p: SearchParams = parse(args(json!({"query": "x"}))).unwrap();
        assert_eq!(
            p.validate().unwrap().max_results,
            None,
            "engine default 50 applies"
        );
        for bad in [
            json!({"query": ""}),
            json!({"query": "x", "minScore": 1.5}),
            json!({"query": "x", "maxResults": 0}),
        ] {
            let p: SearchParams = parse(args(bad.clone())).unwrap();
            assert!(p.validate().is_err(), "{bad}");
        }
        assert!(parse::<SearchParams>(args(json!({"maxResults": 3}))).is_err());
        assert!(parse::<SearchParams>(args(json!({"query": "x", "folderIds": ["nope"]}))).is_err());
        let p: SearchParams = parse(args(json!({"query": "x", "mode": "dense_only"}))).unwrap();
        assert_eq!(p.validate().unwrap().mode, Some(SearchMode::DenseOnly));
    }

    /// Amendment 16: two tools always, three opt-in ones; no text points to a tool that is gone.
    #[test]
    fn two_tools_always_and_three_opt_in_with_object_schemas() {
        let d = definitions();
        assert_eq!(d.iter().map(|t| t.name).collect::<Vec<_>>(), TOOLS);
        assert_eq!(TOOLS, [CONTEXT_SEARCH, LIST_FOLDERS]);
        let opt_in = [
            sync_folder_definition(),
            add_folder_definition(),
            answer_definition(),
        ];
        assert_eq!(
            opt_in.iter().map(|t| t.name).collect::<Vec<_>>(),
            ["sync_folder", "add_folder", "context_answer"]
        );
        for t in d.iter().chain(opt_in.iter()) {
            assert_eq!(t.schema["type"], "object");
            assert!(!t.description.is_empty() && !t.title.is_empty());
            let text = format!(
                "{} {} {}",
                t.title,
                t.description,
                Value::Object(t.schema.clone())
            );
            for gone in ["read_file", "grep", "get_status", "enhance_prompt"] {
                assert!(!text.contains(gone), "{} mentions {gone}: {text}", t.name);
            }
            assert!(!text.contains("  "), "{}: doubled spaces: {text}", t.name);
        }
    }

    #[test]
    fn answer_arguments() {
        let p: AnswerParams = parse(args(json!({"query": "why?", "max_sources": 99}))).unwrap();
        let a = p.validate("claude").unwrap();
        assert_eq!((a.question.as_str(), a.max_sources), ("why?", Some(30)));
        assert_eq!(a.client_id.as_deref(), Some("claude"));
        let p: AnswerParams = parse(args(json!({"question": "  "}))).unwrap();
        assert!(p.validate("c").is_err());
    }
}
