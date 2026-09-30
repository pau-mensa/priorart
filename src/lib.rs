//! priorart: a self-hostable text store for shared coding-agent experiences.

pub mod analyzer;
pub mod api;
pub mod auth;
pub mod config;
pub mod excerpt;
pub mod gather;
pub mod index;
pub mod mcp;
pub mod metrics;
mod ownership;
pub mod policy;
pub mod searchlog;
pub mod service;
pub mod store;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
