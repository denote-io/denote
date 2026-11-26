//! Storage transports implementing [`super::backend::Backend`].

pub(crate) mod gdrive;
pub(crate) mod git;
pub(crate) mod http;
pub(crate) mod s3;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod webdav;
