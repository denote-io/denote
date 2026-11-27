//! Sync payloads, persisted backend configuration, and keychain secrets.
//! See `docs/sync.md` ("Command payloads").

use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{AppError, AppResult};

pub(crate) const MAX_INTERVAL_MINUTES: u32 = 1440;
/// Portable renderer preferences. Every other key is rejected.
pub(crate) const PREFERENCE_KEYS: [&str; 6] = [
    "denote-theme",
    "denote-editor-display",
    "denote-markdown-view-mode",
    "denote-show-dotfiles",
    "denote.speech-shortcut.v1",
    "denote.plugin-rail.v1",
];
pub(crate) const MAX_PREFERENCE_BYTES: usize = 64 * 1024;
pub(crate) const DEFAULT_INTERVAL_MINUTES: u32 = 15;
const MAX_FIELD_BYTES: usize = 2048;
const MAX_SECRET_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum GitAuthMode {
    System,
    Public,
    SshAgent,
    GithubHttps,
}

/// Renderer request. Secrets travel only renderer to host.
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum SyncBackendRequest {
    Folder {
        path: String,
    },
    Webdav {
        url: String,
        username: String,
        password: String,
    },
    #[serde(rename_all = "camelCase")]
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        prefix: String,
        path_style: bool,
        access_key_id: String,
        secret_access_key: String,
    },
    #[serde(rename_all = "camelCase")]
    GoogleDrive {
        connection_token: String,
        folder_name: String,
    },
    #[serde(rename_all = "camelCase")]
    Git {
        remote_url: String,
        branch: String,
        #[zeroize(skip)]
        auth_mode: GitAuthMode,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SetupMode {
    Create,
    Join,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SyncSetupRequest {
    pub mode: SetupMode,
    pub device_name: String,
    pub passphrase: String,
    pub remember_passphrase: bool,
    pub backend: SyncBackendRequest,
}

impl Drop for SyncSetupRequest {
    fn drop(&mut self) {
        self.passphrase.zeroize();
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SyncOptions {
    pub interval_minutes: u32,
    pub paused: bool,
    pub sync_history: bool,
    pub sync_preferences: bool,
    pub sync_plugin_data: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            paused: false,
            sync_history: true,
            sync_preferences: true,
            sync_plugin_data: true,
        }
    }
}

impl SyncOptions {
    pub(crate) fn validate(&self) -> AppResult<()> {
        if self.interval_minutes > MAX_INTERVAL_MINUTES {
            return Err(AppError::InvalidData(format!(
                "The sync interval must be between 0 and {MAX_INTERVAL_MINUTES} minutes"
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum SyncVaultRequest {
    #[serde(rename_all = "camelCase")]
    MapNew { sync_id: String, path: String },
    #[serde(rename_all = "camelCase")]
    MapExisting { sync_id: String, scope_id: String },
    #[serde(rename_all = "camelCase")]
    Skip { sync_id: String },
    #[serde(rename_all = "camelCase")]
    Enroll { scope_id: String },
    #[serde(rename_all = "camelCase")]
    Stop { sync_id: String },
    /// Publishes deletions a scan held back because they removed most of the vault.
    #[serde(rename_all = "camelCase")]
    ConfirmDeletions { sync_id: String },
}

/// Non-secret, persisted backend identity.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum BackendConfig {
    Folder {
        path: String,
    },
    Webdav {
        url: String,
    },
    #[serde(rename_all = "camelCase")]
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        prefix: String,
        path_style: bool,
    },
    #[serde(rename_all = "camelCase")]
    GoogleDrive {
        folder_name: String,
        account_label: String,
        client_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Git {
        remote_url: String,
        branch: String,
        auth_mode: GitAuthMode,
    },
}

impl BackendConfig {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Folder { .. } => "folder",
            Self::Webdav { .. } => "webdav",
            Self::S3 { .. } => "s3",
            Self::GoogleDrive { .. } => "google-drive",
            Self::Git { .. } => "git",
        }
    }

    /// A display label without credentials.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Folder { path } => path.clone(),
            Self::Webdav { url } => url.clone(),
            Self::S3 {
                endpoint,
                bucket,
                prefix,
                ..
            } => {
                if prefix.is_empty() {
                    format!("{bucket} ({endpoint})")
                } else {
                    format!("{bucket}/{prefix} ({endpoint})")
                }
            }
            Self::GoogleDrive {
                folder_name,
                account_label,
                ..
            } => format!("{folder_name} ({account_label})"),
            Self::Git {
                remote_url, branch, ..
            } => format!("{remote_url} ({branch})"),
        }
    }
}

/// Non-secret configuration persisted in the sync store.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersistedConfig {
    /// The plugin whose enabled `sync` permission authorizes sync.
    pub plugin_id: String,
    pub repository_id: String,
    pub backend: BackendConfig,
    pub device_name: String,
    pub remember_passphrase: bool,
    pub options: SyncOptions,
}

/// Credentials stored only in the operating-system keychain.
#[derive(Debug, Deserialize, Serialize, Zeroize, ZeroizeOnDrop, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum BackendSecrets {
    None,
    Webdav {
        username: String,
        password: String,
    },
    #[serde(rename_all = "camelCase")]
    S3 {
        access_key_id: String,
        secret_access_key: String,
    },
    #[serde(rename_all = "camelCase")]
    GoogleDrive {
        refresh_token: String,
    },
}

/// Host services a transport may need. Never contains secrets.
#[derive(Clone)]
pub(crate) struct BackendContext {
    /// The sync plugin whose permission authorizes this transport.
    pub plugin_id: String,
    /// `sync/` in application data. Transports keep private state only under
    /// `sync_dir/backends/<kind>/`, which Disconnect removes.
    pub sync_dir: PathBuf,
    /// Set when the operation that opened this transport is cancelled.
    pub cancel: Arc<AtomicBool>,
    pub plugins: Option<crate::plugins::PluginManager>,
    /// Every known vault root; a transport's private data may not overlap them.
    pub vault_roots: Vec<PathBuf>,
}

/// Result of the Google Drive OAuth flow, held by the host until `sync_configure`.
pub(crate) struct GoogleConnection {
    pub account_label: String,
    pub refresh_token: String,
    pub client_id: Option<String>,
}

impl Drop for GoogleConnection {
    fn drop(&mut self) {
        self.refresh_token.zeroize();
    }
}

pub(crate) fn bounded_field(value: &str, label: &str, allow_empty: bool) -> AppResult<String> {
    let trimmed = value.trim();
    if (!allow_empty && trimmed.is_empty())
        || trimmed.len() > MAX_FIELD_BYTES
        || trimmed.chars().any(char::is_control)
    {
        return Err(AppError::InvalidData(format!("Enter a valid {label}")));
    }
    Ok(trimmed.to_string())
}

pub(crate) fn bounded_secret(value: &str, label: &str) -> AppResult<String> {
    if value.is_empty()
        || value.len() > MAX_SECRET_BYTES
        || value.chars().any(|character| character == '\0')
    {
        return Err(AppError::InvalidData(format!("Enter a valid {label}")));
    }
    Ok(value.to_string())
}

pub(crate) fn validate_device_name(name: &str) -> AppResult<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 64 || trimmed.chars().any(char::is_control) {
        return Err(AppError::InvalidData(
            "Device names must contain 1 to 64 visible characters".into(),
        ));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_from_camel_case_payloads() {
        let request: SyncSetupRequest = serde_json::from_value(serde_json::json!({
            "mode": "join",
            "deviceName": "Laptop",
            "passphrase": "synthetic passphrase",
            "rememberPassphrase": true,
            "backend": {
                "kind": "s3", "endpoint": "https://s3.example.invalid", "region": "auto",
                "bucket": "b", "prefix": "", "pathStyle": true,
                "accessKeyId": "AKIASYNTHETIC", "secretAccessKey": "synthetic"
            }
        }))
        .unwrap();
        assert_eq!(request.mode, SetupMode::Join);
        assert!(matches!(
            request.backend,
            SyncBackendRequest::S3 {
                path_style: true,
                ..
            }
        ));
        let vault: SyncVaultRequest = serde_json::from_value(
            serde_json::json!({"action": "map-existing", "syncId": "a", "scopeId": "b"}),
        )
        .unwrap();
        assert_eq!(
            vault,
            SyncVaultRequest::MapExisting {
                sync_id: "a".into(),
                scope_id: "b".into()
            }
        );
        assert!(
            serde_json::from_value::<SyncOptions>(serde_json::json!({
                "intervalMinutes": 5, "paused": false, "syncHistory": true,
                "syncPreferences": true, "syncPluginData": true, "extra": 1
            }))
            .is_err()
        );
        let git: SyncBackendRequest = serde_json::from_value(serde_json::json!({
            "kind": "git", "remoteUrl": "https://example.invalid/r.git", "branch": "main", "authMode": "ssh-agent"
        }))
        .unwrap();
        assert!(matches!(
            git,
            SyncBackendRequest::Git {
                auth_mode: GitAuthMode::SshAgent,
                ..
            }
        ));
    }

    #[test]
    fn options_and_names_are_bounded() {
        assert!(
            SyncOptions {
                interval_minutes: 1441,
                ..SyncOptions::default()
            }
            .validate()
            .is_err()
        );
        SyncOptions {
            interval_minutes: 0,
            ..SyncOptions::default()
        }
        .validate()
        .unwrap();
        assert!(validate_device_name(" ").is_err());
        assert!(validate_device_name(&"x".repeat(65)).is_err());
        assert_eq!(validate_device_name(" Desk ").unwrap(), "Desk");
    }
}
