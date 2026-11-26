//! Whole-app sync. See `docs/sync.md` for the protocol, scope, and invariants.

pub(crate) mod apply;
pub(crate) mod backend;
#[cfg(test)]
pub(crate) mod backend_tests;
pub(crate) mod backends;
pub(crate) mod commands;
pub(crate) mod config;
pub(crate) mod crypto;
pub(crate) mod engine;
pub(crate) mod merge;
pub(crate) mod metadata;
pub(crate) mod model;
pub(crate) mod plugin_data;
pub(crate) mod scan;
pub(crate) mod service;
pub(crate) mod status;
pub(crate) mod store;
