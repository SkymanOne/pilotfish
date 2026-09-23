//! parl — a fleet of headless `pi` coding agents orchestrated by Claude Code.
//!
//! The binary and the library share this crate: `main.rs` is a thin clap
//! dispatcher, everything else lives here so integration tests can exercise it.
//! The TypeScript implementation this rewrite replaces remains in `src/*.ts`
//! until the cutover step removes it.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod cli;
pub mod fleet;
pub mod git;
pub mod mcp;
pub mod ops;
pub mod orch;
pub mod paths;
pub mod route;
pub mod secrets;
pub mod tui;
pub mod util;
pub mod worker;
