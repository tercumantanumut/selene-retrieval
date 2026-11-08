//! Remote provider contract tests (M3-P; M3 exit criteria): one contract per enabled provider of
//! Selene's registry, against **recorded fixtures** served by a local fake HTTP server
//! (`tests/fixtures/providers/<id>.json`). No real key, no provider, no internet.
//!
//! * Contract per provider: key check (+ key stored only after it passed), model discovery
//!   (live list or registry hints with no request), default model, chat, streaming, embeddings,
//!   the auth header form, and the plain-language error mapping of each recorded error body
//!   (wrong key, quota, rate limit, unavailable).
//! * **No egress without consent:** with no consent recorded, every entry point refuses with
//!   `PrivacyBlocked` and the server never sees a connection.
//! * **Cancel stops billing:** cancelling (or dropping) a pending chat or a running stream closes
//!   the HTTP connection at once (observed by the server); timeouts do the same.
//! * Offline / blocked / authenticated proxy: mapped to `Offline`, `NetworkBlocked`,
//!   `ProviderUnavailable` (local server not running); proxy credentials are sent to the proxy.
#![cfg(feature = "remote")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use sr_common::error::{ErrorCode, Purpose};
use sr_providers::registry::{AuthKind, Protocol, ProviderSpec, Validation, manifest};
use sr_providers::remote::{ProviderHandle, RemoteEmbedder, RemoteSettings, check_key, errors};
use sr_providers::secrets::provider_account;
use sr_providers::testing::fake_http::{FakeHttp, Recorded, Reply};
use sr_providers::{
    ApiKey, ChatMessage, ChatRequest, EgressCtx, EmbeddingProvider, LlmProvider, MemoryGate,
    MemorySecretStore, PrivacyGate, Resilient, Role, SecretStore,
};
use tokio_util::sync::CancellationToken;

const KEY: &str = "sk-test-contract-0000-1234";

// ------------------------------------------------------------------------------------ helpers

fn fixture(id: &str) -> Value {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/providers")
        .join(format!("{id}.json"));
    let s = std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "every enabled provider needs a fixture: {}: {e}",
            p.display()
        )
    });
    serde_json::from_str(&s).unwrap()
}

/// Enabled providers that talk HTTP from here (the CLI sign-in providers run their own program;
/// `tests/cli_fake.rs`).
fn enabled() -> Vec<&'static ProviderSpec> {
    manifest()
        .providers()
        .filter(|p| p.enabled && !p.is_cli())
        .collect()
}

fn spec(id: &str) -> &'static ProviderSpec {
    manifest().get(id).unwrap()
}

fn reply(v: &Value) -> Reply {
    let headers = v
        .get("headers")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default();
    Reply::Json {
        status: v["status"].as_u64().unwrap() as u16,
        headers,
        body: v["body"].clone(),
    }
}

fn sse(v: &Value, delay: Duration) -> Reply {
    Reply::Sse {
        blocks: v
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_str().unwrap().to_string())
            .collect(),
        delay,
    }
}

fn chat_suffix(s: &ProviderSpec) -> &'static str {
    if s.protocol == Some(Protocol::Anthropic) {
        "/messages"
    } else {
        "/chat/completions"
    }
}

/// (method, path suffix) of the key check.
fn validate_route(s: &ProviderSpec) -> (&'static str, &'static str) {
    match s.validation {
        Some(Validation::Models) => ("GET", "/models"),
        Some(Validation::OpenrouterKey) => ("GET", "/key"),
        Some(Validation::ChatProbe) => ("POST", chat_suffix(s)),
        Some(Validation::CliStatus) | None => panic!("enabled provider {} has no validation", s.id),
    }
}

fn settings(fake: &FakeHttp, timeout: Duration) -> RemoteSettings {
    RemoteSettings {
        base_url_override: Some(format!("{}/v1", fake.base())),
        attempt_timeout: timeout,
        ..RemoteSettings::default()
    }
}

fn key_for(s: &ProviderSpec) -> ApiKey {
    if s.needs_key() {
        ApiKey::new(KEY)
    } else {
        ApiKey::default()
    }
}

fn granted(id: &str) -> Arc<MemoryGate> {
    let g = Arc::new(MemoryGate::new());
    g.grant(id, Purpose::ALL);
    g
}

