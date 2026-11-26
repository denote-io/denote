//! Storage transports implementing [`super::backend::Backend`].

mod factory;
pub(crate) mod folder;
#[cfg(test)]
pub(crate) mod memory;

pub(crate) use factory::{connect_google, open};
