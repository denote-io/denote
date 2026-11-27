//! Constructs transports from persisted configuration and keychain secrets.

use std::path::Path;

use crate::{
    error::{AppError, AppResult},
    plugins::{PluginManager, git::PluginGitAuthMode},
    sync::{
        backend::Backend,
        config::{BackendConfig, BackendContext, BackendSecrets, GitAuthMode, GoogleConnection},
    },
};

use super::{
    folder::FolderBackend,
    gdrive::{self, GoogleClient, GoogleDriveBackend},
    git::{GitBackend, GitBackendConfig, clone_directory},
    s3::S3Backend,
    webdav::WebDavBackend,
};

/// The Git plugin whose host-owned executable and GitHub CLI choices the Git
/// transport reuses when it is installed. Otherwise the sync plugin's own
/// defaults apply: bundled Git and no GitHub CLI.
const GIT_TOOLS_PLUGIN_ID: &str = "denote.git";

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
        (BackendConfig::Webdav { url }, BackendSecrets::Webdav { username, password }) => {
            Ok(Box::new(WebDavBackend::new(url, username, password)?))
        }
        (
            BackendConfig::S3 {
                endpoint,
                region,
                bucket,
                prefix,
                path_style,
            },
            BackendSecrets::S3 {
                access_key_id,
                secret_access_key,
            },
        ) => Ok(Box::new(S3Backend::new(
            endpoint,
            region,
            bucket,
            prefix,
            *path_style,
            access_key_id,
            secret_access_key,
        )?)),
        (
            BackendConfig::GoogleDrive {
                folder_name,
                client_id,
                ..
            },
            BackendSecrets::GoogleDrive { refresh_token },
        ) => {
            let client = GoogleClient::resolve(client_id.clone())?;
            Ok(Box::new(GoogleDriveBackend::new(
                refresh_token,
                &client.id,
                client.secret.as_deref(),
                folder_name,
            )?))
        }
        (
            BackendConfig::Git {
                remote_url,
                branch,
                auth_mode,
            },
            _,
        ) => {
            let plugins = context.plugins.as_ref().ok_or_else(|| {
                AppError::State("Git sync needs the plugin manager, which is unavailable.".into())
            })?;
            let mut git = GitBackendConfig::for_plugin(
                plugins,
                &git_tools_plugin(plugins, &context.plugin_id),
                clone_directory(
                    &context.sync_dir.join("backends").join("git"),
                    remote_url,
                    branch,
                ),
                remote_url.clone(),
                branch.clone(),
                git_auth_mode(*auth_mode),
                context.vault_roots.clone(),
            )?;
            git.cancel = Some(context.cancel.clone());
            Ok(Box::new(GitBackend::new(git)?))
        }
        (_, _) => Err(AppError::State(format!(
            "The saved {} sync credentials are missing or do not match. Reconnect sync.",
            config.kind()
        ))),
    }
}

fn git_tools_plugin(plugins: &PluginManager, sync_plugin_id: &str) -> String {
    if plugins.settings(GIT_TOOLS_PLUGIN_ID).is_ok() {
        GIT_TOOLS_PLUGIN_ID.to_string()
    } else {
        sync_plugin_id.to_string()
    }
}

fn git_auth_mode(mode: GitAuthMode) -> PluginGitAuthMode {
    match mode {
        GitAuthMode::System => PluginGitAuthMode::System,
        GitAuthMode::Public => PluginGitAuthMode::Public,
        GitAuthMode::SshAgent => PluginGitAuthMode::SshAgent,
        GitAuthMode::GithubHttps => PluginGitAuthMode::GithubHttps,
    }
}

/// Runs the Google Drive OAuth PKCE loopback flow, opening the consent page
/// with `open_url`. The result stays host-side until `sync_configure`.
pub(crate) fn connect_google(
    client_id: Option<String>,
    open_url: &dyn Fn(&str) -> AppResult<()>,
) -> AppResult<GoogleConnection> {
    let mut connection = gdrive::connect(client_id, open_url)?;
    Ok(GoogleConnection {
        account_label: std::mem::take(&mut connection.account_label),
        refresh_token: std::mem::take(&mut connection.refresh_token),
        client_id: connection.client_id.take(),
    })
}
