//! Repositories. Every function takes a `&Connection`: the writer's (inside
//! [`crate::Catalog::write`]) or a read-pool snapshot (inside [`crate::Catalog::read`]).
//! Multi-statement operations are atomic on their own (SAVEPOINT) and also compose inside a
//! caller's larger transaction.

pub mod activity;
pub mod approvals;
pub mod chunks;
pub mod clients;
pub mod files;
pub mod folders;
pub mod groups;
pub mod jobs;
pub mod meta;
pub mod outbox;
pub mod privacy;
pub mod profiles;
pub mod status;
