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

pub(crate) fn open(
    config: &BackendConfig,
    secrets: &BackendSecrets,
    context: &BackendContext,
) -> AppResult<Box<dyn Backend>> {
    let _ = (secrets, context);
    match config {
        BackendConfig::Folder { path } => Ok(Box::new(FolderBackend::open(Path::new(path))?)),
        // Wired by the network transports (`webdav`, `s3`, `gdrive`, `git`).
        BackendConfig::Webdav { .. }
        | BackendConfig::S3 { .. }
        | BackendConfig::GoogleDrive { .. }
        | BackendConfig::Git { .. } => Err(unavailable(config.kind())),
    }
}

/// Runs the Google Drive OAuth PKCE loopback flow.
pub(crate) fn connect_google(
    client_id: Option<String>,
    open_url: &dyn Fn(&str) -> AppResult<()>,
) -> AppResult<GoogleConnection> {
    let _ = (client_id, open_url);
    Err(unavailable("google-drive"))
}

fn unavailable(kind: &str) -> AppError {
    AppError::State(format!("The {kind} sync backend is not available in this build."))
}
