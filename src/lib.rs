//! ghc-proxy library crate: a GitHub Copilot API proxy exposing OpenAI- and
//! Anthropic-compatible endpoints. Rust port of `ghc-tunnel`.

pub mod anthropic;
pub mod auth;
pub mod config;
pub mod filters;
pub mod gemini;
pub mod responses;
pub mod server;
pub mod state;
pub mod store;
pub mod translate;
pub mod util;
