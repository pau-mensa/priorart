//! priorart: a self-hostable text store for shared coding-agent experiences.

pub mod analyzer;
pub mod api;
pub mod auth;
pub mod cli;
pub mod config;
pub mod excerpt;
pub mod gather;
pub mod index;
pub mod mcp;
pub mod metrics;
mod ownership;
pub mod policy;
pub mod recipe;
pub mod searchlog;
pub mod service;
pub mod store;

/// The lateweave priorart builds against, for recipes outside this crate.
pub use lateweave;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
