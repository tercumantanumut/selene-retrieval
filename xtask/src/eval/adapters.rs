//! Engine adapters. The harness only sees [`SearchAdapter`]; adding an engine means adding one
//! implementation here (and a spec prefix in [`from_spec`]).
//!
//! | spec | engine |
//! |---|---|
//! | `results:<file.json>` | precomputed results (baselines, offline runs, other tools) |
//! | `cmd:<program> [args..]` | any process speaking the JSONL protocol below (sr-core bench bin) |
//! | `mcp:<program> [args..]` | the daemon/bridge over MCP stdio (`tools/call context_search`) |
//!
//! `results` file: `{"engine","variant","corpus","results":[{"qid","latency_ms"?, "hits":[{"path",
//! "start_line","end_line","score"?}]}]}` (sr-core's `line_start`/`line_end` are accepted too).
//!
//! `cmd` protocol (one JSON object per line): the harness writes `{"qid","query","k"}`, the engine
//! answers `{"hits":[...], "latency_ms"?: f64}`. Without `latency_ms` the harness uses the
//! round-trip wall time. This is how sr-core runs in-process: a tiny bench binary built on
//! `sr_core::query::Searcher` reads queries and prints hits. That keeps xtask free of the
//! LanceDB/ORT dependency stack, which other crates are still building.
//!
//! `mcp` (experimental until the M1 MCP server lands): JSON-RPC 2.0 over stdio. It sends
//! `initialize` and `notifications/initialized`, then one `tools/call` per query with
//! `{"name":"context_search","arguments":{"query","maxResults":k}}`. Hits are read from
//! `structuredContent.hits` (or `.results`), falling back to JSON in `content[0].text`. The
//! latency is the full tool-call round trip. Server notifications (progress, logs) are skipped.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use super::golden::{GoldenQuery, Hit};

pub struct SearchOutput {
    pub hits: Vec<Hit>,
    pub latency_ms: Option<f64>,
}

pub trait SearchAdapter {
    /// Short engine name shown in reports.
    fn engine(&self) -> String;
    /// Variant label (model, quantisation, settings) shown in reports.
    fn variant(&self) -> String;
    fn search(&mut self, q: &GoldenQuery, k: usize) -> Result<SearchOutput, String>;
}

/// Parses `[label=]kind:arg`.
pub fn from_spec(
    spec: &str,
    timeout: Duration,
) -> Result<(Option<String>, Box<dyn SearchAdapter>), String> {
    let (label, rest) = match spec.split_once('=') {
        Some((l, r)) if !l.contains(':') && !l.is_empty() => (Some(l.to_string()), r),
        _ => (None, spec),
    };
    let (kind, arg) = rest.split_once(':').ok_or_else(|| {
        format!("engine spec {spec:?}: expected results:<file> | cmd:<program> | mcp:<program>")
    })?;
    let adapter: Box<dyn SearchAdapter> = match kind {
        "results" => Box::new(ResultsFile::load(Path::new(arg))?),
        "cmd" => Box::new(CommandEngine::spawn(arg, timeout)?),
        "mcp" => Box::new(McpStdio::spawn(arg, timeout)?),
        other => return Err(format!("unknown engine kind {other:?} in {spec:?}")),
    };
    Ok((label, adapter))
}

// ---------------------------------------------------------------------------------------------
// results:<file>

#[derive(Deserialize)]
struct ResultsDoc {
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    results: Vec<ResultRow>,
}

#[derive(Deserialize)]
struct ResultRow {
    qid: String,
    #[serde(default)]
    latency_ms: Option<f64>,
    hits: Vec<Hit>,
}

pub struct ResultsFile {
    engine: String,
    variant: String,
    rows: HashMap<String, (Vec<Hit>, Option<f64>)>,
}

impl ResultsFile {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let doc: ResultsDoc =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut rows = HashMap::new();
        for r in doc.results {
            if rows.insert(r.qid.clone(), (r.hits, r.latency_ms)).is_some() {
                return Err(format!("{}: duplicate qid {}", path.display(), r.qid));
            }
        }
        Ok(Self {
            engine: doc.engine.unwrap_or(stem),
            variant: doc.variant.unwrap_or_default(),
            rows,
        })
    }
}