async fn connect(
    fake: &FakeHttp,
    s: &ProviderSpec,
    gate: Arc<MemoryGate>,
    timeout: Duration,
) -> ProviderHandle {
    ProviderHandle::connect(s, key_for(s), settings(fake, timeout), gate, None)
        .await
        .unwrap()
}

fn req(model: &str, purpose: Purpose) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: vec![
            ChatMessage {
                role: Role::System,
                content: "Answer from the user's files.".into(),
            },
            ChatMessage {
                role: Role::User,
                content: "Where is the answer?".into(),
            },
        ],
        max_tokens: Some(64),
        temperature: None,
        purpose,
        egress: EgressCtx {
            collection_id: Some("01J0000000000000000000TEST".into()),
            client_id: Some("claude".into()),
        },
    }
}

fn assert_headers(s: &ProviderSpec, r: &Recorded, raw_get: bool) {
    let ua = r.header("user-agent").unwrap_or_default();
    assert!(
        ua.starts_with("SeleneRetrieval/"),
        "{}: user-agent {ua:?}",
        s.id
    );
    for (k, v) in &r.headers {
        // Never a user identifier (e-mail) in any header.
        assert!(
            !v.contains('@'),
            "{}: header {k} carries an identifier",
            s.id
        );
        assert_ne!(k, "from", "{}: From header", s.id);
    }
    match s.protocol {
        Some(Protocol::Anthropic) => {
            assert_eq!(r.header("x-api-key"), Some(KEY), "{}", s.id);
            assert!(r.header("anthropic-version").is_some(), "{}", s.id);
            assert!(r.header("authorization").is_none(), "{}", s.id);
        }
        _ if s.needs_key() => {
            assert_eq!(
                r.header("authorization"),
                Some(format!("Bearer {KEY}").as_str()),
                "{}",
                s.id
            );
        }
        _ if raw_get => assert!(
            r.header("authorization").is_none(),
            "{}: no key, no header",
            s.id
        ),
        _ => {}
    }
}

fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

// --------------------------------------------------------------------------- contract tests

#[tokio::test]
async fn every_enabled_provider_has_a_fixture_and_oauth_stays_disabled() {
    // Amendments 10.2 / 10.3: "Claude (sign in)" / "Codex (sign in)" are enabled but run the
    // user's own CLI; over HTTP they refuse to connect (even with a "key") and make no request.
    for id in ["claudecode", "codex"] {
        let c = manifest().get(id).expect("CLI sign-in row");
        assert!(c.enabled && c.auth_kind == Some(AuthKind::Oauth) && c.is_cli());
        let fake = FakeHttp::start().await.unwrap();
        let e = ProviderHandle::connect(
            c,
            ApiKey::new(KEY),
            settings(&fake, Duration::from_secs(5)),
            granted(id),
            None,
        )
        .await
        .err()
        .map(|e| e.code);
        assert_eq!(e, Some(ErrorCode::ProviderNotConfigured), "{id}");
        assert_eq!(fake.connections(), 0);
    }
    assert!(enabled().len() >= 3);
    for s in enabled() {
        let fx = fixture(&s.id);
        assert_eq!(fx["provider"].as_str(), Some(s.id.as_str()));
        assert!(
            fx["errors"].as_object().is_some_and(|e| !e.is_empty()),
            "{}",
            s.id
        );
    }
    for id in ["antigravity"] {
        if let Some(p) = manifest().get(id) {
            assert!(!p.enabled, "{id} stays disabled (vendor, M5-S)");
            let fake = FakeHttp::start().await.unwrap();
            let e = ProviderHandle::connect(
                p,
                ApiKey::new(KEY),
                settings(&fake, Duration::from_secs(5)),
                granted(id),
                None,
            )
            .await
            .err()
            .map(|e| e.code);
            assert_eq!(e, Some(ErrorCode::ProviderNotConfigured));
            assert_eq!(fake.connections(), 0);
        }
    }
}

