//! In-memory backend for tests, with failure injection.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use crate::{
    error::{AppError, AppResult},
    sync::backend::{Backend, PutMode, PutOutcome, validate_key, validate_prefix},
};

#[derive(Default)]
pub(crate) struct MemoryBackend {
    values: Mutex<BTreeMap<String, Vec<u8>>>,
    failing: Mutex<BTreeSet<String>>,
}

impl MemoryBackend {
    pub(crate) fn keys(&self) -> Vec<String> {
        self.values
            .lock()
            .expect("memory backend")
            .keys()
            .cloned()
            .collect()
    }

    pub(crate) fn raw(&self, key: &str) -> Option<Vec<u8>> {
        self.values
            .lock()
            .expect("memory backend")
            .get(key)
            .cloned()
    }

    pub(crate) fn set_raw(&self, key: &str, bytes: Vec<u8>) {
        self.values
            .lock()
            .expect("memory backend")
            .insert(key.to_string(), bytes);
    }

    pub(crate) fn remove_raw(&self, key: &str) {
        self.values.lock().expect("memory backend").remove(key);
    }

    /// Every operation on a key starting with `prefix` fails until cleared.
    pub(crate) fn fail_prefix(&self, prefix: &str) {
        self.failing
            .lock()
            .expect("memory backend")
            .insert(prefix.to_string());
    }

    pub(crate) fn clear_failures(&self) {
        self.failing.lock().expect("memory backend").clear();
    }

    fn check(&self, key: &str) -> AppResult<()> {
        if self
            .failing
            .lock()
            .map_err(|_| AppError::State("memory backend poisoned".into()))?
            .iter()
            .any(|prefix| key.starts_with(prefix.as_str()))
        {
            return Err(AppError::State(format!(
                "Synthetic backend failure for {key}"
            )));
        }
        Ok(())
    }

    fn values(&self) -> AppResult<std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>>> {
        self.values
            .lock()
            .map_err(|_| AppError::State("memory backend poisoned".into()))
    }
}

impl Backend for MemoryBackend {
    fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        validate_key(key)?;
        self.check(key)?;
        Ok(self.values()?.get(key).cloned())
    }

    fn put(&self, key: &str, bytes: &[u8], mode: PutMode) -> AppResult<PutOutcome> {
        validate_key(key)?;
        self.check(key)?;
        let mut values = self.values()?;
        if mode == PutMode::CreateOnly && values.contains_key(key) {
            return Ok(PutOutcome::AlreadyExists);
        }
        values.insert(key.to_string(), bytes.to_vec());
        Ok(PutOutcome::Written)
    }

    fn list(&self, prefix: &str) -> AppResult<Vec<String>> {
        validate_prefix(prefix)?;
        self.check(prefix)?;
        Ok(self
            .values()?
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect())
    }

    fn delete(&self, key: &str) -> AppResult<()> {
        validate_key(key)?;
        self.check(key)?;
        self.values()?.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn memory_backend_meets_the_contract() {
        crate::sync::backend_tests::conformance(&super::MemoryBackend::default());
    }
}
