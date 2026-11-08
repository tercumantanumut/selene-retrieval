//! M3-Q: the AI provider inside the engine (providers.* / consent.* / settings.answers app-RPC,
//! the consent gate, tool flags) and `answer` / `enhance_prompt` over indexed folders, with a
//! scripted fake chat provider (no network, no key leaves the test).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use futures::future::BoxFuture;
use serde_json::json;
use sr_common::error::{Error, ErrorCode, Purpose};
use sr_engine::answers::{AnswerArgs, EnhanceArgs};
use sr_engine::llm::LlmOverride;
use sr_engine::{Engine, ExtractSetup};
use sr_providers::{
    Caps, ChatRequest, ChatResponse, ChatStream, LlmProvider, MemorySecretStore, ModelInfo,
    PrivacyGate, SecretStore,
};
use tokio_util::sync::CancellationToken;

/// Replies from a script (last reply repeats); records every request; can fail every call.
struct FakeLlm {
    replies: Mutex<Vec<String>>,
    seen: Mutex<Vec<ChatRequest>>,
    fail: Option<ErrorCode>,
}

impl FakeLlm {
    fn new(replies: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.iter().map(|s| (*s).to_string()).collect()),
            seen: Mutex::new(Vec::new()),
            fail: None,
        })
    }
    fn failing(code: ErrorCode) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(Vec::new()),
            seen: Mutex::new(Vec::new()),
            fail: Some(code),
        })
    }
    fn prompts(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .flat_map(|r| r.messages.iter().map(|m| m.content.clone()))
            .collect()
    }
}

impl LlmProvider for FakeLlm {
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
        Box::pin(async {
            Ok(vec![ModelInfo {
                id: "test/model".into(),
                display_name: "Test model".into(),
                context_tokens: None,
            }])
        })
    }
    fn chat<'a>(
        &'a self,
        req: &'a ChatRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, sr_common::Result<ChatResponse>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(Error::new(ErrorCode::Cancelled));
            }
            self.seen.lock().unwrap().push(req.clone());
            if let Some(code) = self.fail {
                return Err(Error::new(code));
            }
            let mut r = self.replies.lock().unwrap();
            let text = if r.len() > 1 {
                r.remove(0)
            } else {
                r.first().cloned().unwrap_or_default()
            };
            Ok(ChatResponse {
                text,
                input_tokens: Some(10),
                output_tokens: Some(5),
            })
        })
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

/// A provider id from the embedded Selene registry that is enabled (so `providers.save` accepts it).
fn provider_id() -> String {
    sr_providers::manifest()
        .providers()
        .find(|p| p.enabled && p.needs_key())
        .map(|p| p.id.clone())
        .expect("an enabled key-based provider in the manifest")
}