#[tokio::test]
async fn contract_per_provider() {
    for s in enabled() {
        let fx = fixture(&s.id);
        let fake = FakeHttp::start().await.unwrap();
        let (vm, vs) = validate_route(s);
        let cs = chat_suffix(s);
        let live = s.discovery == sr_providers::registry::Discovery::Live;
        fake.route_times(vm, vs, reply(&fx["validate"]), 1);
        if live {
            fake.route_times("GET", "/models", reply(&fx["models"]), 1);
        }
        fake.route_times("POST", cs, reply(&fx["chat"]), 1);
        fake.route_times("POST", cs, sse(&fx["stream"], Duration::from_millis(5)), 1);
        if let Some(e) = fx.get("embeddings") {
            fake.route_times("POST", "/embeddings", reply(e), 1);
        }
        let gate = granted(&s.id);
        let secrets = MemorySecretStore::new();
        let h = connect(&fake, s, gate.clone(), Duration::from_secs(10)).await;
        let cancel = CancellationToken::new();

        // [Check and save]: validated, stored, models listed, default picked.
        let kc = check_key(&h, &secrets, &cancel)
            .await
            .unwrap_or_else(|e| panic!("{}: check_key: {e}", s.id));
        let stored = secrets.get(&provider_account(&s.id)).unwrap();
        assert_eq!(stored.as_deref(), s.needs_key().then_some(KEY), "{}", s.id);
        assert!(
            kc.default_model.is_some(),
            "{}: a default model is picked",
            s.id
        );
        if live {
            let ids: Vec<&str> = kc.models.iter().map(|m| m.id.as_str()).collect();
            let want: Vec<&str> = fx["models"]["expect_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(ids, want, "{}", s.id);
            assert!(!kc.models_are_hints);
        } else {
            assert!(kc.models_are_hints, "{}: registry hints", s.id);
            assert_eq!(kc.default_model, s.default_model, "{}", s.id);
        }
        if s.needs_key() {
            assert_eq!(kc.key_hint.as_deref(), Some("1234"));
        }

        // Chat.
        let model = fx["chat_model"].as_str().unwrap();
        let llm = h.llm();
        let r = llm
            .chat(&req(model, Purpose::Answer), &cancel)
            .await
            .unwrap_or_else(|e| panic!("{}: chat: {e}", s.id));
        assert_eq!(r.text, "The answer is in notes.md.", "{}", s.id);
        assert_eq!(
            (r.input_tokens, r.output_tokens),
            (Some(42), Some(7)),
            "{}",
            s.id
        );

        // Streaming.
        let mut st = llm
            .chat_stream(&req(model, Purpose::Answer), &cancel)
            .await
            .unwrap_or_else(|e| panic!("{}: stream: {e}", s.id));
        let mut text = String::new();
        while let Some(d) = st.next().await {
            text.push_str(&d.unwrap_or_else(|e| panic!("{}: stream item: {e}", s.id)));
        }
        assert_eq!(text, "The answer is in notes.md.", "{}", s.id);

        // Embeddings (providers offered for cloud search).
        if let Some(e) = fx.get("embeddings") {
            let emb = h.embedder(e["model"].as_str().unwrap(), 4).unwrap();
            let v = emb
                .embed_documents(&["first", "second"], &cancel)
                .await
                .unwrap();
            assert_eq!(v[0], vec![1.0, 0.0, 0.0, 0.0], "{}: sorted by index", s.id);
            let n: f32 = v[1].iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((n - 1.0).abs() < 1e-5, "{}: normalised", s.id);
            assert!(emb.profile().is_remote && emb.profile().precision == "remote");
        } else {
            assert!(
                h.embedder("x", 4).is_err(),
                "{}: no embeddings offered",
                s.id
            );
        }

        // What went over the wire.
        let reqs = fake.requests();
        for r in &reqs {
            assert_headers(s, r, r.method == "GET");
        }
        let chat_reqs: Vec<&Recorded> = reqs.iter().filter(|r| r.target.ends_with(cs)).collect();
        let body = chat_reqs.last().unwrap().json();
        assert_eq!(
            body["stream"].as_bool(),
            Some(true),
            "{}: last chat call streams",
            s.id
        );
        assert!(body.to_string().contains(model), "{}: model sent", s.id);
        assert!(
            body.to_string().contains("Where is the answer?"),
            "{}",
            s.id
        );
        // Every request was audited, with the caller's context on chat calls.
        let audit = gate.records();
        assert_eq!(
            audit.len(),
            reqs.len(),
            "{}: one audit row per request",
            s.id
        );
        assert!(audit.iter().any(|a| a.purpose == Purpose::ValidateKey));
        let chat_audit: Vec<_> = audit
            .iter()
            .filter(|a| a.purpose == Purpose::Answer)
            .collect();
        assert_eq!(chat_audit.len(), 2, "{}", s.id);
        assert!(chat_audit.iter().all(|a| a.bytes > 0
            && a.provider == s.id
            && a.ctx.client_id.as_deref() == Some("claude")));
    }
}

#[tokio::test]
async fn recorded_errors_map_to_plain_codes() {
    for s in enabled() {
        let fx = fixture(&s.id);
        for (name, e) in fx["errors"].as_object().unwrap() {
            let want = e["expect"].as_str().unwrap();
            let fake = FakeHttp::start().await.unwrap();
            let cs = chat_suffix(s);
            fake.route("POST", cs, reply(e));
            let h = connect(&fake, s, granted(&s.id), Duration::from_secs(10)).await;
            let model = fx["chat_model"].as_str().unwrap();
            let err = h
                .llm()
                .chat(&req(model, Purpose::Answer), &CancellationToken::new())
                .await
                .expect_err(name);
            assert_eq!(err.code.as_str(), want, "{} / {name}: {err}", s.id);
            assert_eq!(err.ctx.provider.as_deref(), Some(s.display_name.as_str()));
            let msg = err.user_message();
            assert!(!msg.title.is_empty() && !msg.body.is_empty());
            if matches!(err.code, ErrorCode::ProviderAuth | ErrorCode::ProviderQuota) {
                assert!(
                    msg.title.contains(&s.display_name),
                    "{}: {}",
                    s.id,
                    msg.title
                );
            }
            if err.code == ErrorCode::RateLimited {
                let secs: u64 = e["headers"]["retry-after"]
                    .as_str()
                    .unwrap()
                    .parse()
                    .unwrap();
                assert_eq!(
                    err.retry_after,
                    Some(Duration::from_secs(secs)),
                    "{} / {name}",
                    s.id
                );
            }
            // The key never reaches the user text or Details.
            let all = format!("{} {} {:?}", msg.title, msg.body, err.ctx.detail);
            assert!(!all.contains(KEY), "{} / {name}", s.id);

            // A rejected key at [Check and save] is not stored.
            if want == "ProviderAuth" {
                let fake2 = FakeHttp::start().await.unwrap();
                let (vm, vs) = validate_route(s);
                fake2.route(vm, vs, reply(e));
                let h2 = connect(&fake2, s, granted(&s.id), Duration::from_secs(10)).await;
                let secrets = MemorySecretStore::new();
                let r = check_key(&h2, &secrets, &CancellationToken::new()).await;
                assert_eq!(
                    r.err().map(|e| e.code),
                    Some(ErrorCode::ProviderAuth),
                    "{}",
                    s.id
                );
                assert!(
                    secrets.accounts().is_empty(),
                    "{}: rejected key not saved",
                    s.id
                );
            }
        }
    }
}

// ------------------------------------------------------------------- consent and egress audit

#[tokio::test]
async fn no_egress_without_consent() {
    for s in enabled() {
        let fake = FakeHttp::start().await.unwrap();
        // Routes that would succeed: only the gate may stop the calls.
        let fx = fixture(&s.id);
        let (vm, vs) = validate_route(s);
        fake.route(vm, vs, reply(&fx["validate"]));
        fake.route("POST", chat_suffix(s), reply(&fx["chat"]));
        if let Some(m) = fx.get("models") {
            fake.route("GET", "/models", reply(m));
        }
        if let Some(e) = fx.get("embeddings") {
            fake.route("POST", "/embeddings", reply(e));
        }
        let gate = Arc::new(MemoryGate::new()); // nothing granted
        let h = connect(&fake, s, gate.clone(), Duration::from_secs(5)).await;
        let c = CancellationToken::new();
        let blocked = |r: sr_common::Result<()>, what: &str| {
            let e = r.expect_err(what);
            assert_eq!(e.code, ErrorCode::PrivacyBlocked, "{} / {what}", s.id);
            assert_eq!(e.ctx.provider.as_deref(), Some(s.display_name.as_str()));
            assert!(e.ctx.purpose.is_some());
        };
        let llm = h.llm();
        blocked(llm.validate_key(&c).await, "validate_key");
        let secrets = MemorySecretStore::new();
        blocked(check_key(&h, &secrets, &c).await.map(|_| ()), "check_key");
        assert!(secrets.accounts().is_empty());
        for p in [Purpose::Answer, Purpose::Enhance, Purpose::Iterate] {
            blocked(llm.chat(&req("m", p), &c).await.map(|_| ()), "chat");
            blocked(
                llm.chat_stream(&req("m", p), &c).await.map(|_| ()),
                "chat_stream",
            );
        }
        let resilient = Resilient::new(h.llm());
        blocked(
            resilient
                .chat(&req("m", Purpose::Answer), &c)
                .await
                .map(|_| ()),
            "resilient chat",
        );
        match s.discovery {
            sr_providers::registry::Discovery::Live => {
                blocked(llm.list_models(&c).await.map(|_| ()), "list_models")
            }
            // Registry hints are local data: allowed, and no request.
            sr_providers::registry::Discovery::Registry => {
                assert!(llm.list_models(&c).await.is_ok())
            }
        }
        if s.supports_embeddings() {
            let emb = h.embedder("m", 4).unwrap();
            blocked(
                emb.embed_documents(&["secret text"], &c).await.map(|_| ()),
                "embed_doc",
            );
            blocked(
                emb.embed_query("secret query", &c).await.map(|_| ()),
                "embed_query",
            );
            blocked(
                RemoteEmbedder::probe_dims(&h, "m", &c).await.map(|_| ()),
                "probe_dims",
            );
        }
        assert_eq!(
            fake.connections(),
            0,
            "{}: no connection without consent",
            s.id
        );
        assert!(fake.requests().is_empty());
        assert!(gate.records().is_empty(), "{}: nothing to audit", s.id);
    }
}

#[tokio::test]
async fn consent_is_per_purpose_and_revocable() {
    let s = spec("openrouter");
    let fx = fixture("openrouter");
    let fake = FakeHttp::start().await.unwrap();
    fake.route("POST", "/chat/completions", reply(&fx["chat"]));
    fake.route("POST", "/embeddings", reply(&fx["embeddings"]));
    let gate = Arc::new(MemoryGate::new());
    gate.grant("openrouter", &[Purpose::Answer]);
    let h = connect(&fake, s, gate.clone(), Duration::from_secs(5)).await;
    let c = CancellationToken::new();
    assert!(
        h.llm()
            .chat(&req("openrouter/auto", Purpose::Answer), &c)
            .await
            .is_ok()
    );
    // Answer consent doesn't cover the text of the user's files.
    let emb = h.embedder("qwen/qwen3-embedding-4b", 4).unwrap();
    let e = emb
        .embed_documents(&["a", "b"], &c)
        .await
        .err()
        .map(|e| e.code);
    assert_eq!(e, Some(ErrorCode::PrivacyBlocked));
    let e = h
        .llm()
        .chat(&req("openrouter/auto", Purpose::Enhance), &c)
        .await
        .err()
        .map(|e| e.code);
    assert_eq!(e, Some(ErrorCode::PrivacyBlocked));
    assert_eq!(fake.requests().len(), 1);
    // Revoked: the next call is refused before sending.
    gate.revoke("openrouter", Purpose::Answer);
    let e = h
        .llm()
        .chat(&req("openrouter/auto", Purpose::Answer), &c)
        .await
        .err()
        .map(|e| e.code);
    assert_eq!(e, Some(ErrorCode::PrivacyBlocked));
    assert_eq!(fake.requests().len(), 1);
    let audit = gate.records();
    assert_eq!(audit.len(), 1);
    assert_eq!(
        audit[0].ctx.collection_id.as_deref(),
        Some("01J0000000000000000000TEST")
    );
    assert_eq!(audit[0].host, "127.0.0.1");
    // The consent groups the UI shows as one switch.
    assert_eq!(
        sr_providers::consent_group(Purpose::Iterate),
        &[Purpose::Answer, Purpose::Iterate]
    );
}

// --------------------------------------------------------------- cancellation stops billing

#[tokio::test]
async fn cancel_aborts_a_pending_chat() {
    let s = spec("deepseek");
    let fake = FakeHttp::start().await.unwrap();
    fake.route("POST", "/chat/completions", Reply::Hang);
    let h = connect(&fake, s, granted("deepseek"), Duration::from_secs(60)).await;
    let cancel = CancellationToken::new();
    let (llm, c2) = (h.llm(), cancel.clone());
    let task = tokio::spawn(async move {
        llm.chat(&req("deepseek-v4-pro", Purpose::Answer), &c2)
            .await
    });
    assert!(
        fake.wait_requests(1, Duration::from_secs(5)).await,
        "request reached the server"
    );
    let t0 = std::time::Instant::now();
    cancel.cancel();
    let r = task.await.unwrap();
    assert_eq!(r.err().map(|e| e.code), Some(ErrorCode::Cancelled));
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert!(
        fake.wait_closed(1, Duration::from_secs(3)).await,
        "the HTTP connection is closed at once (no orphan request)"
    );
}

#[tokio::test]
async fn cancel_and_drop_abort_a_running_stream() {
    let s = spec("anthropic");
    let fx = fixture("anthropic");
    let mut blocks: Vec<Value> = fx["stream"].as_array().unwrap().clone();
    // Make it long: repeat the text deltas.
    let delta = blocks[3].clone();
    for _ in 0..40 {
        blocks.insert(4, delta.clone());
    }
    let slow = Value::Array(blocks);
    let fake = FakeHttp::start().await.unwrap();
    fake.route_times(
        "POST",
        "/messages",
        sse(&slow, Duration::from_millis(150)),
        2,
    );
    let h = connect(&fake, s, granted("anthropic"), Duration::from_secs(30)).await;

    // 1. Cancel through the token.
    let cancel = CancellationToken::new();
    let llm = h.llm();
    let mut st = llm
        .chat_stream(&req("claude-sonnet-4-6", Purpose::Answer), &cancel)
        .await
        .unwrap();
    assert!(st.next().await.unwrap().is_ok(), "first delta");
    cancel.cancel();
    let next = st.next().await.unwrap();
    assert_eq!(next.err().map(|e| e.code), Some(ErrorCode::Cancelled));
    assert!(st.next().await.is_none(), "stream ends after cancel");
    assert!(
        fake.wait_closed(1, Duration::from_secs(3)).await,
        "connection closed on cancel"
    );

    // 2. The caller just drops the stream.
    let cancel = CancellationToken::new();
    let mut st = llm
        .chat_stream(&req("claude-sonnet-4-6", Purpose::Answer), &cancel)
        .await
        .unwrap();
    assert!(st.next().await.unwrap().is_ok());
    drop(st);
    assert!(
        fake.wait_closed(2, Duration::from_secs(3)).await,
        "connection closed on drop"
    );
}

#[tokio::test]
async fn attempt_timeout_aborts_too() {
    let s = spec("kimi");
    let fake = FakeHttp::start().await.unwrap();
    fake.route("POST", "/chat/completions", Reply::Hang);
    let h = connect(&fake, s, granted("kimi"), Duration::from_secs(1)).await;
    let r = h
        .llm()
        .chat(
            &req("kimi-k2-turbo-preview", Purpose::Answer),
            &CancellationToken::new(),
        )
        .await;
    let e = r.err().unwrap();
    assert_eq!(e.code, ErrorCode::Timeout);
    assert!(e.retryable);
    assert!(fake.wait_closed(1, Duration::from_secs(3)).await);
}

#[tokio::test]
async fn resilient_retries_429_and_audits_each_attempt() {
    let s = spec("openrouter");
    let fx = fixture("openrouter");
    let fake = FakeHttp::start().await.unwrap();
    fake.route_times("POST", "/chat/completions", reply(&fx["errors"]["rate"]), 1);
    fake.route("POST", "/chat/completions", reply(&fx["chat"]));
    let gate = granted("openrouter");
    let h = connect(&fake, s, gate.clone(), Duration::from_secs(10)).await;
    let r = h
        .resilient_llm()
        .chat(
            &req("openrouter/auto", Purpose::Answer),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(r.text, "The answer is in notes.md.");
    assert_eq!(fake.requests().len(), 2);
    assert_eq!(gate.records().len(), 2, "each attempt is its own egress");
}

#[tokio::test]
async fn cancel_during_retry_wait_sends_nothing_more() {
    let s = spec("openrouter");
    let fake = FakeHttp::start().await.unwrap();
    let mut rate = fixture("openrouter")["errors"]["rate"].clone();
    rate["headers"]["retry-after"] = Value::from("30");
    fake.route("POST", "/chat/completions", reply(&rate));
    let h = connect(&fake, s, granted("openrouter"), Duration::from_secs(10)).await;
    let cancel = CancellationToken::new();
    let (r, c2) = (h.resilient_llm(), cancel.clone());
    let task =
        tokio::spawn(async move { r.chat(&req("openrouter/auto", Purpose::Answer), &c2).await });
    assert!(fake.wait_requests(1, Duration::from_secs(5)).await);
    cancel.cancel();
    assert_eq!(
        task.await.unwrap().err().map(|e| e.code),
        Some(ErrorCode::Cancelled)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.requests().len(), 1);
}

// ------------------------------------------------------------ offline, blocked, proxy, local

#[tokio::test]
async fn local_server_not_running_is_provider_unavailable() {
    let s = spec("ollama");
    let port = closed_port();
    let st = RemoteSettings {
        base_url_override: Some(format!("http://127.0.0.1:{port}")),
        attempt_timeout: Duration::from_secs(5),
        ..RemoteSettings::default()
    };
    let h = ProviderHandle::connect(s, ApiKey::default(), st, granted("ollama"), None)
        .await
        .unwrap();
    let e = h
        .llm()
        .validate_key(&CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(e.code, ErrorCode::ProviderUnavailable);
    assert!(e.user_message().title.contains("Ollama"));
}

#[tokio::test]
async fn no_connection_to_a_remote_host_is_offline() {
    // A real reqwest connection failure, classified as for a remote provider host.
    sr_providers::remote::http::ensure_crypto_provider();
    let port = closed_port();
    let err = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .unwrap_err();
    let site = errors::Site {
        provider: "DeepSeek",
        host: "api.deepseek.com",
        key: None,
        local: false,
    };
    let e = errors::from_reqwest(&site, &err);
    assert_eq!(e.code, ErrorCode::Offline);
    assert!(e.retryable);
    let msg = e.user_message();
    assert!(msg.body.contains("api.deepseek.com"), "{}", msg.body);
}

#[tokio::test]
async fn html_sign_in_page_is_network_blocked() {
    let s = spec("deepseek");
    let fake = FakeHttp::start().await.unwrap();
    fake.route(
        "GET",
        "/models",
        Reply::Raw {
            status: 200,
            headers: Vec::new(),
            content_type: "text/html".into(),
            body: "<html><body>Sign in to the guest network</body></html>".into(),
        },
    );
    let h = connect(&fake, s, granted("deepseek"), Duration::from_secs(5)).await;
    let e = h
        .llm()
        .validate_key(&CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(e.code, ErrorCode::NetworkBlocked);
}

#[tokio::test]
async fn authenticated_proxy_gets_the_credentials_and_407_is_network_blocked() {
    // The real OpenRouter address, but every byte goes to our fake proxy: no internet.
    let s = spec("openrouter");
    let proxy = FakeHttp::start().await.unwrap();
    proxy.route(
        "CONNECT",
        "openrouter.ai:443",
        Reply::Raw {
            status: 407,
            headers: vec![("proxy-authenticate".into(), "Basic realm=\"corp\"".into())],
            content_type: "text/plain".into(),
            body: String::new(),
        },
    );
    let st = RemoteSettings {
        proxy_override: Some(proxy.base()),
        attempt_timeout: Duration::from_secs(5),
        ..RemoteSettings::default()
    };
    let creds = Some(("alice".to_string(), "s3cret".to_string()));
    let h = ProviderHandle::connect(s, ApiKey::new(KEY), st, granted("openrouter"), creds)
        .await
        .unwrap();
    let e = h
        .llm()
        .validate_key(&CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(e.code, ErrorCode::NetworkBlocked, "{e}");
    let reqs = proxy.requests();
    assert_eq!(reqs.len(), 1, "only the CONNECT, nothing direct");
    assert_eq!(reqs[0].method, "CONNECT");
    assert_eq!(
        reqs[0].header("proxy-authorization"),
        Some("Basic YWxpY2U6czNjcmV0")
    );
    // The API key never goes to the proxy in clear.
    assert!(reqs[0].headers.iter().all(|(_, v)| !v.contains(KEY)));
    assert!(h.proxy_diagnostics().iter().all(|l| !l.contains("s3cret")));
}

#[tokio::test]
async fn missing_key_fails_before_any_request() {
    let s = spec("deepseek");
    let fake = FakeHttp::start().await.unwrap();
    let e = ProviderHandle::connect(
        s,
        ApiKey::default(),
        settings(&fake, Duration::from_secs(5)),
        granted("deepseek"),
        None,
    )
    .await
    .err()
    .map(|e| e.code);
    assert_eq!(e, Some(ErrorCode::ProviderAuth));
    assert_eq!(fake.connections(), 0);
}

#[tokio::test]
async fn store_backed_connect_reads_key_and_proxy_credentials() {
    let s = spec("deepseek");
    let fx = fixture("deepseek");
    let fake = FakeHttp::start().await.unwrap();
    fake.route("GET", "/models", reply(&fx["validate"]));
    let secrets = MemorySecretStore::new();
    secrets.set(&provider_account("deepseek"), KEY).unwrap();
    secrets
        .set(sr_providers::secrets::PROXY_ACCOUNT, "alice:s3cret")
        .unwrap();
    let h = ProviderHandle::connect_with_store(
        s,
        &secrets,
        settings(&fake, Duration::from_secs(5)),
        granted("deepseek"),
    )
    .await
    .unwrap();
    assert!(
        h.llm()
            .validate_key(&CancellationToken::new())
            .await
            .is_ok()
    );
    assert_eq!(
        fake.requests()[0].header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    // Loopback stays direct even with proxy credentials configured.
    assert_eq!(fake.requests()[0].method, "GET");
    assert!(format!("{h:?}").contains("redacted") && !format!("{h:?}").contains(KEY));
}

#[tokio::test]
async fn probe_dims_reads_the_vector_length() {
    let s = spec("ollama");
    let fake = FakeHttp::start().await.unwrap();
    fake.route(
        "POST",
        "/embeddings",
        // Recorded shape (Ollama / vLLM / OpenRouter all send `usage`; genai requires it).
        Reply::json(200, serde_json::json!({"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2,0.3]}],"model":"nomic-embed-text","usage":{"prompt_tokens":1,"total_tokens":1}})),
    );
    let h = connect(&fake, s, granted("ollama"), Duration::from_secs(5)).await;
    let d = RemoteEmbedder::probe_dims(&h, "nomic-embed-text", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(d, 3);
    // A wrong-dims answer for a 4-dim profile is refused, never stored.
    let emb = h.embedder("nomic-embed-text", 4).unwrap();
    let e = emb
        .embed_query("q", &CancellationToken::new())
        .await
        .err()
        .map(|e| e.code);
    assert_eq!(e, Some(ErrorCode::ModelIncompatible));
}

#[tokio::test]
async fn model_ids_are_sent_verbatim() {
    // genai would read `-max` / `-low` / … as a reasoning-effort suffix and strip it.
    let s = spec("openrouter");
    let fx = fixture("openrouter");
    let fake = FakeHttp::start().await.unwrap();
    fake.route("POST", "/chat/completions", reply(&fx["chat"]));
    let h = connect(&fake, s, granted("openrouter"), Duration::from_secs(5)).await;
    let c = CancellationToken::new();
    for m in [
        "qwen/qwen3-max",
        "openai/gpt-5-mini-low",
        "deepseek/deepseek-r1-zero",
    ] {
        h.llm().chat(&req(m, Purpose::Answer), &c).await.unwrap();
        let body = fake.requests().last().unwrap().json();
        assert_eq!(body["model"].as_str(), Some(m));
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }
}

#[test]
fn gate_trait_objects_compose() {
    let g: Arc<dyn PrivacyGate> = Arc::new(MemoryGate::new());
    assert!(g.check("x", Purpose::Answer).is_err());
}
