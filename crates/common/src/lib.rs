//! sr-common: the **one** config schema, **one** error model and **one** event set shared by
//! every Selene Retrieval crate, plus ids, the daemon protocol and the path sandbox.
//!
//! Deliberately small: no tokio, no storage, no network dependencies (docs/02-architecture.md §2.1).

/// App-RPC data shapes added in M4 (docs/research/m4-rpc-contract.md).
pub mod api;
pub mod config;
pub mod error;
pub mod events;
pub mod ids;
pub mod protocol;
pub mod sandbox;
pub mod tier;

pub use error::{Error, ErrorCode, Result, SuggestedAction, UserMessage};
