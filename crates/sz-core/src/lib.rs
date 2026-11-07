//! Core model, adapters and operations for SessionZero.

pub mod adapter;
pub mod adapters;
pub mod cache;
pub mod config;
pub mod group;
pub mod model;
pub mod paths;
pub mod rules;
pub mod scan;
pub mod trash;
pub mod util;

pub use adapter::Adapter;
pub use model::{AgentKind, Artifact, DbDelete, PreviewMsg, Session};
