//! M5-C: "Claude (sign in)" / "Codex (sign in)" against FAKE `claude.cmd` / `codex.cmd` scripts in
//! a temp folder (the npm shim shape). The real CLIs are never started and no prompt is sent
//! anywhere. The fakes record their arguments and stdin, print canned status / event lines, and
//! one of them starts a grandchild to prove that cancel kills the whole process tree.
#![cfg(windows)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use sr_common::api::CliSigninState;
use sr_common::error::{ErrorCode, Purpose};
use sr_providers::cli::{CliKind, CliLlm, CliPath, check_status, discover};
use sr_providers::registry::manifest;
use sr_providers::remote::RemoteSettings;
use sr_providers::{ChatMessage, ChatRequest, LlmProvider, MemoryGate, Role};
use tokio_util::sync::CancellationToken;

const FAKE_CLAUDE: &str = r#"@echo off
setlocal
set "D=%~dp0"
if "%~1"=="--version" (
  echo 2.1.999 ^(Claude Code^)
  exit /b 0
)
if "%~1"=="auth" (
  type "%D%status.json"
  exit /b 0
)
>"%D%args.txt" echo(%*
findstr "^" > "%D%stdin.txt"
if exist "%D%slow.flag" (
  start "" /b "%D%slow.cmd"
  ping -n 30 127.0.0.1 >nul
)
type "%D%stream.jsonl"
if exist "%D%fail.flag" exit /b 1
exit /b 0
"#;

const FAKE_CODEX: &str = r#"@echo off
setlocal
set "D=%~dp0"
if "%~1"=="--version" (
  echo codex-cli 0.999.0
  exit /b 0
)
if "%~1"=="login" (
  type "%D%login.txt" 1>&2
  exit /b 0
)
>>"%D%args.txt" echo(%*
echo(%* | findstr /c:"features list" >nul && (
  type "%D%features.txt"
  exit /b 0
)
findstr "^" > "%D%stdin.txt"
type "%D%exec.jsonl"
exit /b 0
"#;

/// A grandchild that writes `late.txt` after ~3 s unless it is killed first.
const SLOW: &str = "@echo off\r\nping -n 4 127.0.0.1 >nul\r\necho late>\"%~dp0late.txt\"\r\n";

struct Fake {
    _dir: tempfile::TempDir,
    root: PathBuf,
    exe: CliPath,
}

impl Fake {
    fn new(kind: CliKind) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (name, body) = match kind {
            CliKind::Claude => ("claude.cmd", FAKE_CLAUDE),
            CliKind::Codex => ("codex.cmd", FAKE_CODEX),
        };
        std::fs::write(root.join(name), body.replace('\n', "\r\n")).unwrap();
        std::fs::write(root.join("slow.cmd"), SLOW).unwrap();
        let exe = discover::accept(&root.join(name)).expect("absolute .cmd");
        Self {
            _dir: dir,
            root,
            exe,
        }
    }
    fn put(&self, name: &str, text: &str) {
        std::fs::write(self.root.join(name), text).unwrap();
    }
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(name)).unwrap_or_default()
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

fn llm(kind: CliKind, exe: &CliPath, gate: Arc<MemoryGate>) -> CliLlm {
    let spec = manifest().require(kind.provider().id).unwrap().clone();
    CliLlm::new(kind, exe.clone(), spec, gate, &RemoteSettings::default())
}

fn granted(kind: CliKind) -> Arc<MemoryGate> {
    let g = Arc::new(MemoryGate::new());
    g.grant(kind.provider().id, &[Purpose::Answer, Purpose::Iterate]);
    g
}

fn req(text: &str) -> ChatRequest {
    ChatRequest {
        model: String::new(),
        messages: vec![
            ChatMessage {
                role: Role::System,
                content: "Answer from the sources.".into(),
            },
            ChatMessage {
                role: Role::User,
                content: text.into(),
            },
        ],
        max_tokens: None,
        temperature: None,
        purpose: Purpose::Answer,
        egress: Default::default(),
    }
}

const CLAUDE_OK: &str = concat!(
    r#"{"type":"system","subtype":"init","tools":[],"mcp_servers":[]}"#,
    "\n",
    r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello "}}}"#,
    "\n",
    r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}}"#,
    "\n",
    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello world"}]}}"#,
    "\n",
    r#"{"type":"result","subtype":"success","is_error":false,"result":"Hello world","usage":{"input_tokens":20,"output_tokens":2}}"#,
    "\n"
);

