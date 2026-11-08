//! Daemon pipe protocol: the first frame every client sends (docs/02-architecture.md §2.2).
//!
//! `kind=mcp`: after the handshake the rest of the stream is raw MCP JSON-RPC (the bridge is a
//! byte proxy). `kind=app`: length-delimited JSON-RPC 2.0 app-RPC frames. M0 skeleton only.

/// Extract-worker protocol (sr-extract and its supervisor). Owner: M1-B.
pub mod extract;

/// Daemon pipe protocol: handshake, version-skew rules and the app-RPC envelope. Owner: M1-C.
/// Re-exported here so `sr_common::protocol::{Handshake, ConnKind, PROTO_VERSION, ..}` stay put.
pub mod daemon;
pub use daemon::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_n_minus_1_and_n_only() {
        assert!(accepts(5, 5));
        assert!(accepts(5, 4));
        assert!(!accepts(5, 3));
        assert!(!accepts(5, 6));
        assert!(!accepts(0, u16::MAX), "no overflow");
        assert!(accepts(u16::MAX, u16::MAX - 1));
        assert!(accepts(
            PROTO_VERSION,
            Handshake::new(ConnKind::Mcp, "claude").proto
        ));
    }
}
