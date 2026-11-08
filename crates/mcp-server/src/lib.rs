//! sr-mcp-server: MCP tools and resources over [`sr_engine::EngineHandle`], MCP over the named pipe
//! (rmcp `transport-async-rw`), and the `sr-daemon` binary (pipe server, hidden `admin` CLI).
//! Opt-in streamable HTTP with per-tool bearer tokens ([`http`], M4; off by default, 127.0.0.1 only).

pub mod admin;
pub mod daemon;
/// Opt-in streamable HTTP for local tools (M4-E).
pub mod http;
pub mod server;
pub mod service;
pub mod slot;
pub mod tools;

pub use server::{McpServer, RESOURCE_FOLDERS, RESOURCE_STATUS, tool_error};
pub use service::DaemonService;
pub use slot::EngineSlot;
pub use tools::TOOLS;

/// Install the one rustls crypto provider for the process (Amendment 3.13). Idempotent.
pub fn install_rustls_provider() -> bool {
    rustls::crypto::ring::default_provider()
        .install_default()
        .is_ok()
        || rustls::crypto::CryptoProvider::get_default().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sr_common::error::{Error, ErrorCode};

    #[test]
    fn tool_error_shape() {
        let e = Error::new(ErrorCode::ProviderAuth).with_provider("OpenAI");
        let r = tool_error(&e);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["isError"], true);
        assert_eq!(v["structuredContent"]["code"], "ProviderAuth");
        let text = v["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("re-enter their OpenAI key"), "{text}");
    }

    /// All 41 codes render through the one renderer: plain title + body + agent action, never the
    /// raw code in the text, and `structuredContent {code, retryable, retry_after}`.
    #[test]
    fn every_error_code_maps_to_a_plain_tool_error() {
        assert_eq!(ErrorCode::ALL.len(), 41);
        for code in ErrorCode::ALL {
            let e = Error::new(*code);
            let m = e.user_message();
            let v = serde_json::to_value(tool_error(&e)).unwrap();
            assert_eq!(v["isError"], true);
            assert_eq!(v["structuredContent"]["code"], code.as_str());
            assert_eq!(v["structuredContent"]["retryable"], e.retryable);
            let text = v["content"][0]["text"].as_str().unwrap_or_default();
            assert!(!m.title.is_empty() && !m.body.is_empty(), "{code}");
            assert!(text.contains(&m.title), "{code}: {text}");
            let camel = code.as_str().chars().skip(1).any(char::is_uppercase);
            if camel {
                assert!(!text.contains(code.as_str()), "raw code in text: {text}");
            }
        }
        let e = Error::new(ErrorCode::PathOutsideSandbox);
        assert!(tool_error(&e).content.iter().any(|c| {
            serde_json::to_string(c)
                .unwrap()
                .contains("outside your folders")
        }));
    }

    #[test]
    fn rustls_provider_installs_once() {
        assert!(install_rustls_provider());
        assert!(install_rustls_provider());
    }
}