#[tokio::test]
async fn claude_status_comes_from_auth_status_json() {
    let f = Fake::new(CliKind::Claude);
    f.put(
        "status.json",
        r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"fake@example.com","subscriptionType":"pro"}"#,
    );
    let s = check_status(CliKind::Claude, Some(&f.exe)).await;
    assert_eq!(s.state, CliSigninState::SignedIn);
    assert!(s.authenticated && s.cli_found);
    assert_eq!(s.email.as_deref(), Some("fake@example.com"));
    assert_eq!(s.plan.as_deref(), Some("Pro"));
    assert_eq!(s.cli_version.as_deref(), Some("2.1.999"));
    f.put("status.json", r#"{"loggedIn":false}"#);
    let s = check_status(CliKind::Claude, Some(&f.exe)).await;
    assert_eq!(s.state, CliSigninState::SignedOut);
    assert!(!s.authenticated && s.email.is_none());
}

#[tokio::test]
async fn claude_chat_runs_with_every_tool_off_and_the_prompt_on_stdin() {
    let f = Fake::new(CliKind::Claude);
    f.put("stream.jsonl", CLAUDE_OK);
    let l = llm(CliKind::Claude, &f.exe, granted(CliKind::Claude));
    let cancel = CancellationToken::new();
    let r = l
        .chat(&req("What is in the report?"), &cancel)
        .await
        .unwrap();
    assert_eq!(
        r.text, "Hello world",
        "deltas only, not repeated by the whole message"
    );
    assert_eq!((r.input_tokens, r.output_tokens), (Some(20), Some(2)));
    let args = f.read("args.txt");
    assert!(args.contains(r#"--tools """#), "{args}");
    assert!(args.contains(r#"--setting-sources """#), "{args}");
    assert!(args.contains("--strict-mcp-config"), "{args}");
    assert!(args.contains("--no-session-persistence"), "{args}");
    assert!(
        args.contains("--model claude-sonnet-5-5"),
        "registry default: {args}"
    );
    assert!(
        !args.contains("--mcp-config") && !args.contains("dangerously"),
        "{args}"
    );
    let stdin = f.read("stdin.txt");
    assert!(stdin.contains("What is in the report?"), "prompt on stdin");
    assert!(stdin.contains("<instructions>"), "{stdin}");
    assert!(
        !args.contains("What is in the report?"),
        "never on the command line"
    );

    // A model picked from the list (Amendment 11) is passed as is.
    let mut q = req("What is in the report?");
    q.model = "claude-opus-5-5".into();
    l.chat(&q, &cancel).await.unwrap();
    let args = f.read("args.txt");
    assert!(args.contains("--model claude-opus-5-5"), "{args}");

    // Streaming yields the deltas.
    let mut s = l.chat_stream(&req("Again"), &cancel).await.unwrap();
    let mut out = String::new();
    while let Some(t) = s.next().await {
        out.push_str(&t.unwrap());
    }
    assert_eq!(out, "Hello world");
}

#[tokio::test]
async fn no_consent_means_nothing_runs() {
    let f = Fake::new(CliKind::Claude);
    f.put("stream.jsonl", CLAUDE_OK);
    let l = llm(CliKind::Claude, &f.exe, Arc::new(MemoryGate::new()));
    let e = l
        .chat(&req("x"), &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::PrivacyBlocked);
    assert!(!f.path("args.txt").exists(), "the CLI was never started");
}

#[tokio::test]
async fn claude_failures_map_to_plain_codes() {
    let f = Fake::new(CliKind::Claude);
    let l = llm(CliKind::Claude, &f.exe, granted(CliKind::Claude));
    let c = CancellationToken::new();
    f.put(
        "stream.jsonl",
        concat!(
            r#"{"type":"assistant","error":"authentication_failed","message":{"content":[{"type":"text","text":"Invalid API key · Please run /login"}]}}"#,
            "\n"
        ),
    );
    assert_eq!(
        l.chat(&req("x"), &c).await.unwrap_err().code,
        ErrorCode::ProviderAuth
    );
    f.put(
        "stream.jsonl",
        concat!(
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Claude AI usage limit reached|4102444800"}"#,
            "\n"
        ),
    );
    let e = l.chat_stream(&req("x"), &c).await.err().unwrap();
    assert_eq!(e.code, ErrorCode::RateLimited);
    // A tool call is never expected: the run is stopped.
    f.put(
        "stream.jsonl",
        concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"dir"}}]}}"#,
            "\n"
        ),
    );
    assert_eq!(
        l.chat(&req("x"), &c).await.unwrap_err().code,
        ErrorCode::ProviderRequest
    );
    // Nothing printed, non-zero exit: "isn't responding" with the stderr detail.
    f.put("stream.jsonl", "");
    f.put("fail.flag", "1");
    assert_eq!(
        l.chat(&req("x"), &c).await.unwrap_err().code,
        ErrorCode::ProviderUnavailable
    );
}