impl SearchAdapter for ResultsFile {
    fn engine(&self) -> String {
        self.engine.clone()
    }
    fn variant(&self) -> String {
        self.variant.clone()
    }
    fn search(&mut self, q: &GoldenQuery, k: usize) -> Result<SearchOutput, String> {
        match self.rows.get(&q.qid) {
            Some((hits, lat)) => Ok(SearchOutput {
                hits: hits.iter().take(k).cloned().collect(),
                latency_ms: *lat,
            }),
            None => Err(format!("no results for qid {}", q.qid)),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// shared child-process plumbing

struct LineChild {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<std::io::Result<String>>,
    timeout: Duration,
    program: String,
}

impl LineChild {
    fn spawn(cmdline: &str, timeout: Duration) -> Result<Self, String> {
        Self::spawn_with_env(cmdline, timeout, &[])
    }

    fn spawn_with_env(
        cmdline: &str,
        timeout: Duration,
        envs: &[(&str, &str)],
    ) -> Result<Self, String> {
        let mut parts = cmdline.split_ascii_whitespace();
        let program = parts.next().ok_or("empty command")?.to_string();
        let mut child = Command::new(&program)
            .args(parts)
            .envs(envs.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("could not start {program}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines: rx,
            timeout,
            program,
        })
    }

    fn send(&mut self, v: &Value) -> Result<(), String> {
        let mut s = v.to_string();
        s.push('\n');
        self.stdin
            .write_all(s.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("{}: write failed: {e}", self.program))
    }

    fn recv(&self) -> Result<Value, String> {
        loop {
            match self.lines.recv_timeout(self.timeout) {
                // Protocol messages are JSON objects; anything else on stdout (banners, stray
                // logging) is skipped rather than treated as a protocol error.
                Ok(Ok(line)) if !line.trim_start().starts_with('{') => continue,
                Ok(Ok(line)) => {
                    return serde_json::from_str(&line).map_err(|e| {
                        format!("{}: bad JSON line ({e}): {}", self.program, truncate(&line))
                    });
                }
                Ok(Err(e)) => return Err(format!("{}: read failed: {e}", self.program)),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "{}: no answer within {:?}",
                        self.program, self.timeout
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!("{}: exited", self.program));
                }
            }
        }
    }
}

impl Drop for LineChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn truncate(s: &str) -> String {
    s.chars().take(200).collect()
}

fn parse_hits(v: &Value) -> Result<Vec<Hit>, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("bad hits array: {e}"))
}

// ---------------------------------------------------------------------------------------------
// cmd:<program>

pub struct CommandEngine {
    proc: LineChild,
    name: String,
}

impl CommandEngine {
    pub fn spawn(cmdline: &str, timeout: Duration) -> Result<Self, String> {
        Ok(Self::from_child(
            LineChild::spawn(cmdline, timeout)?,
            cmdline,
        ))
    }

    fn from_child(proc: LineChild, name: &str) -> Self {
        Self {
            proc,
            name: name.to_string(),
        }
    }
}

