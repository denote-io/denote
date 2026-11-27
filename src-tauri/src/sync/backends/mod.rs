//! Storage transports implementing [`super::backend::Backend`].

mod factory;
pub(crate) mod folder;
pub(crate) mod gdrive;
pub(crate) mod git;
pub(crate) mod http;
#[cfg(test)]
pub(crate) mod memory;
pub(crate) mod s3;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod webdav;

pub(crate) use factory::{connect_google, open};