#[tokio::test]
async fn cancel_kills_the_whole_process_tree() {
    let f = Fake::new(CliKind::Claude);
    f.put("stream.jsonl", CLAUDE_OK);
    f.put("slow.flag", "1");
    let l = llm(CliKind::Claude, &f.exe, granted(CliKind::Claude));
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        c2.cancel();
    });
    let t0 = Instant::now();
    let e = l.chat(&req("x"), &cancel).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
    assert!(t0.elapsed() < Duration::from_secs(10), "returned at cancel");
    // The grandchild (started by the fake) would write late.txt after ~3 s.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !f.path("late.txt").exists(),
        "the grandchild survived the cancel"
    );
}

#[tokio::test]
async fn codex_status_and_exec() {
    let f = Fake::new(CliKind::Codex);
    f.put("login.txt", "Logged in using ChatGPT\r\n");
    let s = check_status(CliKind::Codex, Some(&f.exe)).await;
    assert_eq!(s.state, CliSigninState::SignedIn);
    assert_eq!(s.plan.as_deref(), Some("ChatGPT"));
    assert_eq!(s.cli_version.as_deref(), Some("0.999.0"));
    f.put("login.txt", "Not logged in\r\n");
    assert!(
        !check_status(CliKind::Codex, Some(&f.exe))
            .await
            .authenticated
    );

    // The tool lock must read back as off, or nothing runs.
    f.put("features.txt", "shell_tool    stable   true\r\n");
    f.put(
        "exec.jsonl",
        concat!(
            r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"Hi there"}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":2}}"#,
            "\n"
        ),
    );
    let l = llm(CliKind::Codex, &f.exe, granted(CliKind::Codex));
    let c = CancellationToken::new();
    assert_eq!(
        l.chat(&req("x"), &c).await.unwrap_err().code,
        ErrorCode::ProviderRequest
    );
    assert!(!f.path("stdin.txt").exists(), "exec never ran");
    f.put(
        "features.txt",
        "shell_tool    stable   false\r\nunified_exec  stable   true\r\n",
    );
    let r = l.chat(&req("Summarise"), &c).await.unwrap();
    assert_eq!(r.text, "Hi there");
    let args = f.read("args.txt");
    let exec = args.lines().last().unwrap_or_default();
    for want in [
        "exec",
        "--json",
        "--ephemeral",
        "--skip-git-repo-check",
        "--ignore-user-config",
        "--sandbox read-only",
        "features.shell_tool=false",
        "mcp_servers={}",
        "-m gpt-5.5 ",
    ] {
        assert!(exec.contains(want), "{want}: {exec}");
    }
    assert!(!exec.contains("model_reasoning_effort"), "{exec}");
    assert!(
        !exec.contains("dangerously") && !exec.contains("--search"),
        "{exec}"
    );
    assert!(exec.trim_end().ends_with(" -"), "prompt from stdin: {exec}");
    assert!(f.read("stdin.txt").contains("Summarise"));

    // A picked model with an effort (Amendment 11): the model and the effort, split.
    let mut q = req("Summarise");
    q.model = "gpt-6-astra-max".into();
    assert_eq!(l.chat(&q, &c).await.unwrap().text, "Hi there");
    let args = f.read("args.txt");
    let exec = args.lines().last().unwrap_or_default();
    // Windows quotes an argument with `=` on the command line (`-c "model_reasoning_effort=max"`).
    assert!(exec.contains(" -m gpt-6-astra -c "), "{exec}");
    assert!(exec.contains("model_reasoning_effort=max"), "{exec}");

    // A command item stops the run.
    f.put(
        "exec.jsonl",
        concat!(
            r#"{"type":"item.started","item":{"id":"i2","type":"command_execution","command":"type secrets.txt"}}"#,
            "\n"
        ),
    );
    assert_eq!(
        l.chat(&req("x"), &c).await.unwrap_err().code,
        ErrorCode::ProviderRequest
    );
}

#[test]
fn console_sign_in_line_is_quoted_and_fixed() {
    let line = sr_providers::cli::process::console_line(
        Path::new(r"C:\Users\Ana Maria\.local\bin\claude.exe"),
        &["auth", "login"],
    );
    assert_eq!(
        line,
        r#"/d /v:off /c start "" /wait "C:\Users\Ana Maria\.local\bin\claude.exe" auth login"#
    );
}