impl SearchAdapter for CommandEngine {
    fn engine(&self) -> String {
        format!("cmd:{}", self.name)
    }
    fn variant(&self) -> String {
        String::new()
    }
    fn search(&mut self, q: &GoldenQuery, k: usize) -> Result<SearchOutput, String> {
        let t0 = Instant::now();
        self.proc
            .send(&json!({"qid": q.qid, "query": q.query, "k": k}))?;
        let v = self.proc.recv()?;
        let wall = t0.elapsed().as_secs_f64() * 1000.0;
        if let Some(err) = v.get("error") {
            return Err(format!("engine error for {}: {err}", q.qid));
        }
        let hits = parse_hits(v.get("hits").ok_or("response has no \"hits\"")?)?;
        let latency_ms = v.get("latency_ms").and_then(Value::as_f64).or(Some(wall));
        Ok(SearchOutput {
            hits: hits.into_iter().take(k).collect(),
            latency_ms,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// mcp:<program>

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

pub struct McpStdio {
    proc: LineChild,
    name: String,
    next_id: u64,
    server: String,
}

impl McpStdio {
    pub fn spawn(cmdline: &str, timeout: Duration) -> Result<Self, String> {
        Self::from_child(LineChild::spawn(cmdline, timeout)?, cmdline)
    }

    fn from_child(proc: LineChild, name: &str) -> Result<Self, String> {
        let mut me = Self {
            proc,
            name: name.into(),
            next_id: 1,
            server: String::new(),
        };
        let init = me.call(
            "initialize",
            json!({"protocolVersion": MCP_PROTOCOL_VERSION, "capabilities": {},
                   "clientInfo": {"name": "xtask-eval", "version": env!("CARGO_PKG_VERSION")}}),
        )?;
        me.server = init
            .pointer("/serverInfo/name")
            .and_then(Value::as_str)
            .map(|n| {
                let v = init
                    .pointer("/serverInfo/version")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                format!("{n} {v}").trim().to_string()
            })
            .unwrap_or_default();
        me.proc
            .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        Ok(me)
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.proc
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let msg = self.proc.recv()?;
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue; // notification or a server->client request we don't serve
            }
            if let Some(err) = msg.get("error") {
                return Err(format!("{method}: {err}"));
            }
            return msg
                .get("result")
                .cloned()
                .ok_or_else(|| format!("{method}: response without result"));
        }
    }
}

/// Extracts hits from a `tools/call` result (structured content first, then text content).
pub fn mcp_result_hits(result: &Value) -> Result<Vec<Hit>, String> {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(format!("tool error: {}", truncate(&result.to_string())));
    }
    if let Some(sc) = result.get("structuredContent") {
        for key in ["hits", "results"] {
            if let Some(arr) = sc.get(key) {
                return parse_hits(arr);
            }
        }
    }
    let text = result
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .ok_or("tool result has neither structuredContent hits nor text content")?;
    let v: Value =
        serde_json::from_str(text).map_err(|e| format!("text content is not JSON: {e}"))?;
    for key in ["hits", "results"] {
        if let Some(arr) = v.get(key) {
            return parse_hits(arr);
        }
    }
    parse_hits(&v)
}

impl SearchAdapter for McpStdio {
    fn engine(&self) -> String {
        if self.server.is_empty() {
            format!("mcp:{}", self.name)
        } else {
            format!("mcp:{}", self.server)
        }
    }
    fn variant(&self) -> String {
        String::new()
    }
    fn search(&mut self, q: &GoldenQuery, k: usize) -> Result<SearchOutput, String> {
        let t0 = Instant::now();
        let res = self.call(
            "tools/call",
            json!({"name": "context_search", "arguments": {"query": q.query, "maxResults": k}}),
        )?;
        let latency = t0.elapsed().as_secs_f64() * 1000.0;
        let hits = mcp_result_hits(&res)?;
        Ok(SearchOutput {
            hits: hits.into_iter().take(k).collect(),
            latency_ms: Some(latency),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_ENV: &str = "XTASK_EVAL_FAKE_ENGINE";

    /// Not a real test: when re-launched by the round-trip tests below with `FAKE_ENV` set, this
    /// test body becomes a fake engine speaking the `cmd` or `mcp` protocol on stdin/stdout.
    /// libtest's own stdout lines ("running 1 test") are skipped by `LineChild::recv`; libtest
    /// leaves "test <name> ... " unterminated, so the fake engine starts with a newline.
    #[test]
    fn fake_engine_process() {
        let Ok(mode) = std::env::var(FAKE_ENV) else {
            return;
        };
        let stdin = std::io::stdin();
        let mut out = std::io::stdout();
        writeln!(out).unwrap();
        for line in stdin.lock().lines() {
            let msg: Value = serde_json::from_str(&line.unwrap()).unwrap();
            let reply = if mode == "cmd" {
                json!({"hits": [{"path": format!("{}.ts", msg["qid"].as_str().unwrap()), "start_line": 1, "end_line": msg["k"]}],
                       "latency_ms": 2.5})
            } else {
                let Some(id) = msg.get("id").cloned() else {
                    continue;
                }; // notifications/initialized
                writeln!(
                    out,
                    r#"{{"jsonrpc":"2.0","method":"notifications/message","params":{{}}}}"#
                )
                .unwrap();
                match msg["method"].as_str().unwrap() {
                    "initialize" => {
                        json!({"jsonrpc": "2.0", "id": id, "result": {"serverInfo": {"name": "fake", "version": "9"}}})
                    }
                    _ => {
                        json!({"jsonrpc": "2.0", "id": id, "result": {"structuredContent": {"hits": [
                        {"path": msg["params"]["arguments"]["query"], "line_start": 4, "line_end": 6}]}}})
                    }
                }
            };
            writeln!(out, "{reply}").unwrap();
            out.flush().unwrap();
        }
    }

    fn fake(mode: &str) -> LineChild {
        let exe = std::env::current_exe().unwrap();
        let cmdline = format!(
            "{} --exact eval::adapters::tests::fake_engine_process --nocapture --test-threads=1",
            exe.display()
        );
        LineChild::spawn_with_env(&cmdline, Duration::from_secs(30), &[(FAKE_ENV, mode)]).unwrap()
    }

    #[test]
    fn cmd_adapter_round_trip_through_a_child_process() {
        let mut e = CommandEngine::from_child(fake("cmd"), "fake");
        for qid in ["a", "b"] {
            let out = e.search(&q(qid), 7).unwrap();
            assert_eq!(
                out.hits,
                vec![Hit {
                    path: format!("{qid}.ts"),
                    start_line: Some(1),
                    end_line: Some(7),
                    score: None
                }]
            );
            assert_eq!(out.latency_ms, Some(2.5)); // engine-reported latency wins over wall time
        }
    }

    #[test]
    fn mcp_adapter_handshake_and_tool_call_skip_notifications() {
        let mut e = McpStdio::from_child(fake("mcp"), "fake").unwrap();
        assert_eq!(e.engine(), "mcp:fake 9");
        let mut query = q("a");
        query.query = "lib/x.rs".into();
        let out = e.search(&query, 5).unwrap();
        assert_eq!(out.hits[0].path, "lib/x.rs");
        assert_eq!(
            (out.hits[0].start_line, out.hits[0].end_line),
            (Some(4), Some(6))
        );
        assert!(out.latency_ms.is_some());
    }

    fn q(qid: &str) -> GoldenQuery {
        GoldenQuery {
            qid: qid.into(),
            query: "x".into(),
            kind: "nl".into(),
            relevant: vec![],
        }
    }

    #[test]
    fn results_file_adapter_truncates_to_k_and_reports_missing_qids() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("r.json");
        std::fs::write(
            &p,
            r#"{"engine":"e","variant":"v","results":[{"qid":"a","latency_ms":3.5,"hits":[
                {"path":"x","start_line":1,"end_line":2},{"path":"y","line_start":4,"line_end":5}]}]}"#,
        )
        .unwrap();
        let (label, mut a) = from_spec(
            &format!("base=results:{}", p.display()),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(label.as_deref(), Some("base"));
        assert_eq!((a.engine(), a.variant()), ("e".into(), "v".into()));
        let out = a.search(&q("a"), 1).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.latency_ms, Some(3.5));
        assert!(a.search(&q("zzz"), 10).is_err());
    }

    #[test]
    fn spec_parsing_rejects_unknown_kinds() {
        assert!(from_spec("nope:x", Duration::from_secs(1)).is_err());
        assert!(from_spec("plainfile.json", Duration::from_secs(1)).is_err());
    }

    #[test]
    fn mcp_hits_from_structured_or_text_content() {
        let structured =
            json!({"structuredContent": {"hits": [{"path": "a", "line_start": 1, "line_end": 3}]}});
        assert_eq!(mcp_result_hits(&structured).unwrap()[0].end_line, Some(3));
        let text =
            json!({"content": [{"type": "text", "text": "{\"results\":[{\"path\":\"b\"}]}"}]});
        assert_eq!(mcp_result_hits(&text).unwrap()[0].path, "b");
        assert!(mcp_result_hits(&json!({"isError": true, "content": []})).is_err());
    }
}
