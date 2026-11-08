//! Amendment 16: the MCP tool surface over an in-process engine (test embedder, no extract
//! worker, no model files) and an in-memory MCP session. The default list is exactly
//! `context_search` and `list_folders`; removed names are unknown tools, and so are `sync_folder`,
//! `add_folder` and `context_answer` while they aren't listed; turning `sync_folder` on (or connecting a provider for
//! `context_answer`) lists the tool after `notifications/tools/list_changed`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use rmcp::ServiceExt;
use serde_json::{Value, json};
use sr_common::error::{Error, ErrorCode};
use sr_engine::appops::AdvancedUpdate;
use sr_engine::llm::LlmOverride;
use sr_engine::{EmbedderSource, Engine, EngineConfig, EngineHandle, ExtractSetup};
use sr_mcp_server::McpServer;
use sr_providers::{Caps, ChatRequest, ChatResponse, ChatStream, LlmProvider, ModelInfo};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio_util::sync::CancellationToken;

/// A chat provider that is never asked anything here (listing only needs it connected).
struct NoChat;

impl LlmProvider for NoChat {
    fn name(&self) -> &str {
        "OpenRouter"
    }
    fn capabilities(&self) -> Caps {
        Caps::default()
    }
    fn list_models<'a>(
        &'a self,
        _c: &'a CancellationToken,
    ) -> BoxFuture<'a, sr_common::Result<Vec<ModelInfo>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn chat<'a>(
        &'a self,
        _r: &'a ChatRequest,
        _c: &'a CancellationToken,
    ) -> BoxFuture<'a, sr_common::Result<ChatResponse>> {
        Box::pin(async { Err(Error::new(ErrorCode::ProviderRequest)) })
    }
    fn chat_stream<'a>(
        &'a self,
        _r: &'a ChatRequest,
        _c: &'a CancellationToken,
    ) -> BoxFuture<'a, sr_common::Result<ChatStream>> {
        Box::pin(async { Err(Error::new(ErrorCode::ProviderRequest)) })
    }
    fn validate_key<'a>(
        &'a self,
        _c: &'a CancellationToken,
    ) -> BoxFuture<'a, sr_common::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// A provider id from the embedded Selene registry that `providers.save` accepts.
fn provider_id() -> String {
    sr_providers::manifest()
        .providers()
        .find(|p| p.enabled && p.needs_key())
        .map(|p| p.id.clone())
        .expect("an enabled key-based provider in the manifest")
}

fn config(data: &Path) -> EngineConfig {
    let mut c = EngineConfig::with_embedder(
        data.to_path_buf(),
        data.join("app"),
        EmbedderSource::Provided(Arc::new(sr_providers::testing::HashEmbedder::new(
            "a16-tools",
            384,
        ))),
    );
    c.extract = ExtractSetup::Disabled;
    c.background = false;
    c.tokenizer_json = Some(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../core/testdata/bge-small-tokenizer.json"),
    );
    c.secrets = Some(Arc::new(sr_providers::MemorySecretStore::new()));
    c.llm = Some(LlmOverride {
        provider_id: provider_id(),
        display_name: "Test provider".into(),
        llm: Arc::new(NoChat),
    });
    c
}

/// The client end of one MCP session (newline-delimited JSON-RPC).
struct Session {
    r: BufReader<ReadHalf<DuplexStream>>,
    w: WriteHalf<DuplexStream>,
    notes: Vec<Value>,
    next_id: u64,
}

impl Session {
    async fn open(engine: &EngineHandle) -> Self {
        let (client, server_io) = tokio::io::duplex(1 << 20);
        let server = McpServer::new(engine.clone()).with_client("claude");
        tokio::spawn(async move {
            if let Ok(running) = server.serve(server_io).await {
                let _ = running.waiting().await;
            }
        });
        let (r, w) = tokio::io::split(client);
        let mut s = Self {
            r: BufReader::new(r),
            w,
            notes: Vec::new(),
            next_id: 1,
        };
        let init = s
            .call(
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "a16-test", "version": "0"}}),
            )
            .await;
        assert_eq!(init["result"]["serverInfo"]["name"], "selene-retrieval");
        let instructions = init["result"]["instructions"].as_str().unwrap();
        assert!(
            instructions.contains("`context_search`") && instructions.contains("`context_answer`"),
            "{instructions}"
        );
        for gone in [
            "read_file",
            "grep",
            "get_status",
            "enhance_prompt",
            "`search`",
        ] {
            assert!(!instructions.contains(gone), "{gone}: {instructions}");
        }
        s.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        s
    }

    async fn send(&mut self, v: Value) {
        let mut line = v.to_string();
        line.push('\n');
        self.w.write_all(line.as_bytes()).await.unwrap();
        self.w.flush().await.unwrap();
    }

    async fn read(&mut self) -> Value {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(30), self.r.read_line(&mut line))
            .await
            .expect("MCP message timed out")
            .unwrap();
        assert!(n > 0, "session closed");
        serde_json::from_str(line.trim()).unwrap()
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let v = self.read().await;
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
            self.notes.push(v);
        }
    }

    async fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.call("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    async fn names(&mut self) -> Vec<String> {
        let r = self.call("tools/list", json!({})).await;
        r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// Waits for `notifications/tools/list_changed` (one that already arrived counts) and
    /// consumes it.
    async fn list_changed(&mut self) {
        let is_it = |n: &Value| n["method"] == "notifications/tools/list_changed";
        loop {
            if let Some(i) = self.notes.iter().position(is_it) {
                self.notes.remove(i);
                return;
            }
            let v = self.read().await;
            self.notes.push(v);
        }
    }
}

