//! Portable plugin settings and state. See `docs/sync.md` ("Scope matrix").

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::AppResult;

/// Settings that name executables, paths, authentication, signing, or standing
/// automation stay on the device that chose them.
const LOCAL_SETTING_KEYS: [&str; 13] = [
    "gitExecutableMode",
    "gitExecutablePath",
    "githubExecutableMode",
    "githubExecutablePath",
    "useSystemGitSettings",
    "authenticationMode",
    "commitSigning",
    "gpgSigningKey",
    "autoCommitIntervalMinutes",
    "autoPushAfterCommit",
    "autoCommitMessage",
    "includePatterns",
    "excludePatterns",
];

pub(crate) fn portable_setting(key: &str) -> bool {
    !LOCAL_SETTING_KEYS.contains(&key)
        && !key.ends_with("Path")
        && !key.ends_with("Executable")
        && !key.ends_with("ExecutableMode")
}

/// One enabled plugin's synced data.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PluginDataView {
    pub plugin_id: String,
    pub settings_version: u32,
    /// Portable settings whose value differs from the manifest default.
    pub settings: BTreeMap<String, Value>,
    pub state: BTreeMap<String, Value>,
}

pub(crate) trait PluginDataSource: Send + Sync {
    /// Enabled plugins, plus a warning for each plugin whose data could not be read.
    fn enabled_plugins(&self) -> AppResult<(Vec<PluginDataView>, Vec<String>)>;
    /// Sets (or, for `None`, resets to its default) one portable setting through
    /// validated settings import. Returns `false` when the plugin is not enabled
    /// or uses a different settings schema version.
    fn apply_setting(
        &self,
        plugin_id: &str,
        version: u32,
        key: &str,
        value: Option<&Value>,
    ) -> AppResult<bool>;
    /// Sets or removes one state value. Returns `false` when the plugin is not enabled.
    fn apply_state(&self, plugin_id: &str, key: &str, value: Option<&Value>) -> AppResult<bool>;
}

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::Mutex;

    use super::*;
    use crate::error::AppError;

    /// In-memory plugins for engine tests.
    #[derive(Default)]
    pub(crate) struct FakePlugins {
        pub plugins: Mutex<BTreeMap<String, PluginDataView>>,
    }

    impl FakePlugins {
        pub(crate) fn enable(&self, plugin_id: &str, version: u32) {
            self.plugins.lock().unwrap().insert(
                plugin_id.into(),
                PluginDataView {
                    plugin_id: plugin_id.into(),
                    settings_version: version,
                    ..PluginDataView::default()
                },
            );
        }

        pub(crate) fn disable(&self, plugin_id: &str) {
            self.plugins.lock().unwrap().remove(plugin_id);
        }

        pub(crate) fn view(&self, plugin_id: &str) -> Option<PluginDataView> {
            self.plugins.lock().unwrap().get(plugin_id).cloned()
        }

        pub(crate) fn set_setting(&self, plugin_id: &str, key: &str, value: Value) {
            if let Some(view) = self.plugins.lock().unwrap().get_mut(plugin_id) {
                view.settings.insert(key.into(), value);
            }
        }

        pub(crate) fn set_state(&self, plugin_id: &str, key: &str, value: Value) {
            if let Some(view) = self.plugins.lock().unwrap().get_mut(plugin_id) {
                view.state.insert(key.into(), value);
            }
        }
    }

    impl PluginDataSource for FakePlugins {
        fn enabled_plugins(&self) -> AppResult<(Vec<PluginDataView>, Vec<String>)> {
            let plugins = self
                .plugins
                .lock()
                .map_err(|_| AppError::State("fake plugins poisoned".into()))?;
            let views = plugins
                .values()
                .cloned()
                .map(|mut view| {
                    view.settings.retain(|key, _| portable_setting(key));
                    view
                })
                .collect();
            Ok((views, Vec::new()))
        }

        fn apply_setting(
            &self,
            plugin_id: &str,
            version: u32,
            key: &str,
            value: Option<&Value>,
        ) -> AppResult<bool> {
            let mut plugins = self
                .plugins
                .lock()
                .map_err(|_| AppError::State("fake plugins poisoned".into()))?;
            let Some(view) = plugins.get_mut(plugin_id) else {
                return Ok(false);
            };
            if view.settings_version != version {
                return Ok(false);
            }
            match value {
                Some(value) => view.settings.insert(key.into(), value.clone()),
                None => view.settings.remove(key),
            };
            Ok(true)
        }

        fn apply_state(
            &self,
            plugin_id: &str,
            key: &str,
            value: Option<&Value>,
        ) -> AppResult<bool> {
            let mut plugins = self
                .plugins
                .lock()
                .map_err(|_| AppError::State("fake plugins poisoned".into()))?;
            let Some(view) = plugins.get_mut(plugin_id) else {
                return Ok(false);
            };
            match value {
                Some(value) => view.state.insert(key.into(), value.clone()),
                None => view.state.remove(key),
            };
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_and_path_settings_stay_local() {
        for key in [
            "gitExecutablePath",
            "githubExecutableMode",
            "authenticationMode",
            "commitSigning",
            "gpgSigningKey",
            "autoPushAfterCommit",
            "includePatterns",
            "customToolPath",
            "pandocExecutable",
            "linterExecutableMode",
        ] {
            assert!(!portable_setting(key), "{key}");
        }
        for key in ["remoteName", "fontSize", "path", "theme"] {
            assert!(portable_setting(key), "{key}");
        }
    }
}
