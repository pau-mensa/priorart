//! priorart: a self-hostable text store for shared coding-agent experiences.

pub mod analyzer;
pub mod config;
pub mod encoder;
pub mod excerpt;
pub mod gather;
pub mod index;
pub mod store;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
