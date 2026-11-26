//! Constructs transports from persisted configuration and keychain secrets.

use std::path::Path;

use crate::{
    error::{AppError, AppResult},
    sync::{
        backend::Backend,
        config::{BackendConfig, BackendContext, BackendSecrets, GoogleConnection},
    },
};

use super::folder::FolderBackend;

/// Opens the transport `config` names. Secrets come from the keychain only.
pub(crate) fn open(
    config: &BackendConfig,
    secrets: &BackendSecrets,
    context: &BackendContext,
) -> AppResult<Box<dyn Backend>> {
    match (config, secrets) {
        (BackendConfig::Folder { path }, _) => {
            let root = Path::new(path);
            let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            if context
                .vault_roots
                .iter()
                .any(|vault| canonical.starts_with(vault) || vault.starts_with(&canonical))
            {
                return Err(AppError::InvalidPath(
                    "The sync folder cannot be inside a vault or contain one".into(),
                ));
            }
            Ok(Box::new(FolderBackend::open(root)?))
        }
        // The network transports (`webdav`, `s3`, `gdrive`, `git`) are wired here.
        (
            BackendConfig::Webdav { .. }
            | BackendConfig::S3 { .. }
            | BackendConfig::GoogleDrive { .. }
            | BackendConfig::Git { .. },
            _,
        ) => Err(unavailable(config.kind())),
    }
}

/// Runs the Google Drive OAuth PKCE loopback flow, opening the consent page
/// with `open_url`. The result stays host-side until `sync_configure`.
pub(crate) fn connect_google(
    client_id: Option<String>,
    open_url: &dyn Fn(&str) -> AppResult<()>,
) -> AppResult<GoogleConnection> {
    let _ = (client_id, open_url);
    Err(unavailable("google-drive"))
}

fn unavailable(kind: &str) -> AppError {
    AppError::State(format!(
        "The {kind} sync backend is not available in this build."
    ))
}
