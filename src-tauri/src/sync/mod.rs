//! Whole-app sync. See `docs/sync.md` for the protocol, scope, and invariants.

pub(crate) mod apply;
pub(crate) mod backend;
#[cfg(test)]
pub(crate) mod backend_tests;
pub(crate) mod backends;
pub(crate) mod config;
pub(crate) mod crypto;
pub(crate) mod merge;
pub(crate) mod model;
pub(crate) mod scan;
pub(crate) mod store;