fn noop() -> Arc<dyn Fn(sr_engine::answers::AnswerPhase) + Send + Sync> {
    Arc::new(|_| {})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_lifecycle_tool_flags_consent_and_scoped_answers() {
    let t = tmp();
    let data = t.path().join("d");
    let a_dir = t.path().join("alpha");
    let b_dir = t.path().join("beta");
    write_files(
        &a_dir,
        &[(
            "rotation.md",
            "The backup rotation keeps seven daily copies and four weekly ones.",
        )],
    );
    write_files(
        &b_dir,
        &[(
            "secret-plan.md",
            "The backup rotation in the other folder is unrelated text.",
        )],
    );
    let p = Profile::new("answers1");
    let id = provider_id();
    let llm = FakeLlm::new(&[
        r#"{"answer": "Seven daily copies and four weekly ones are kept [1].", "citations": [{"source": 1}], "enough": true}"#,
    ]);
    let secrets = Arc::new(MemorySecretStore::new());
    let mut cfg = config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &p))),
    );
    cfg.secrets = Some(secrets.clone());
    cfg.llm = Some(LlmOverride {
        provider_id: id.clone(),
        display_name: "Test provider".into(),
        llm: llm.clone(),
    });
    let e = Engine::start(cfg).await.unwrap();

    // No provider: no `context_answer`, plain "No AI provider set up"; the opt-in tools are off.
    let f = e.tool_flags();
    assert_eq!(f, sr_engine::llm::ToolFlags::default());
    let err = e
        .answer(
            AnswerArgs {
                question: "how many copies?".into(),
                ..AnswerArgs::default()
            },
            noop(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ProviderNotConfigured);

    // "Save without checking": key in the secret store (never echoed), answers turned on.
    let saved = e
        .rpc(
            "providers.save",
            json!({ "provider": id, "key": "sk-test-0000-1234", "check": false }),
        )
        .await
        .unwrap();
    assert_eq!(saved["validated"], false);
    let st = &saved["status"];
    assert_eq!(st["key_saved"], true);
    assert_eq!(st["key_last4"], "1234");
    assert_eq!(st["check"], "not_checked");
    assert_eq!(st["ready"], true);
    assert!(
        !saved.to_string().contains("sk-test-0000"),
        "the key is never echoed"
    );
    assert_eq!(
        secrets
            .get(&sr_providers::secrets::provider_account(&id))
            .unwrap()
            .as_deref(),
        Some("sk-test-0000-1234")
    );
    let f = e.tool_flags();
    assert!(f.answer && !f.add_folder && !f.sync_folder);
    let ans = e.rpc("settings.answers", json!({})).await.unwrap();
    assert_eq!(ans["answers_enabled"], true);
    assert_eq!(
        ans["deeper_answers"], false,
        "deeper answers stay off until toggled"
    );

    // Consent gate: a purpose without consent is refused (no egress), remembered and pushed.
    let mut rx = e.bus().subscribe();
    let gate = e.privacy_gate();
    let blocked = gate.check(&id, Purpose::Answer).unwrap_err();
    assert_eq!(blocked.code, ErrorCode::PrivacyBlocked);
    let s = e.status();
    let need = s
        .value
        .consent_needed
        .expect("consent_needed in the snapshot");
    assert_eq!(
        (need.provider.as_str(), need.purpose),
        (id.as_str(), Purpose::Answer)
    );
    let mut saw_needed = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(
            ev.payload,
            sr_common::events::EngineEvent::ConsentNeeded { .. }
        ) {
            saw_needed = true;
        }
    }
    assert!(saw_needed);
    // One switch covers answer + iterate (OQ-34).
    let rows = e
        .rpc(
            "consent.set",
            json!({ "provider": id, "purpose": "answer", "allowed": true }),
        )
        .await
        .unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|r| r["purpose"] == "iterate" && r["allowed"] == true)
    );
    assert!(gate.check(&id, Purpose::Answer).is_ok());
    assert!(gate.check(&id, Purpose::Iterate).is_ok());
    assert!(
        gate.check(&id, Purpose::Enhance).is_err(),
        "other purposes stay blocked"
    );
    assert!(
        e.status()
            .value
            .consent_needed
            .as_ref()
            .is_none_or(|n| n.purpose != Purpose::Answer)
    );

    // Answers use only the queried folder.
    let fa = e.add_folder(&a_dir.to_string_lossy(), None).await.unwrap();
    e.add_folder(&b_dir.to_string_lossy(), None).await.unwrap();
    e.run_pending_jobs().await.unwrap();
    let out = e
        .answer(
            AnswerArgs {
                question: "how many backup copies are kept?".into(),
                folder_ids: Some(vec![fa.folder_id]),
                ..AnswerArgs::default()
            },
            noop(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!out.no_good_match);
    assert!(out.text.contains("[1]"));
    assert_eq!(out.citations.len(), 1);
    assert!(
        out.sources.iter().all(|s| s.rel_path == "rotation.md"),
        "{:?}",
        out.sources
    );
    let prompts = llm.prompts().join("\n");
    assert!(prompts.contains("rotation.md"));
    assert!(
        !prompts.contains("secret-plan.md"),
        "the other folder never reaches the provider"
    );

    // Answers off → `context_answer` hidden although the provider stays connected.
    e.rpc(
        "settings.update_answers",
        json!({ "answers_enabled": false }),
    )
    .await
    .unwrap();
    let f = e.tool_flags();
    assert!(!f.answer && e.llm_slot().is_some());

    // Remove: key deleted, consents dropped, tools hidden.
    let st = e.rpc("providers.remove", json!({})).await.unwrap();
    assert_eq!(st["status"]["provider"], serde_json::Value::Null);
    assert!(
        secrets
            .get(&sr_providers::secrets::provider_account(&id))
            .unwrap()
            .is_none()
    );
    assert!(gate.check(&id, Purpose::Answer).is_err());
    let f = e.tool_flags();
    assert!(!f.answer);
    e.drain().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enhance_prompt_grounds_the_request_and_falls_back_when_the_provider_does_not_answer() {
    let t = tmp();
    let data = t.path().join("d");
    let dir = t.path().join("repo");
    write_files(
        &dir,
        &[
            (
                "src/scheduler.rs",
                "pub fn run_scheduler() {\n    // reconcile every hour\n}\n",
            ),
            (
                "docs/sync.md",
                "The scheduler reconciles folders every hour.",
            ),
        ],
    );
    let p = Profile::new("answers2");
    let id = provider_id();
    let llm = FakeLlm::failing(ErrorCode::Timeout);
    let mut cfg = config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &p))),
    );
    cfg.secrets = Some(Arc::new(MemorySecretStore::new()));
    cfg.llm = Some(LlmOverride {
        provider_id: id.clone(),
        display_name: "Test provider".into(),
        llm: llm.clone(),
    });
    let e = Engine::start(cfg).await.unwrap();
    e.rpc(
        "providers.save",
        json!({ "provider": id, "key": "sk-test-abcdefgh", "check": false }),
    )
    .await
    .unwrap();
    e.add_folder(&dir.to_string_lossy(), None).await.unwrap();
    e.run_pending_jobs().await.unwrap();
    let out = e
        .enhance_prompt(
            EnhanceArgs {
                prompt: "make the scheduler reconcile more often".into(),
                memories: Some("- prefers small PRs\n- prefers small PRs".into()),
                history: vec![sr_engine::answers::HistoryMessage {
                    role: "user".into(),
                    content: "we talked about sync".into(),
                }],
                ..EnhanceArgs::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    // The provider "didn't respond": the heuristic fallback, grounded in the folder.
    assert!(out.result.enhanced && out.result.fallback && !out.result.used_llm);
    assert_eq!(out.result.fallback_code, Some(ErrorCode::Timeout));
    assert!(
        out.result.prompt.contains("**Relevant Files Found:**"),
        "{}",
        out.result.prompt
    );
    assert!(!out.result.files.is_empty());
    // The one request had the Selene layout, the de-duplicated memories and the file tree.
    let seen = llm.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].purpose, Purpose::Enhance);
    assert_eq!(seen[0].temperature, Some(0.3));
    assert_eq!(seen[0].max_tokens, Some(3000));
    let user = &seen[0].messages[1].content;
    assert!(user.contains(
        "<composer_prompt>\nmake the scheduler reconcile more often\n</composer_prompt>"
    ));
    assert_eq!(user.matches("- prefers small PRs").count(), 1);
    assert!(user.contains("[1] User: we talked about sync"));
    assert!(user.contains("**Synced Folders:**"));
    assert!(user.contains("<retrieved_context"));
    e.drain().await;
}

