//! Storage contract shared by every sync transport. See `docs/sync.md`.

use crate::error::{AppError, AppResult};

/// Remote prefix under which every key lives.
pub(crate) const REMOTE_PREFIX: &str = "denote-sync/v1/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PutMode {
    CreateOnly,
    Overwrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PutOutcome {
    Written,
    AlreadyExists,
}

/// A flat key-value store. Keys are relative to [`REMOTE_PREFIX`] and validated
/// with [`validate_key`] before any transport sees them.
pub(crate) trait Backend: Send + Sync {
    fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>>;
    /// Never exposes a partially written value under `key`.
    fn put(&self, key: &str, bytes: &[u8], mode: PutMode) -> AppResult<PutOutcome>;
    /// Every key under `prefix`, following all pagination. Order is unspecified.
    /// Failures are errors, never an empty result.
    fn list(&self, prefix: &str) -> AppResult<Vec<String>>;
    #[allow(
        dead_code,
        reason = "format 1 never deletes remote data; the conformance suite covers it"
    )]
    fn delete(&self, key: &str) -> AppResult<()>;
    /// Make remote state visible locally (Git: fetch and rebase).
    fn refresh(&self) -> AppResult<()> {
        Ok(())
    }
    /// Publish buffered writes (Git: commit and push).
    fn flush(&self) -> AppResult<()> {
        Ok(())
    }
}

/// Writes `bytes` under a key that must not exist yet. A key that already
/// holds exactly these bytes counts as written: a retried request whose first
/// attempt landed, or a Google Drive duplicate resolved to that first upload,
/// reports the key as taken although this device wrote it. Sealed envelopes
/// carry a random nonce, so equal bytes prove the write was this one.
pub(crate) fn put_new(backend: &dyn Backend, key: &str, bytes: &[u8]) -> AppResult<PutOutcome> {
    match backend.put(key, bytes, PutMode::CreateOnly)? {
        PutOutcome::Written => Ok(PutOutcome::Written),
        PutOutcome::AlreadyExists if backend.get(key)?.as_deref() == Some(bytes) => {
            Ok(PutOutcome::Written)
        }
        PutOutcome::AlreadyExists => Ok(PutOutcome::AlreadyExists),
    }
}

const MAX_KEY_LENGTH: usize = 512;

/// Accepts `[a-z0-9._/-]` segments without empty, `.`, or `..` parts.
pub(crate) fn validate_key(key: &str) -> AppResult<()> {
    let valid = !key.is_empty()
        && key.len() <= MAX_KEY_LENGTH
        && key.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b'/' | b'-')
        })
        && key
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidData(format!(
            "Invalid sync storage key: {key}"
        )))
    }
}

/// Validates a listing prefix: empty, or a valid key optionally ending in `/`.
pub(crate) fn validate_prefix(prefix: &str) -> AppResult<()> {
    if prefix.is_empty() {
        return Ok(());
    }
    validate_key(prefix.strip_suffix('/').unwrap_or(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_confined() {
        for key in [
            "repository.json",
            "objects/ab/abcdef",
            "heads/0123/00000000000000000001",
        ] {
            validate_key(key).unwrap();
        }
        for key in [
            "", "/abs", "a//b", "../x", "a/./b", "Upper", "a b", "a\\b", "a/",
        ] {
            assert!(validate_key(key).is_err(), "{key}");
        }
        validate_prefix("").unwrap();
        validate_prefix("heads/").unwrap();
        assert!(validate_prefix("../").is_err());
    }
}