/// `r` is exactly the answer a name that never existed gets (JSON-RPC `-32602`, no result).
async fn assert_unknown(s: &mut Session, r: &Value, name: &str) {
    assert!(r.get("result").is_none(), "{name}: {r}");
    let never = s.tool("no_such_tool", json!({})).await;
    let want = never["error"].to_string().replace("no_such_tool", name);
    assert_eq!(r["error"].to_string(), want, "{name}");
    assert_eq!(r["error"]["code"], -32602, "{name}: {r}");
}

fn advanced(sync_folder: bool) -> AdvancedUpdate {
    AdvancedUpdate {
        mcp_sync_folder_enabled: Some(sync_folder),
        ..AdvancedUpdate::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_surface_defaults_opt_in_and_refusals() {
    let t = tempfile::Builder::new().prefix("a16").tempdir().unwrap();
    let e = Engine::start(config(&t.path().join("d"))).await.unwrap();
    let mut s = Session::open(&e).await;

    // Default: exactly the two tools every session lists.
    assert_eq!(s.names().await, ["context_search", "list_folders"]);
    let r = s.tool("context_search", json!({"query": "anything"})).await;
    assert_ne!(r["result"]["isError"], true, "{r}");
    assert_eq!(r["result"]["structuredContent"]["no_folders"], true, "{r}");
    let r = s.tool("list_folders", json!({})).await;
    assert_eq!(
        r["result"]["structuredContent"]["folders"],
        json!([]),
        "{r}"
    );

    // Removed and renamed tools are unknown names (JSON-RPC -32602), like any other.
    for gone in [
        "search",
        "answer",
        "read_file",
        "grep",
        "get_status",
        "enhance_prompt",
        "no_such_tool",
    ] {
        let r = s.tool(gone, json!({"query": "x", "path": "a.md"})).await;
        assert_eq!(r["error"]["code"], -32602, "{gone}: {r}");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&format!("unknown tool {gone}")),
            "{gone}: {r}"
        );
    }

    // Hidden opt-in tools answer exactly like an unknown name and never run (no provider yet, so
    // `context_answer` is hidden too).
    for (name, args) in [
        ("sync_folder", json!({})),
        ("add_folder", json!({"path": t.path().to_string_lossy()})),
        (
            "context_answer",
            json!({"question": "how many copies are kept?"}),
        ),
    ] {
        let r = s.tool(name, args).await;
        assert_unknown(&mut s, &r, name).await;
    }
    // Guards "nothing was queued" (the `-32602` checks above prove the gate).
    assert!(e.pending_approvals().await.unwrap().is_empty());

    // "Let connected apps check folders for changes" on: tools/list_changed, then listed and
    // usable.
    e.update_advanced(advanced(true)).await.unwrap();
    s.list_changed().await;
    assert_eq!(
        s.names().await,
        ["context_search", "list_folders", "sync_folder"]
    );
    let r = s.tool("sync_folder", json!({})).await;
    assert_ne!(r["result"]["isError"], true, "{r}");
    assert_eq!(r["result"]["structuredContent"]["queued"], json!([]), "{r}");

    // Off again: hidden, and an unknown tool again.
    e.update_advanced(advanced(false)).await.unwrap();
    s.list_changed().await;
    assert_eq!(s.names().await, ["context_search", "list_folders"]);
    let r = s.tool("sync_folder", json!({})).await;
    assert_unknown(&mut s, &r, "sync_folder").await;

    // A connected provider (answers turn on with it) lists `context_answer`.
    e.rpc(
        "providers.save",
        json!({ "provider": provider_id(), "key": "sk-test-0000-1234", "check": false }),
    )
    .await
    .unwrap();
    s.list_changed().await;
    assert_eq!(
        s.names().await,
        ["context_search", "list_folders", "context_answer"]
    );
    // Answers off: hidden again although the provider stays connected.
    e.rpc(
        "settings.update_answers",
        json!({ "answers_enabled": false }),
    )
    .await
    .unwrap();
    s.list_changed().await;
    assert_eq!(s.names().await, ["context_search", "list_folders"]);
    let r = s
        .tool(
            "context_answer",
            json!({"question": "how many copies are kept?"}),
        )
        .await;
    assert_unknown(&mut s, &r, "context_answer").await;

    drop(s);
    e.drain().await;
}