/// A bag-of-words embedder: cosine follows shared words, on top of a constant share (like real
/// embedders, unrelated text still scores about 0.5, so `minScore` never filters).
struct BowEmbedder {
    inner: Arc<sr_providers::testing::HashEmbedder>,
}

impl BowEmbedder {
    fn vector(text: &str, dims: usize) -> Vec<f32> {
        let mut v = vec![0f32; dims];
        for w in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            let h = w
                .to_lowercase()
                .bytes()
                .fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
                    (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
                });
            v[1 + (h % (dims as u64 - 1)) as usize] += 1.0;
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1.0);
        v[0] = n;
        let n = std::f32::consts::SQRT_2 * n;
        v.iter_mut().for_each(|x| *x /= n);
        v
    }
}

impl sr_providers::EmbeddingProvider for BowEmbedder {
    fn profile(&self) -> &sr_providers::EmbeddingProfile {
        sr_providers::EmbeddingProvider::profile(&*self.inner)
    }
    fn max_batch(&self) -> usize {
        64
    }
    fn embed_batch<'a>(
        &'a self,
        _kind: sr_providers::EmbedKind,
        texts: &'a [&'a str],
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, sr_common::Result<Vec<Vec<f32>>>> {
        let dims = sr_providers::EmbeddingProvider::profile(&*self.inner).dims as usize;
        Box::pin(async move { Ok(texts.iter().map(|t| Self::vector(t, dims)).collect()) })
    }
}

