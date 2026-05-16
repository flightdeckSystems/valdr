//! Core server state.
//!
//! Owners (per `harness/type-vocabulary.tsv`):
//!   - `Client`          — `src/client.rs`
//!   - `CommandContext`  — `src/command_context.rs`
//!   - `RedisServer`     — `src/server.rs`     (TODO: Phase 2-3 architect packet)
//!   - `RedisDb`         — `src/db.rs`         (TODO: Phase 3 architect packet)
//!   - `RedisObject`     — `src/object.rs`     (TODO: Phase 3 architect packet)
//!
//! Phases 2-3 of the pilot land here.

pub mod client;
pub mod command_context;

pub use client::{Client, ClientId};
pub use command_context::CommandContext;

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        (none — scaffolding placeholder)
//   target_crate:  redis-core
//   confidence:    skeleton
//   todos:         5
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         scaffolding; awaiting first translation packet
// ──────────────────────────────────────────────────────────────────────────
