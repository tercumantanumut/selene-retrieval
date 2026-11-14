//! M4-E: the opt-in HTTP listener end to end. In-process engine (test embedder, no extract
//! worker), the real hyper + rmcp listener on 127.0.0.1, raw HTTP/1.1 requests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sr_engine::{EmbedderSource, Engine, EngineConfig, EngineHandle, ExtractSetup};
use sr_mcp_server::slot::EngineSlot;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config(data: &Path) -> EngineConfig {
    let mut c = EngineConfig::with_embedder(
        data.to_path_buf(),
        data.join("app"),
        EmbedderSource::Provided(Arc::new(sr_providers::testing::HashEmbedder::new(
            "m4e-http", 384,
        ))),
    );
    c.extract = ExtractSetup::Disabled;
    c.background = false;
    c.tokenizer_json = Some(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../core/testdata/bge-small-tokenizer.json"),
    );
    c.secrets = Some(Arc::new(sr_providers::MemorySecretStore::new()));
    c
}

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"m4e-test","version":"1"}}}"#;

fn request(host: &str, auth: Option<&str>, origin: Option<&str>, path: &str) -> String {
    let mut h = String::new();
    if let Some(a) = auth {
        h.push_str(&format!("Authorization: Bearer {a}\r\n"));
    }
    if let Some(o) = origin {
        h.push_str(&format!("Origin: {o}\r\n"));
    }
    format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n{h}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{INIT}",
        INIT.len()
    )
}

/// Status code and whatever body arrived within a few seconds (SSE responses may stay open).
async fn send(port: u16, req: String) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut chunk = [0u8; 4096];
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), s.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) => break,
            Err(_) if !buf.is_empty() && String::from_utf8_lossy(&buf).contains("\r\n\r\n") => {
                // Head and some body are in; an SSE stream may simply stay open.
                if String::from_utf8_lossy(&buf).contains("selene-retrieval") {
                    break;
                }
            }
            Err(_) => {}
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let code = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (code, text)
}

async fn wait_running(e: &EngineHandle, running: bool) -> Option<u16> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let st = e.http_status();
        if st.running == running {
            return st.port;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "HTTP listener did not become running={running}: {:?}",
        e.http_status()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_listener_end_to_end() {
    let t = tempfile::Builder::new().prefix("m4h").tempdir().unwrap();
    let data = t.path().join("d");
    let e = Engine::start(config(&data)).await.unwrap();
    let sup = sr_mcp_server::http::supervise(e.clone(), EngineSlot::ready(e.clone()));
    assert!(!e.http_status().running, "off by default: no port");

    e.http_set_enabled(true).await.unwrap();
    let port = wait_running(&e, true).await.unwrap();
    let tok = e.http_create_token("test tool".into()).await.unwrap();
    let host = format!("127.0.0.1:{port}");

    // No token, wrong token: 401. Wrong Host or a foreign Origin (DNS rebinding): 403.
    assert_eq!(send(port, request(&host, None, None, "/mcp")).await.0, 401);
    assert_eq!(
        send(port, request(&host, Some("nope"), None, "/mcp"))
            .await
            .0,
        401
    );
    assert_eq!(
        send(
            port,
            request(
                &format!("evil.example:{port}"),
                Some(&tok.token),
                None,
                "/mcp"
            )
        )
        .await
        .0,
        403
    );
    assert_eq!(
        send(
            port,
            request(
                &host,
                Some(&tok.token),
                Some("https://evil.example"),
                "/mcp"
            )
        )
        .await
        .0,
        403
    );
    assert_eq!(
        send(port, request(&host, Some(&tok.token), None, "/other"))
            .await
            .0,
        404
    );
    // The real thing: MCP initialize answers as Selene Retrieval.
    let (code, body) = send(port, request(&host, Some(&tok.token), None, "/mcp")).await;
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("selene-retrieval"), "{body}");
    // localhost works too.
    let (code, _) = send(
        port,
        request(&format!("localhost:{port}"), Some(&tok.token), None, "/mcp"),
    )
    .await;
    assert_eq!(code, 200);

    // Revoke: the token stops at once (the listener restarts, ending sessions).
    let id = tok.client.client_id.clone();
    e.http_revoke(id).await.unwrap();
    let port2 = wait_running(&e, true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let port2 = e.http_status().port.unwrap_or(port2);
    assert_eq!(
        send(
            port2,
            request(
                &format!("127.0.0.1:{port2}"),
                Some(&tok.token),
                None,
                "/mcp"
            )
        )
        .await
        .0,
        401
    );

    // Off: the port closes.
    e.http_set_enabled(false).await.unwrap();
    wait_running(&e, false).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port2))
            .await
            .is_err(),
        "nothing listens once HTTP is off"
    );
    sup.abort();
    e.drain().await;
}