/// docs/08: the owner's broad question as a regression test. The folder mimics the failure: a
/// README that names the topic shallowly, a provider file and two test suites full of the
/// question's words, and the file that implements the index, whose many passages each match
/// less. The plain top 9 (the default first-search share of `maxSources` 12) holds no passage of
/// the core file; the passages the model receives do.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broad_question_reaches_the_core_file_within_the_budget() {
    let t = tmp();
    let data = t.path().join("d");
    let dir = t.path().join("repo");
    let q = "How does vector indexing work?";
    let readme = "# Demo store\n\nHow does vector indexing work? It just works: vector indexing \
                  happens in the background.\n";
    let provider: String = (0..40)
        .map(|i| {
            format!(
                "/// How does the provider work? It turns text into a vector for indexing.\n\
                 pub fn embed_vector_{i}(text: &str) -> Vec<f32> {{ work(text) }}\n"
            )
        })
        .collect();
    let suite = |name: &str| -> String {
        (0..60)
            .map(|i| {
                format!(
                    "#[test]\nfn {name}_{i}() {{\n    // Does vector indexing work? How does it work for case {i}?\n    assert!(vector_indexing_works({i}));\n}}\n"
                )
            })
            .collect()
    };
    let core: String = (0..30)
        .map(|i| {
            format!(
                "/// Segment {i}: writes rows into the column store and keeps the ANN index in step.\n\
                 pub fn apply_segment_{i}(table: &mut Table, batch: &RowBatch) -> Result<u64> {{\n    \
                 let rows = batch.rows_for_partition({i});\n    \
                 table.delete_file_rows(batch.file_id)?;\n    \
                 table.merge_insert(rows, \"chunk_id\")?;\n    \
                 table.optimize_if_fragmented(FRAGMENT_LIMIT)?;\n    \
                 let ann = table.index_handle(\"vector\");\n    \
                 ann.retrain_when_rows_double(table.count_rows()?)?;\n    \
                 Ok(table.version())\n}}\n"
            )
        })
        .collect();
    let billing =
        "pub fn invoice_total(lines: &[Line]) -> Money { lines.iter().map(Line::amount).sum() }\n";
    let suite_a = suite("vector_indexing_case");
    let suite_b = suite("does_vector_search_work");
    write_files(
        &dir,
        &[
            ("README.md", readme),
            ("src/providers/embed.rs", &provider),
            ("tests/indexing.rs", &suite_a),
            ("tests/vector_search.rs", &suite_b),
            ("src/index/store.rs", &core),
            ("src/billing.rs", billing),
        ],
    );
    let p = Profile::new("answers-broad");
    let id = provider_id();
    let llm = FakeLlm::new(&[
        r#"{"answer": "The index is kept by the store [1].", "citations": [{"source": 1}], "enough": true}"#,
    ]);
    let mut cfg = config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &p))),
    );
    cfg.embedder = sr_engine::EmbedderSource::Provided(Arc::new(BowEmbedder {
        inner: hash_embedder(),
    }));
    cfg.secrets = Some(Arc::new(MemorySecretStore::new()));
    cfg.llm = Some(LlmOverride {
        provider_id: id.clone(),
        display_name: "Test provider".into(),
        llm: llm.clone(),
    });
    let e = Engine::start(cfg).await.unwrap();
    e.rpc(
        "providers.save",
        json!({ "provider": id, "key": "sk-test-abcdefgh", "check": false }),
    )
    .await
    .unwrap();
    e.rpc(
        "consent.set",
        json!({ "provider": id, "purpose": "answer", "allowed": true }),
    )
    .await
    .unwrap();
    e.add_folder(&dir.to_string_lossy(), None).await.unwrap();
    e.run_pending_jobs().await.unwrap();

    // Before: the plain top 9 is README, provider and test passages only.
    let plain = e
        .search(sr_engine::SearchArgs {
            query: q.into(),
            max_results: Some(9),
            min_score: None,
            folder_ids: None,
            mode: None,
            collapse_duplicates: None,
        })
        .await
        .unwrap();
    let plain_paths: Vec<&str> = plain.hits.iter().map(|h| h.rel_path.as_str()).collect();
    assert_eq!(plain_paths.len(), 9, "{plain_paths:?}");
    assert!(
        !plain_paths.contains(&"src/index/store.rs"),
        "{plain_paths:?}"
    );

    // After: the answer's passages (at most 9 from the first search) include the core file.
    let out = e
        .answer(
            AnswerArgs {
                question: q.into(),
                ..AnswerArgs::default()
            },
            noop(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let used: Vec<&str> = out.sources.iter().map(|s| s.rel_path.as_str()).collect();
    assert!(used.len() <= 9, "{used:?}");
    assert!(used.contains(&"src/index/store.rs"), "{used:?}");
    let seen = llm.seen.lock().unwrap().clone();
    assert!(seen[0].messages[1].content.contains("store.rs"));
    // The README and the provider are still there; the test suites give way to the code.
    for f in ["README.md", "src/providers/embed.rs"] {
        assert!(used.contains(&f), "{f}: {used:?}");
    }
    e.drain().await;
}

/// Owner report 2026-10-20 (docs/08 §9): "How does Selene Retrieval's vector indexing engine
/// work?" with `maxSources` 20 on the project's own folder got the README, CLAUDE.md, the roadmap
/// and engine-crate files, and no passage of the vector store. The answer now searches without
/// the folder's name, inside the folder the question names, with `index` added for `indexing`.
/// The fixture: a folder named like the product (README, agent notes, a roadmap and plans full of
/// the name, engine jobs about "indexing"), whose vector store, keyword index and query code say
/// "index"; and a second folder ("Documents") with notes that match the question word for word.
/// Some files have Windows line ends: no `\r` reaches the model or the caller.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_question_gets_the_vector_store_and_a_query_path() {
    let t = tmp();
    let data = t.path().join("d");
    let repo = t.path().join("selene-retrieval");
    let notes = t.path().join("Documents");
    let q = "How does Selene Retrieval's vector indexing engine work?";
    let crlf = |s: &str| s.replace('\n', "\r\n");
    let readme = crlf(
        "# Selene Retrieval\n\nSelene Retrieval is a search engine for your files. Selene \
         Retrieval keeps indexing in the background, and Selene Retrieval answers questions.\n",
    );
    let agents = "# Selene Retrieval: agent instructions\n\nSelene Retrieval rules for agents \
                  that work on the Selene Retrieval engine.\n";
    let roadmap: String = (0..6)
        .map(|i| {
            format!(
                "## Milestone {i}\n\nSelene Retrieval milestone {i}: the indexing engine work \
                 is planned. How does the Selene Retrieval engine handle indexing? Planned.\n\n"
            )
        })
        .collect();
    let plans: Vec<(String, String)> = (0..8)
        .map(|i| {
            (
                format!("docs/plan-{i}.md"),
                format!(
                    "# Plan {i}\n\nHow does the Selene Retrieval engine do its indexing work? \
                     Selene Retrieval plan {i} describes it in outline.\n"
                ),
            )
        })
        .collect();
    let jobs: Vec<(String, String)> = (0..4)
        .map(|i| {
            (
                format!("crates/engine/src/job_{i}.rs"),
                format!(
                    "/// The engine runs indexing work in the background (job {i}).\n\
                     pub fn run_indexing_job_{i}(engine: &Engine) {{ engine.work({i}); }}\n"
                ),
            )
        })
        .collect();
    let lance: String = std::iter::once(
        "//! Vector store: one table per vector index. The ANN index (IVF_HNSW_SQ) and the \
         scalar indexes (BTREE, BITMAP) are created on the table; a vector query runs an ANN \
         search or an exact scan.\n"
            .to_string(),
    )
    .chain((0..12).map(|i| {
        format!(
            "/// Vector index step {i}: writes vector rows into the table and keeps the ANN \
             index and the scalar indexes in step.\n\
             pub fn vector_index_step_{i}(table: &mut Table) -> Result<u64> {{\n    \
             table.create_index(\"vector\", Index::IvfHnswSq)?;\n    \
             Ok(table.version())\n}}\n"
        )
    }))
    .collect();
    let lexical: String = (0..6)
        .map(|i| {
            format!(
                "/// Keyword index search {i}: BM25 over the keyword index, the lane next to the \
                 vector index.\npub fn search_keyword_index_{i}(q: &str) -> Vec<Hit> {{ bm25(q) }}\n"
            )
        })
        .collect();
    let query: String = (0..6)
        .map(|i| {
            format!(
                "/// Query path {i}: search the vector index and the keyword index, then fuse \
                 both lists by rank.\npub fn query_index_{i}(q: &str) -> Vec<Hit> {{ fuse(q) }}\n"
            )
        })
        .collect();
    let lance = crlf(&lance);
    let mut files: Vec<(&str, &str)> = vec![
        ("README.md", &readme),
        ("CLAUDE.md", agents),
        ("docs/04-roadmap.md", &roadmap),
        ("crates/core/src/index/lance.rs", &lance),
        ("crates/core/src/index/lexical.rs", &lexical),
        ("crates/core/src/query/mod.rs", &query),
    ];
    files.extend(plans.iter().map(|(p, b)| (p.as_str(), b.as_str())));
    files.extend(jobs.iter().map(|(p, b)| (p.as_str(), b.as_str())));
    write_files(&repo, &files);
    write_files(
        &notes,
        &[(
            "notes/vector-indexing.md",
            "How does vector indexing work? Vector indexing engine notes: how the vector \
             indexing engine works, and how vector indexing work is done.\n",
        )],
    );
    let p = Profile::new("answers-owner");
    let id = provider_id();
    let llm = FakeLlm::new(&[
        r#"{"answer": "The vector store keeps the index [1].", "citations": [{"source": 1}], "enough": true}"#,
    ]);
    let mut cfg = config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &p))),
    );
    cfg.embedder = sr_engine::EmbedderSource::Provided(Arc::new(BowEmbedder {
        inner: hash_embedder(),
    }));
    cfg.secrets = Some(Arc::new(MemorySecretStore::new()));
    cfg.llm = Some(LlmOverride {
        provider_id: id.clone(),
        display_name: "Test provider".into(),
        llm: llm.clone(),
    });
    let e = Engine::start(cfg).await.unwrap();
    e.rpc(
        "providers.save",
        json!({ "provider": id, "key": "sk-test-abcdefgh", "check": false }),
    )
    .await
    .unwrap();
    e.rpc(
        "consent.set",
        json!({ "provider": id, "purpose": "answer", "allowed": true }),
    )
    .await
    .unwrap();
    let repo_id = e
        .add_folder(&repo.to_string_lossy(), None)
        .await
        .unwrap()
        .folder_id;
    e.add_folder(&notes.to_string_lossy(), None).await.unwrap();
    e.run_pending_jobs().await.unwrap();

    // The plain search over both folders finds the other folder's notes.
    let plain = e
        .search(sr_engine::SearchArgs {
            query: q.into(),
            max_results: Some(15),
            min_score: None,
            folder_ids: None,
            mode: None,
            collapse_duplicates: None,
        })
        .await
        .unwrap();
    assert!(
        plain
            .hits
            .iter()
            .any(|h| h.rel_path == "notes/vector-indexing.md"),
        "{:?}",
        plain.hits.iter().map(|h| &h.rel_path).collect::<Vec<_>>()
    );

    // maxSources 20: the first search sends at most 15 passages.
    let out = e
        .answer(
            AnswerArgs {
                question: q.into(),
                max_sources: Some(20),
                ..AnswerArgs::default()
            },
            noop(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let used: Vec<&str> = out.sources.iter().map(|s| s.rel_path.as_str()).collect();
    assert!(!used.is_empty() && used.len() <= 15, "{used:?}");
    assert!(used.contains(&"crates/core/src/index/lance.rs"), "{used:?}");
    assert!(
        used.iter().any(|p| [
            "crates/core/src/index/lexical.rs",
            "crates/core/src/query/mod.rs"
        ]
        .contains(p)),
        "{used:?}"
    );
    // The question names the project folder: the other folder's notes stay out.
    assert!(
        out.sources.iter().all(|s| s.folder_id == Some(repo_id)),
        "{used:?}"
    );
    // No carriage return reaches the model or the caller; line numbers are the file's own.
    let seen = llm.seen.lock().unwrap().clone();
    assert!(seen[0].messages[1].content.contains("lance.rs"));
    assert!(!seen[0].messages[1].content.contains('\r'));
    for s in &out.sources {
        assert!(!s.snippet.contains('\r'), "{}", s.rel_path);
    }
    let src = out
        .sources
        .iter()
        .find(|s| s.rel_path == "crates/core/src/index/lance.rs")
        .unwrap();
    let file_lines: Vec<&str> = lance.split('\n').collect();
    let first = src.snippet.lines().next().unwrap().trim_end();
    assert!(
        file_lines[src.line_start as usize - 1]
            .trim_end_matches('\r')
            .ends_with(first),
        "line {}: {first:?}",
        src.line_start
    );
    assert!(src.snippet.lines().count() <= (src.line_end - src.line_start + 1) as usize);
    e.drain().await;
}
