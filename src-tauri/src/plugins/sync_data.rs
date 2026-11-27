//! Host access to plugin settings and state for whole-app sync. Plugins never
//! see this path; values are applied through the validated settings import.

use serde_json::Value;

use crate::{
    error::{AppError, AppResult},
    sync::plugin_data::{PluginDataSource, PluginDataView, portable_setting},
};

use super::{
    PluginManager,
    sandbox::enforce_storage_quota,
    settings::{default_settings, settings_schema_version, validate_storage_key},
};

impl PluginDataSource for PluginManager {
    fn enabled_plugins(&self) -> AppResult<(Vec<PluginDataView>, Vec<String>)> {
        let (enabled, storage) = {
            let state = self.state()?;
            (state.enabled.clone(), state.storage.clone())
        };
        let mut views = Vec::new();
        let mut warnings = Vec::new();
        for plugin_id in enabled {
            let read = || -> AppResult<PluginDataView> {
                let manifest = self.runtime_manifest(&plugin_id)?;
                let defaults = default_settings(&manifest);
                let settings = self.settings(&plugin_id)?;
                let settings = settings
                    .as_object()
                    .map(|object| {
                        object
                            .iter()
                            .filter(|(key, value)| {
                                portable_setting(key) && defaults.get(key.as_str()) != Some(*value)
                            })
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(PluginDataView {
                    plugin_id: plugin_id.clone(),
                    settings_version: settings_schema_version(&manifest),
                    settings,
                    state: storage.get(&plugin_id).cloned().unwrap_or_default(),
                })
            };
            match read() {
                Ok(view) => views.push(view),
                Err(error) => warnings.push(format!(
                    "Plugin data for {plugin_id} was not synced: {error}"
                )),
            }
        }
        Ok((views, warnings))
    }

    fn apply_setting(
        &self,
        plugin_id: &str,
        version: u32,
        key: &str,
        value: Option<&Value>,
    ) -> AppResult<bool> {
        if !portable_setting(key) {
            return Err(AppError::Plugin(format!(
                "The {key} setting is device-local and is never synced"
            )));
        }
        if !self.state()?.enabled.contains(plugin_id) {
            return Ok(false);
        }
        let manifest = self.runtime_manifest(plugin_id)?;
        if settings_schema_version(&manifest) != version {
            return Ok(false);
        }
        let mut settings = self.settings(plugin_id)?;
        let object = settings.as_object_mut().ok_or_else(|| {
            AppError::Plugin(format!("Settings for {plugin_id} must be an object"))
        })?;
        match value
            .cloned()
            .or_else(|| default_settings(&manifest).get(key).cloned())
        {
            Some(value) => object.insert(key.to_string(), value),
            None => object.remove(key),
        };
        self.import_settings(plugin_id, version, settings)?;
        Ok(true)
    }

    fn apply_state(&self, plugin_id: &str, key: &str, value: Option<&Value>) -> AppResult<bool> {
        validate_storage_key(key)?;
        if !self.state()?.enabled.contains(plugin_id) {
            return Ok(false);
        }
        self.update_state(|state| {
            let storage = state.storage.entry(plugin_id.to_string()).or_default();
            match value {
                Some(value) => {
                    enforce_storage_quota(storage, key, value)?;
                    storage.insert(key.to_string(), value.clone());
                }
                None => {
                    storage.remove(key);
                }
            }
            Ok(())
        })?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::super::tests::{catalog, manager};
    use super::*;

    #[test]
    fn syncs_portable_non_default_settings_and_state_of_enabled_plugins_only() {
        let data = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut catalog = catalog();
        catalog.manifest.settings = Some(json!({
            "properties": {
                "fontSize": {"type": "number", "default": 14},
                "toolPath": {"type": "string", "default": ""}
            }
        }));
        let manager = manager(catalog.clone(), &data, &cache);
        let id = catalog.manifest.id.clone();
        assert!(manager.enabled_plugins().unwrap().0.is_empty());
        assert!(!manager.apply_state(&id, "count", Some(&json!(1))).unwrap());
        {
            let mut state = manager.state().unwrap();
            state.enabled.insert(id.clone());
            state
                .installed_manifests
                .insert(id.clone(), catalog.manifest.clone());
        }
        let (views, warnings) = manager.enabled_plugins().unwrap();
        assert!(warnings.is_empty());
        assert_eq!(
            views[0].settings,
            BTreeMap::new(),
            "defaults are not synced"
        );

        assert!(
            manager
                .apply_setting(&id, 0, "fontSize", Some(&json!(18)))
                .unwrap()
        );
        assert!(manager.apply_state(&id, "count", Some(&json!(3))).unwrap());
        let view = manager.enabled_plugins().unwrap().0.remove(0);
        assert_eq!(
            view.settings,
            BTreeMap::from([("fontSize".into(), json!(18))])
        );
        assert_eq!(view.state, BTreeMap::from([("count".into(), json!(3))]));

        assert!(
            manager
                .apply_setting(&id, 0, "toolPath", Some(&json!("/x")))
                .is_err()
        );
        assert!(
            !manager
                .apply_setting(&id, 7, "fontSize", Some(&json!(9)))
                .unwrap()
        );
        assert!(manager.apply_setting(&id, 0, "fontSize", None).unwrap());
        assert!(manager.apply_state(&id, "count", None).unwrap());
        let view = manager.enabled_plugins().unwrap().0.remove(0);
        assert!(view.settings.is_empty() && view.state.is_empty());
        assert_eq!(manager.settings(&id).unwrap()["fontSize"], json!(14));
    }
}
