use std::{sync::Mutex, time::Duration};

#[cfg(any(test, target_os = "macos"))]
use std::path::{Component, Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

use reqwest::{Url, redirect::Policy};
use semver::Version;
use serde::{Deserialize, Serialize};
#[cfg(target_os = "macos")]
use tauri::Manager;
use tauri::{AppHandle, Runtime, ipc::Channel, utils::platform::bundle_type};
use tauri_plugin_updater::{Update, UpdaterExt};
#[cfg(target_os = "macos")]
use walkdir::WalkDir;

use crate::error::{AppError, AppResult};

const CONFIG_SOURCE: &str = include_str!("../updater.json");
const RELEASE_ASSET_SOURCE: &str = include_str!("../../release-assets.json");
const MAX_UPDATE_BYTES: usize = 512 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
// The repository is moving to denote-io/denote. Accept both paths so this build
// keeps updating before and after the GitHub transfer.
const OFFICIAL_REPOSITORIES: [&str; 2] = ["mbianchidev/denote", "denote-io/denote"];
const DEVELOPMENT_IDENTIFIER: &str = "dev.mbianchi.denote.development";
const UPDATE_MANIFEST_NAME: &str = "latest.json";
const RELEASE_LIST_PAGE_SIZE: usize = 30;
const MAX_RELEASE_LIST_BYTES: usize = 2 * 1024 * 1024;

/// The stable channel reads the latest full release. The opt-in beta channel
/// also considers prereleases, then reads that release's signed manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    Stable,
    Beta,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubReleaseAsset {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdaterConfiguration {
    schema_version: u32,
    enabled: bool,
    channel: String,
    endpoint: String,
    public_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseAssetContract {
    schema_version: u32,
    repository: String,
    updater_targets: Vec<UpdaterTarget>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdaterTarget {
    target: String,
    release_file_name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInfo {
    pub operating_system: String,
    pub architecture: String,
    pub bundle_type: String,
    pub update_channel: String,
    pub updater_configured: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableUpdate {
    pub version: String,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", content = "data", rename_all = "camelCase")]
pub enum UpdateProgress {
    Started { content_length: Option<u64> },
    Progress { downloaded: u64 },
    Finished,
}

struct PreparedUpdate {
    update: Update,
    bytes: Vec<u8>,
}

#[derive(Default)]
pub struct UpdateManager {
    prepared: Mutex<Option<PreparedUpdate>>,
}

#[tauri::command]
pub fn get_runtime_info<R: Runtime>(app: AppHandle<R>) -> AppResult<RuntimeInfo> {
    let config = configuration()?;
    Ok(RuntimeInfo {
        operating_system: std::env::consts::OS.to_string(),
        architecture: std::env::consts::ARCH.to_string(),
        bundle_type: current_bundle_type(),
        update_channel: if app.config().identifier == DEVELOPMENT_IDENTIFIER {
            "development".to_string()
        } else {
            config.channel.clone()
        },
        updater_configured: app.config().identifier != DEVELOPMENT_IDENTIFIER
            && configuration_is_enabled(&config),
    })
}

#[tauri::command]
pub async fn check_for_update<R: Runtime>(
    app: AppHandle<R>,
    channel: UpdateChannel,
) -> AppResult<Option<AvailableUpdate>> {
    let update = checked_update(&app, channel).await?;
    Ok(update.map(|update| AvailableUpdate {
        version: update.version,
        notes: update.body,
    }))
}

#[tauri::command]
pub async fn download_update<R: Runtime>(
    app: AppHandle<R>,
    manager: tauri::State<'_, UpdateManager>,
    channel: UpdateChannel,
    expected_version: String,
    progress: Channel<UpdateProgress>,
) -> AppResult<AvailableUpdate> {
    discard_prepared(&manager)?;
    let update = checked_update(&app, channel)
        .await?
        .ok_or_else(|| AppError::Update("No newer Denote release is available".to_string()))?;
    if update.version != expected_version {
        return Err(AppError::Update(format!(
            "The available update changed from {expected_version} to {}. Check again before downloading.",
            update.version
        )));
    }
    let available = AvailableUpdate {
        version: update.version.clone(),
        notes: update.body.clone(),
    };
    let mut downloaded = 0_u64;
    let mut started = false;
    let bytes = update
        .download(
            |chunk_length, content_length| {
                if !started {
                    started = true;
                    let _ = progress.send(UpdateProgress::Started { content_length });
                }
                downloaded = downloaded.saturating_add(chunk_length as u64);
                let _ = progress.send(UpdateProgress::Progress { downloaded });
            },
            || {
                let _ = progress.send(UpdateProgress::Finished);
            },
        )
        .await
        .map_err(update_error)?;
    if bytes.len() > MAX_UPDATE_BYTES {
        return Err(AppError::Update(format!(
            "The verified update exceeds the {} MiB safety limit",
            MAX_UPDATE_BYTES / 1024 / 1024
        )));
    }
    let mut prepared = manager
        .prepared
        .lock()
        .map_err(|_| AppError::Update("The prepared update lock is poisoned".to_string()))?;
    *prepared = Some(PreparedUpdate { update, bytes });
    Ok(available)
}

#[tauri::command]
pub fn discard_prepared_update(manager: tauri::State<'_, UpdateManager>) -> AppResult<()> {
    discard_prepared(&manager)
}

#[tauri::command]
pub fn install_prepared_update<R: Runtime>(
    app: AppHandle<R>,
    manager: tauri::State<'_, UpdateManager>,
    expected_version: String,
) -> AppResult<()> {
    let prepared = {
        let mut slot = manager
            .prepared
            .lock()
            .map_err(|_| AppError::Update("The prepared update lock is poisoned".to_string()))?;
        slot.take().ok_or_else(|| {
            AppError::Update("Download the update before installing it".to_string())
        })?
    };
    if prepared.update.version != expected_version {
        return Err(AppError::Update(
            "The prepared update no longer matches the reviewed version".to_string(),
        ));
    }
    #[cfg(target_os = "macos")]
    let recovery = prepare_macos_recovery(&app)?;
    if let Err(error) = prepared.update.install(&prepared.bytes) {
        restore_prepared(&manager, prepared)?;
        return Err(update_error(error));
    }

    #[cfg(target_os = "macos")]
    if let Err(error) = remove_installed_app_quarantine(&app) {
        let rollback = restore_macos_recovery(&recovery);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(AppError::Update(format!(
                "{error}. The previous Denote installation could not be restored: {rollback_error}"
            ))),
        };
    }

    app.restart();
}

async fn checked_update<R: Runtime>(
    app: &AppHandle<R>,
    channel: UpdateChannel,
) -> AppResult<Option<Update>> {
    let config = configuration()?;
    if app.config().identifier == DEVELOPMENT_IDENTIFIER {
        return Err(AppError::Update(
            "Updates are unavailable in Denote Development".to_string(),
        ));
    }
    if !configuration_is_enabled(&config) {
        return Err(AppError::Update(
            "The stable update channel is not configured with a trusted public key".to_string(),
        ));
    }
    let public_key = config
        .public_key
        .as_deref()
        .expect("enabled updater configuration has a public key");
    let stable_endpoint = Url::parse(&config.endpoint)
        .map_err(|error| AppError::Update(format!("Invalid update endpoint: {error}")))?;
    validate_endpoint(&stable_endpoint)?;
    let endpoint = match channel {
        UpdateChannel::Stable => stable_endpoint,
        UpdateChannel::Beta => beta_endpoint(&stable_endpoint).await?,
    };
    let updater = app
        .updater_builder()
        .pubkey(public_key)
        .endpoints(vec![endpoint])
        .map_err(update_error)?
        .timeout(Duration::from_secs(30))
        .configure_client(|builder| builder.redirect(github_redirect_policy()))
        .build()
        .map_err(update_error)?;
    let update = updater.check().await.map_err(update_error)?;
    if let Some(update) = &update {
        validate_update_download(update)?;
    }
    Ok(update)
}

/// Selects the newest published official release, including prereleases. The
/// GitHub listing only chooses which signed manifest to read; the manifest
/// signature, version comparison, and download URL are verified as for stable.
async fn beta_endpoint(stable_endpoint: &Url) -> AppResult<Url> {
    let repository = official_repository(stable_endpoint)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(github_api_redirect_policy())
        .user_agent(format!("Denote/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(update_error)?;
    let mut response = client
        .get(format!(
            "https://api.github.com/repos/{repository}/releases?per_page={RELEASE_LIST_PAGE_SIZE}"
        ))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|error| {
            AppError::Update(format!("Unable to list Denote beta releases: {error}"))
        })?;
    if !response.status().is_success() {
        return Err(AppError::Update(format!(
            "GitHub returned HTTP {} while listing Denote beta releases",
            response.status().as_u16()
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        AppError::Update(format!("Unable to read Denote beta releases: {error}"))
    })? {
        if body.len() + chunk.len() > MAX_RELEASE_LIST_BYTES {
            return Err(AppError::Update(
                "The Denote beta release list exceeded its size limit".to_string(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let releases: Vec<GithubRelease> = serde_json::from_slice(&body)
        .map_err(|error| AppError::Update(format!("Invalid Denote beta release list: {error}")))?;
    match newest_release_tag(&releases) {
        Some(tag) => tagged_endpoint(repository, &tag),
        None => Ok(stable_endpoint.clone()),
    }
}

fn newest_release_tag(releases: &[GithubRelease]) -> Option<String> {
    releases
        .iter()
        .filter(|release| {
            !release.draft
                && release
                    .assets
                    .iter()
                    .any(|asset| asset.name == UPDATE_MANIFEST_NAME)
        })
        .filter_map(|release| {
            let version = Version::parse(release.tag_name.strip_prefix('v')?).ok()?;
            Some((version, release.tag_name.clone()))
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, tag)| tag)
}

fn tagged_endpoint(repository: &str, tag: &str) -> AppResult<Url> {
    let endpoint = Url::parse(&format!(
        "https://github.com/{repository}/releases/download/{tag}/{UPDATE_MANIFEST_NAME}"
    ))
    .map_err(|error| AppError::Update(format!("Invalid beta update endpoint: {error}")))?;
    validate_tagged_endpoint(&endpoint)?;
    Ok(endpoint)
}

fn official_repository(endpoint: &Url) -> AppResult<&'static str> {
    OFFICIAL_REPOSITORIES
        .into_iter()
        .find(|repository| {
            endpoint.path() == format!("/{repository}/releases/latest/download/latest.json")
        })
        .ok_or_else(|| {
            AppError::Update(
                "The update endpoint must be the official Denote GitHub Release endpoint"
                    .to_string(),
            )
        })
}

fn validate_tagged_endpoint(url: &Url) -> AppResult<()> {
    let tagged_version = url
        .path_segments()
        .and_then(|segments| segments.rev().nth(1))
        .and_then(|tag| tag.strip_prefix('v'))
        .and_then(|version| Version::parse(version).ok());
    let valid = url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.query().is_none()
        && url.fragment().is_none()
        && tagged_version.is_some_and(|version| {
            is_official_release_path(
                url.path(),
                &format!("releases/download/v{version}/{UPDATE_MANIFEST_NAME}"),
            )
        });
    if !valid {
        return Err(AppError::Update(
            "The beta update endpoint must be an official Denote GitHub Release manifest"
                .to_string(),
        ));
    }
    Ok(())
}

fn configuration() -> AppResult<UpdaterConfiguration> {
    let config: UpdaterConfiguration = serde_json::from_str(CONFIG_SOURCE)
        .map_err(|error| AppError::Update(format!("Invalid updater configuration: {error}")))?;
    if config.schema_version != 1 || config.channel != "stable" {
        return Err(AppError::Update(
            "Unsupported updater configuration".to_string(),
        ));
    }
    validate_endpoint(
        &Url::parse(&config.endpoint)
            .map_err(|error| AppError::Update(format!("Invalid update endpoint: {error}")))?,
    )?;
    Ok(config)
}

fn configuration_is_enabled(config: &UpdaterConfiguration) -> bool {
    config.enabled
        && config
            .public_key
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
}

fn release_contract() -> AppResult<ReleaseAssetContract> {
    parse_release_contract(RELEASE_ASSET_SOURCE)
}

fn parse_release_contract(source: &str) -> AppResult<ReleaseAssetContract> {
    let contract: ReleaseAssetContract = serde_json::from_str(source)
        .map_err(|error| AppError::Update(format!("Invalid release asset contract: {error}")))?;
    if contract.schema_version != 1
        || !OFFICIAL_REPOSITORIES.contains(&contract.repository.as_str())
    {
        return Err(AppError::Update(
            "Unsupported release asset contract".to_string(),
        ));
    }
    Ok(contract)
}

fn is_official_release_path(path: &str, release_path: &str) -> bool {
    OFFICIAL_REPOSITORIES
        .iter()
        .any(|repository| path == format!("/{repository}/{release_path}"))
}

fn validate_endpoint(url: &Url) -> AppResult<()> {
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !is_official_release_path(url.path(), "releases/latest/download/latest.json")
    {
        return Err(AppError::Update(
            "The update endpoint must be the official Denote GitHub Release endpoint".to_string(),
        ));
    }
    Ok(())
}

fn validate_update_download(update: &Update) -> AppResult<()> {
    let contract = release_contract()?;
    let target = current_updater_target().ok_or_else(|| {
        AppError::Update("This Denote package type does not support in-app updates".to_string())
    })?;
    let expected = contract
        .updater_targets
        .iter()
        .find(|entry| entry.target == target)
        .ok_or_else(|| {
            AppError::Update(format!(
                "No signed updater artifact is defined for {target}"
            ))
        })?;
    let expected_name = render_release_name(&expected.release_file_name, &update.version)?;
    validate_download_url(&update.download_url, &update.version, &expected_name)
}

fn validate_download_url(url: &Url, version: &str, expected_name: &str) -> AppResult<()> {
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !is_official_release_path(
            url.path(),
            &format!("releases/download/v{version}/{expected_name}"),
        )
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::Update(
            "The update metadata selected an unofficial or unexpected artifact".to_string(),
        ));
    }
    Ok(())
}

fn current_updater_target() -> Option<String> {
    let target = tauri_plugin_updater::target()?;
    Some(format!("{target}-{}", current_bundle_type()))
}

fn current_bundle_type() -> String {
    bundle_type()
        .map(|bundle| format!("{bundle:?}").to_ascii_lowercase())
        .unwrap_or_else(|| "unknown".to_string())
}

fn render_release_name(template: &str, version: &str) -> AppResult<String> {
    if !template.contains("{version}")
        || template.contains('/')
        || template.contains('\\')
        || !version
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".-+".contains(character))
    {
        return Err(AppError::Update(
            "Invalid updater artifact filename".to_string(),
        ));
    }
    Ok(template.replace("{version}", version))
}

fn github_redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many GitHub release redirects");
        }
        match attempt.url().host_str() {
            Some(
                "github.com"
                | "objects.githubusercontent.com"
                | "release-assets.githubusercontent.com"
                | "github-releases.githubusercontent.com",
            ) if attempt.url().scheme() == "https" => attempt.follow(),
            _ => attempt.error("update redirect left the GitHub release host allowlist"),
        }
    })
}

fn github_api_redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many GitHub API redirects");
        }
        if attempt.url().scheme() == "https" && attempt.url().host_str() == Some("api.github.com") {
            attempt.follow()
        } else {
            attempt.error("release list redirect left api.github.com")
        }
    })
}

fn discard_prepared(manager: &UpdateManager) -> AppResult<()> {
    let mut prepared = manager
        .prepared
        .lock()
        .map_err(|_| AppError::Update("The prepared update lock is poisoned".to_string()))?;
    prepared.take();
    Ok(())
}

fn restore_prepared(manager: &UpdateManager, prepared: PreparedUpdate) -> AppResult<()> {
    let mut slot = manager
        .prepared
        .lock()
        .map_err(|_| AppError::Update("The prepared update lock is poisoned".to_string()))?;
    *slot = Some(prepared);
    Ok(())
}

fn update_error(error: impl std::fmt::Display) -> AppError {
    AppError::Update(error.to_string())
}

#[cfg(target_os = "macos")]
fn remove_installed_app_quarantine<R: Runtime>(app: &AppHandle<R>) -> AppResult<()> {
    let executable = std::env::current_exe()
        .map_err(|error| AppError::Update(format!("Unable to locate Denote: {error}")))?;
    let bundle = app_bundle_from_executable(&executable, &app.package_info().name)?;
    for entry in WalkDir::new(&bundle).follow_links(false) {
        let entry = entry.map_err(|error| {
            AppError::Update(format!("Unable to inspect updated Denote: {error}"))
        })?;
        if entry.file_type().is_symlink() {
            continue;
        }
        if xattr::get(entry.path(), "com.apple.quarantine")
            .map_err(|error| {
                AppError::Update(format!(
                    "Unable to inspect Denote quarantine metadata: {error}"
                ))
            })?
            .is_some()
        {
            xattr::remove(entry.path(), "com.apple.quarantine").map_err(|error| {
                AppError::Update(format!(
                    "Unable to remove quarantine from the verified Denote update: {error}"
                ))
            })?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
struct MacUpdateRecovery {
    _directory: tempfile::TempDir,
    backup_bundle: PathBuf,
    installed_bundle: PathBuf,
}

#[cfg(target_os = "macos")]
fn prepare_macos_recovery<R: Runtime>(app: &AppHandle<R>) -> AppResult<MacUpdateRecovery> {
    let executable = std::env::current_exe()
        .map_err(|error| AppError::Update(format!("Unable to locate Denote: {error}")))?;
    let installed_bundle = app_bundle_from_executable(&executable, &app.package_info().name)?;
    let cache = app.path().app_cache_dir().map_err(|error| {
        AppError::Update(format!("Unable to locate update recovery storage: {error}"))
    })?;
    std::fs::create_dir_all(&cache).map_err(|error| {
        AppError::Update(format!(
            "Unable to prepare update recovery storage: {error}"
        ))
    })?;
    let directory = tempfile::Builder::new()
        .prefix("denote-update-recovery-")
        .tempdir_in(cache)
        .map_err(|error| {
            AppError::Update(format!("Unable to create update recovery storage: {error}"))
        })?;
    let backup_bundle = directory.path().join("Denote.app");
    run_ditto(
        &installed_bundle,
        &backup_bundle,
        "back up the current Denote app",
    )?;
    Ok(MacUpdateRecovery {
        _directory: directory,
        backup_bundle,
        installed_bundle,
    })
}

#[cfg(target_os = "macos")]
fn restore_macos_recovery(recovery: &MacUpdateRecovery) -> AppResult<()> {
    if recovery.installed_bundle.exists() {
        std::fs::remove_dir_all(&recovery.installed_bundle).map_err(|error| {
            AppError::Update(format!(
                "Unable to remove the failed Denote replacement: {error}"
            ))
        })?;
    }
    run_ditto(
        &recovery.backup_bundle,
        &recovery.installed_bundle,
        "restore the previous Denote app",
    )
}

#[cfg(target_os = "macos")]
fn run_ditto(source: &Path, destination: &Path, action: &str) -> AppResult<()> {
    let status = Command::new("/usr/bin/ditto")
        .arg("--rsrc")
        .arg("--extattr")
        .arg("--acl")
        .arg(source)
        .arg(destination)
        .status()
        .map_err(|error| AppError::Update(format!("Unable to {action}: {error}")))?;
    if !status.success() {
        return Err(AppError::Update(format!(
            "Unable to {action}; ditto exited with {status}"
        )));
    }
    Ok(())
}

#[cfg(any(test, target_os = "macos"))]
fn app_bundle_from_executable(executable: &Path, product_name: &str) -> AppResult<PathBuf> {
    if product_name != "Denote" {
        return Err(AppError::Update(
            "Only the production Denote app bundle can be finalized".to_string(),
        ));
    }
    let components = executable.components().collect::<Vec<_>>();
    let contents = components
        .windows(2)
        .position(|pair| {
            pair[0] == Component::Normal("Denote.app".as_ref())
                && pair[1] == Component::Normal("Contents".as_ref())
        })
        .ok_or_else(|| {
            AppError::Update("The running executable is not inside Denote.app".to_string())
        })?;
    let mut bundle = PathBuf::new();
    for component in components.into_iter().take(contents + 1) {
        bundle.push(component.as_os_str());
    }
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_updater_has_a_trusted_public_key() {
        let config = configuration().expect("configuration");
        assert!(configuration_is_enabled(&config));
        assert!(config.public_key.is_some_and(|key| !key.trim().is_empty()));
    }

    #[test]
    fn beta_channel_selects_the_newest_published_release_with_a_manifest() {
        let release = |tag: &str, draft: bool, manifest: bool| GithubRelease {
            tag_name: tag.to_string(),
            draft,
            assets: if manifest {
                vec![GithubReleaseAsset {
                    name: UPDATE_MANIFEST_NAME.to_string(),
                }]
            } else {
                vec![GithubReleaseAsset {
                    name: "Denote_9.0.0_aarch64.dmg".to_string(),
                }]
            },
        };
        let releases = [
            release("v0.14.3", false, true),
            release("v1.0.0-rc.1", false, true),
            release("v1.0.0-rc.2", false, true),
            release("v1.0.0-rc.3", true, true),
            release("v2.0.0", false, false),
            release("nightly", false, true),
            release("v1.0", false, true),
        ];
        assert_eq!(
            newest_release_tag(&releases).as_deref(),
            Some("v1.0.0-rc.2")
        );

        let with_final = [
            release("v1.0.0-rc.2", false, true),
            release("v1.0.0", false, true),
        ];
        assert_eq!(newest_release_tag(&with_final).as_deref(), Some("v1.0.0"));
        assert_eq!(newest_release_tag(&[]), None);
    }

    #[test]
    fn beta_endpoints_are_exact_official_release_manifests() {
        for repository in OFFICIAL_REPOSITORIES {
            let stable = Url::parse(&format!(
                "https://github.com/{repository}/releases/latest/download/latest.json"
            ))
            .expect("url");
            assert_eq!(official_repository(&stable).expect("official"), repository);
            assert_eq!(
                tagged_endpoint(repository, "v1.0.0-rc.1")
                    .expect("tagged endpoint")
                    .as_str(),
                format!(
                    "https://github.com/{repository}/releases/download/v1.0.0-rc.1/latest.json"
                )
            );
        }
        for rejected in [
            "https://github.com/denote-io/denote/releases/download/v1.0.0-rc.1/other.json",
            "https://github.com/denote-io/denote/releases/download/1.0.0-rc.1/latest.json",
            "https://github.com/denote-io/denote/releases/download/vnext/latest.json",
            "https://github.com/synthetic-owner/denote/releases/download/v1.0.0/latest.json",
            "https://example.test/denote-io/denote/releases/download/v1.0.0/latest.json",
            "http://github.com/denote-io/denote/releases/download/v1.0.0/latest.json",
            "https://github.com/denote-io/denote/releases/download/v1.0.0/latest.json?x=1",
        ] {
            assert!(
                validate_tagged_endpoint(&Url::parse(rejected).expect("url")).is_err(),
                "{rejected}"
            );
        }
        assert!(tagged_endpoint("synthetic-owner/denote", "v1.0.0").is_err());
    }

    #[test]
    fn update_channels_accept_only_stable_and_beta() {
        assert_eq!(
            serde_json::from_str::<UpdateChannel>("\"stable\"").expect("stable"),
            UpdateChannel::Stable
        );
        assert_eq!(
            serde_json::from_str::<UpdateChannel>("\"beta\"").expect("beta"),
            UpdateChannel::Beta
        );
        assert!(serde_json::from_str::<UpdateChannel>("\"nightly\"").is_err());
    }

    #[test]
    fn official_endpoint_is_exact_for_both_repository_owners() {
        for repository in ["mbianchidev/denote", "denote-io/denote"] {
            validate_endpoint(
                &Url::parse(&format!(
                    "https://github.com/{repository}/releases/latest/download/latest.json"
                ))
                .expect("url"),
            )
            .expect("official endpoint");
        }
        for rejected in [
            "https://example.test/latest.json",
            "http://github.com/denote-io/denote/releases/latest/download/latest.json",
            "https://github.com/synthetic-owner/denote/releases/latest/download/latest.json",
            "https://github.com/denote-io/denote-fork/releases/latest/download/latest.json",
            "https://github.com/denote-io/denote/releases/latest/download/other.json",
        ] {
            assert!(
                validate_endpoint(&Url::parse(rejected).expect("url")).is_err(),
                "{rejected}"
            );
        }
    }

    #[test]
    fn release_contract_accepts_only_official_repositories() {
        let contract = |repository: &str| {
            format!(r#"{{"schemaVersion":1,"repository":"{repository}","updaterTargets":[]}}"#)
        };
        assert!(parse_release_contract(&contract("mbianchidev/denote")).is_ok());
        assert!(parse_release_contract(&contract("denote-io/denote")).is_ok());
        assert!(parse_release_contract(&contract("synthetic-owner/denote")).is_err());
        release_contract().expect("embedded release contract");
    }

    #[test]
    fn update_downloads_must_be_exact_official_release_assets() {
        let name = "Denote_1.2.3_aarch64.app.tar.gz";
        for repository in ["mbianchidev/denote", "denote-io/denote"] {
            let url = Url::parse(&format!(
                "https://github.com/{repository}/releases/download/v1.2.3/{name}"
            ))
            .expect("url");
            validate_download_url(&url, "1.2.3", name).expect("official download");
        }
        for rejected in [
            format!("https://github.com/synthetic-owner/denote/releases/download/v1.2.3/{name}"),
            format!("https://example.test/denote-io/denote/releases/download/v1.2.3/{name}"),
            format!("https://github.com/denote-io/denote/releases/download/v1.2.4/{name}"),
            format!("https://github.com/denote-io/denote/releases/download/v1.2.3/{name}?x=1"),
            format!("https://github.com/denote-io/denote/releases/download/v1.2.3/{name}#x"),
            "https://github.com/denote-io/denote/releases/download/v1.2.3/Other.tar.gz".to_string(),
        ] {
            assert!(
                validate_download_url(&Url::parse(&rejected).expect("url"), "1.2.3", name).is_err(),
                "{rejected}"
            );
        }
    }

    #[test]
    fn macos_bundle_path_is_derived_without_caller_input() {
        let path = app_bundle_from_executable(
            Path::new("/Applications/Denote.app/Contents/MacOS/denote"),
            "Denote",
        )
        .expect("bundle");
        assert_eq!(path, PathBuf::from("/Applications/Denote.app"));
        assert!(
            app_bundle_from_executable(
                Path::new("/Applications/Other.app/Contents/MacOS/denote"),
                "Denote",
            )
            .is_err()
        );
    }

    #[test]
    fn release_names_reject_paths() {
        assert_eq!(
            render_release_name("Denote_{version}_x64.dmg", "1.2.3").expect("name"),
            "Denote_1.2.3_x64.dmg"
        );
        assert!(render_release_name("../{version}", "1.2.3").is_err());
    }
}
