//! Git transport: a private clone of a dedicated repository, driven through
//! the hardened host Git execution in `crate::plugins::git`.
//!
//! Keys are files under `denote-sync/v1/` in the clone's working tree.
//! `put`, `get`, `list`, and `delete` touch only the working tree; `refresh`
//! commits pending writes, fetches, and rebases onto the remote branch;
//! `flush` does the same and pushes, retrying a bounded number of times when
//! the push is rejected because the remote moved. It never force-pushes.
//!
//! Rebase conflicts are resolved by keeping the remote copy. Every key is
//! written once (`objects/` are content-addressed, `heads/` belong to one
//! writer), so a conflict means another device already published the same
//! key and its copy is authoritative.
//!
//! Every Git invocation runs with the host hardening (no hooks, filters,
//! credential helpers, pagers, or editors; HTTPS and SSH only). On top of
//! that, symbolic links are checked out as plain files and
//! `.git/info/attributes` marks every path binary without filters, so remote
//! content can neither redirect file access nor be transformed.

use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::Instant,
};

use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    plugins::git::{
        CommandOutcome, GIT_TIMEOUT, GitDirectoryState, GitExecution, GitOperationRegistry,
        GitTransportPolicy, PluginGitAuthMode, askpass::AskpassMaterial,
        assert_repository_config_is_safe, detect_operation_state, first_line, git_cli_path_string,
        push_needs_pull, redact, resolve_git_directory, run_git_command, validate_branch_name,
        validate_remote_url_for,
    },
    sync::backend::{Backend, PutMode, PutOutcome, REMOTE_PREFIX, validate_key, validate_prefix},
};

use super::http::MAX_OBJECT_BYTES;

const OPERATION_OWNER: &str = "host-sync";
const AUTHOR_NAME: &str = "Denote Sync";
const AUTHOR_EMAIL: &str = "sync@denote.invalid";
const COMMIT_MESSAGE: &str = "Denote sync";
const MAX_PUSH_ATTEMPTS: usize = 5;
const MAX_REBASE_STEPS: usize = 64;
const MAX_LISTED_KEYS: usize = 2_000_000;
const TEMP_DIRECTORY: &str = "denote-sync-tmp";
const MANAGED_ATTRIBUTES: &str = "# Managed by Denote sync: content is opaque.\n* -text -diff -merge -filter -ident -working-tree-encoding\n";

/// Produces askpass material for a network operation against `urls`, or
/// `None` when the mode needs nothing from the host. The integrator backs it
/// with the plugin manager's authentication adapter.
pub(crate) type GitCredentialProvider =
    Box<dyn Fn(PluginGitAuthMode, &[String]) -> AppResult<Option<AskpassMaterial>> + Send + Sync>;

pub(crate) struct GitBackendConfig {
    /// Host-owned directory for the private clone, under application data.
    pub(crate) clone_dir: PathBuf,
    pub(crate) remote_url: String,
    pub(crate) branch: String,
    pub(crate) auth_mode: PluginGitAuthMode,
    /// The resolved, pinned Git executable.
    pub(crate) executable: PathBuf,
    /// Host-owned empty hooks directory.
    pub(crate) hooks_directory: PathBuf,
    /// Host-owned empty global configuration file.
    pub(crate) global_config: PathBuf,
    /// Every known vault root; the clone may not overlap any of them.
    pub(crate) vault_roots: Vec<PathBuf>,
    pub(crate) credentials: Option<GitCredentialProvider>,
    pub(crate) transport: GitTransportPolicy,
}

pub(crate) struct GitBackend {
    config: GitBackendConfig,
    /// Canonical clone directory.
    root: PathBuf,
    remote_ref: String,
    operations: GitOperationRegistry,
    lock: Mutex<()>,
}

impl GitBackend {
    pub(crate) fn new(config: GitBackendConfig) -> AppResult<Self> {
        validate_remote_url_for(&config.remote_url, config.transport)?;
        validate_branch_name(&config.branch)?;
        if !config.clone_dir.is_absolute() {
            return Err(AppError::InvalidPath(
                "The Git sync clone directory must be absolute".to_string(),
            ));
        }
        reject_symlink(&config.clone_dir)?;
        fs::create_dir_all(&config.clone_dir)?;
        let root = fs::canonicalize(&config.clone_dir)?;
        for vault in &config.vault_roots {
            let vault = fs::canonicalize(vault).unwrap_or_else(|_| vault.clone());
            if root.starts_with(&vault) || vault.starts_with(&root) {
                return Err(AppError::InvalidPath(
                    "The Git sync repository cannot be inside a vault or contain one. Use a dedicated repository."
                        .to_string(),
                ));
            }
        }
        if Path::new(&config.remote_url).is_absolute() {
            let remote = fs::canonicalize(&config.remote_url)
                .unwrap_or_else(|_| PathBuf::from(&config.remote_url));
            if config.vault_roots.iter().any(|vault| {
                let vault = fs::canonicalize(vault).unwrap_or_else(|_| vault.clone());
                remote.starts_with(&vault)
            }) {
                return Err(AppError::InvalidPath(
                    "The Git sync remote cannot be a vault's repository".to_string(),
                ));
            }
        }
        let remote_ref = format!("refs/remotes/origin/{}", config.branch);
        let backend = Self {
            config,
            root,
            remote_ref,
            operations: GitOperationRegistry::default(),
            lock: Mutex::new(()),
        };
        backend.ensure_repository()?;
        Ok(backend)
    }

    /// Stops any running Git child, e.g. when sync is cancelled.
    pub(crate) fn cancel(&self) {
        self.operations.cancel_all();
    }

    fn guard(&self) -> AppResult<MutexGuard<'_, ()>> {
        self.lock
            .lock()
            .map_err(|_| AppError::State("Git sync lock is poisoned".to_string()))
    }

    fn git_dir(&self) -> PathBuf {
        self.root.join(".git")
    }

    fn data_dir(&self) -> PathBuf {
        self.root.join(REMOTE_PREFIX.trim_end_matches('/'))
    }

    fn run(&self, args: &[&str], network: bool, mutating: bool) -> AppResult<CommandOutcome> {
        let askpass = match (&self.config.credentials, network) {
            (Some(provider), true) => {
                provider(self.config.auth_mode, &[self.config.remote_url.clone()])?
            }
            _ => None,
        };
        let execution = GitExecution {
            executable: &self.config.executable,
            repository_root: &self.root,
            hooks_directory: &self.config.hooks_directory,
            global_config: &self.config.global_config,
            redacted_roots: vec![self.root.clone()],
            askpass: askpass.as_ref(),
            encrypted: false,
            transport: self.config.transport,
        };
        let mut arguments: Vec<String> = [
            "-c",
            "core.symlinks=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.quotePath=false",
            "-c",
            "core.longpaths=true",
            "-c",
            "rebase.updateRefs=false",
            "-c",
            "rebase.autoSquash=false",
            "-c",
            "pull.rebase=false",
            "-c",
            "commit.verbose=false",
        ]
        .iter()
        .map(|value| value.to_string())
        .collect();
        arguments.extend([
            "-c".to_string(),
            format!("user.name={AUTHOR_NAME}"),
            "-c".to_string(),
            format!("user.email={AUTHOR_EMAIL}"),
        ]);
        arguments.extend(args.iter().map(|value| value.to_string()));
        let operation_id = Uuid::new_v4().to_string();
        let token = self.operations.register(OPERATION_OWNER, &operation_id)?;
        let outcome = run_git_command(
            &arguments,
            &execution,
            &token,
            Instant::now() + GIT_TIMEOUT,
            mutating,
        );
        self.operations.finish(&operation_id);
        let outcome = outcome?;
        if outcome.cancelled {
            return Err(AppError::State("Git sync was cancelled".to_string()));
        }
        Ok(outcome)
    }

    /// Runs a command that must succeed.
    fn run_ok(&self, args: &[&str], network: bool, action: &str) -> AppResult<CommandOutcome> {
        let outcome = self.run(args, network, true)?;
        if outcome.exit_code != 0 {
            return Err(self.failure(action, &outcome));
        }
        Ok(outcome)
    }

    fn failure(&self, action: &str, outcome: &CommandOutcome) -> AppError {
        let stderr = String::from_utf8_lossy(&outcome.stderr);
        let detail = first_line(&redact(&stderr, std::slice::from_ref(&self.root)));
        let hint = if is_auth_failure(&stderr) {
            " Check the repository authentication in Sync settings."
        } else {
            ""
        };
        if detail.is_empty() {
            AppError::State(format!("Git sync could not {action}.{hint}"))
        } else {
            AppError::State(format!("Git sync could not {action}: {detail}.{hint}"))
        }
    }

    fn ensure_repository(&self) -> AppResult<()> {
        match resolve_git_directory(&self.root)? {
            GitDirectoryState::Missing => {
                if fs::read_dir(&self.root)?.next().is_some() {
                    return Err(AppError::InvalidPath(
                        "The Git sync clone directory is not empty and is not a repository"
                            .to_string(),
                    ));
                }
                self.run_ok(&["init", "--quiet"], false, "create the local repository")?;
                let head = format!("refs/heads/{}", self.config.branch);
                self.run_ok(&["symbolic-ref", "HEAD", &head], false, "select the branch")?;
            }
            GitDirectoryState::Directory => {}
        }
        let git_dir = self.git_dir();
        assert_repository_config_is_safe(&git_dir)?;
        let info = git_dir.join("info");
        reject_symlink(&info)?;
        fs::create_dir_all(&info)?;
        let attributes = info.join("attributes");
        reject_symlink(&attributes)?;
        if fs::read_to_string(&attributes).ok().as_deref() != Some(MANAGED_ATTRIBUTES) {
            fs::write(&attributes, MANAGED_ATTRIBUTES)?;
        }
        if detect_operation_state(&git_dir).rebase_in_progress {
            self.run_ok(&["rebase", "--abort"], false, "abort an interrupted rebase")?;
        }
        Ok(())
    }

    fn has_head(&self) -> AppResult<bool> {
        Ok(self
            .run(
                &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
                false,
                false,
            )?
            .exit_code
            == 0)
    }

    fn resolve(&self, reference: &str) -> AppResult<Option<String>> {
        let spec = format!("{reference}^{{commit}}");
        let outcome = self.run(&["rev-parse", "--verify", "--quiet", &spec], false, false)?;
        if outcome.exit_code != 0 {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8_lossy(&outcome.stdout).trim().to_string(),
        ))
    }

    fn remote_arg(&self) -> String {
        if Path::new(&self.config.remote_url).is_absolute() {
            git_cli_path_string(Path::new(&self.config.remote_url))
        } else {
            self.config.remote_url.clone()
        }
    }

    /// Commits pending working-tree changes under the remote prefix.
    fn commit_pending(&self) -> AppResult<()> {
        if !self.data_dir().exists() {
            return Ok(());
        }
        let prefix = REMOTE_PREFIX.trim_end_matches('/');
        self.run_ok(&["add", "--all", "--", prefix], false, "stage changes")?;
        let staged = self.run(&["diff", "--cached", "--quiet", "--", prefix], false, false)?;
        match staged.exit_code {
            0 => Ok(()),
            1 => {
                let author = format!("{AUTHOR_NAME} <{AUTHOR_EMAIL}>");
                self.run_ok(
                    &[
                        "commit",
                        "--quiet",
                        "--no-verify",
                        "--no-gpg-sign",
                        "--no-edit",
                        "--author",
                        &author,
                        "-m",
                        COMMIT_MESSAGE,
                        "--",
                        prefix,
                    ],
                    false,
                    "commit changes",
                )?;
                Ok(())
            }
            _ => Err(self.failure("inspect changes", &staged)),
        }
    }

    /// Fetches the remote branch. Returns `false` when it does not exist yet.
    fn fetch(&self) -> AppResult<bool> {
        let remote = self.remote_arg();
        let refspec = format!("+refs/heads/{}:{}", self.config.branch, self.remote_ref);
        let outcome = self.run(
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                &remote,
                &refspec,
            ],
            true,
            true,
        )?;
        if outcome.exit_code == 0 {
            return Ok(true);
        }
        // Tell a missing branch apart from a real failure without parsing
        // localized messages.
        let head = format!("refs/heads/{}", self.config.branch);
        let listing = self.run(&["ls-remote", "--heads", "--", &remote, &head], true, false)?;
        if listing.exit_code == 0 && listing.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(false);
        }
        Err(self.failure("fetch from the remote repository", &outcome))
    }

    /// Makes the local branch contain the fetched remote branch.
    fn integrate(&self) -> AppResult<()> {
        if self.resolve(&self.remote_ref)?.is_none() {
            return Ok(());
        }
        if !self.has_head()? {
            self.run_ok(
                &["reset", "--quiet", "--hard", &self.remote_ref],
                false,
                "check out the remote branch",
            )?;
            return Ok(());
        }
        let contained = self.run(
            &["merge-base", "--is-ancestor", &self.remote_ref, "HEAD"],
            false,
            false,
        )?;
        if contained.exit_code == 0 {
            return Ok(());
        }
        let outcome = self.run(
            &[
                "rebase",
                "--quiet",
                "--no-autostash",
                "--empty=drop",
                "--strategy-option=ours",
                &self.remote_ref,
            ],
            false,
            true,
        )?;
        if outcome.exit_code == 0 {
            return Ok(());
        }
        self.resolve_rebase()
    }

    /// Finishes a stopped rebase by keeping the remote side of every
    /// conflicting path under the remote prefix.
    fn resolve_rebase(&self) -> AppResult<()> {
        for _ in 0..MAX_REBASE_STEPS {
            if !detect_operation_state(&self.git_dir()).rebase_in_progress {
                return Ok(());
            }
            let unmerged = self.run(&["ls-files", "--unmerged", "-z"], false, false)?;
            if unmerged.exit_code != 0 {
                return self.abort_rebase("list conflicting files");
            }
            let mut conflicts: Vec<(String, bool)> = Vec::new();
            for entry in unmerged
                .stdout
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
            {
                let entry = String::from_utf8_lossy(entry);
                let Some((meta, path)) = entry.split_once('\t') else {
                    continue;
                };
                let upstream = meta.split_whitespace().nth(2) == Some("2");
                match conflicts.iter_mut().find(|(existing, _)| existing == path) {
                    Some((_, has_upstream)) => *has_upstream |= upstream,
                    None => conflicts.push((path.to_string(), upstream)),
                }
            }
            for (path, upstream) in &conflicts {
                if !path.starts_with(REMOTE_PREFIX) {
                    return self.abort_rebase("rebase onto the remote branch because a file outside denote-sync/v1/ conflicts");
                }
                // During a rebase, "ours" (stage 2) is the remote branch.
                let resolved = if *upstream {
                    self.run(&["checkout", "--ours", "--", path], false, true)?
                        .exit_code
                        == 0
                        && self.run(&["add", "--", path], false, true)?.exit_code == 0
                } else {
                    self.run(&["rm", "--quiet", "--force", "--", path], false, true)?
                        .exit_code
                        == 0
                };
                if !resolved {
                    return self.abort_rebase("resolve a conflict with the remote branch");
                }
            }
            let staged = self.run(&["diff", "--cached", "--quiet"], false, false)?;
            let step = if staged.exit_code == 0 {
                self.run(&["rebase", "--skip"], false, true)?
            } else {
                self.run(
                    &["-c", "core.editor=:", "rebase", "--continue"],
                    false,
                    true,
                )?
            };
            if step.exit_code != 0 && conflicts.is_empty() {
                return self.abort_rebase("continue the rebase onto the remote branch");
            }
        }
        self.abort_rebase("finish the rebase onto the remote branch")
    }

    fn abort_rebase(&self, action: &str) -> AppResult<()> {
        let _ = self.run(&["rebase", "--abort"], false, true);
        Err(AppError::State(format!("Git sync could not {action}")))
    }

    fn push(&self) -> AppResult<bool> {
        let remote = self.remote_arg();
        let refspec = format!("HEAD:refs/heads/{}", self.config.branch);
        let outcome = self.run(
            &[
                "push",
                "--porcelain",
                "--no-verify",
                "--no-signed",
                "--recurse-submodules=no",
                "--",
                &remote,
                &refspec,
            ],
            true,
            true,
        )?;
        if outcome.exit_code == 0 {
            self.run_ok(
                &["update-ref", &self.remote_ref, "HEAD"],
                false,
                "record the pushed branch",
            )?;
            return Ok(true);
        }
        if push_needs_pull(&String::from_utf8_lossy(&outcome.stdout)) {
            return Ok(false);
        }
        Err(self.failure("push to the remote repository", &outcome))
    }

    fn sync_remote(&self) -> AppResult<()> {
        self.commit_pending()?;
        self.fetch()?;
        self.integrate()
    }

    /// Resolves `key` under the data directory, refusing any symbolic link
    /// or non-directory on the way.
    fn key_path(&self, key: &str) -> AppResult<PathBuf> {
        validate_key(key)?;
        let mut path = self.root.clone();
        for component in Path::new(REMOTE_PREFIX).join(key).components() {
            let Component::Normal(segment) = component else {
                return Err(AppError::InvalidPath(format!(
                    "Invalid sync storage key: {key}"
                )));
            };
            path.push(segment);
            reject_symlink(&path)?;
        }
        Ok(path)
    }

    fn temp_file(&self) -> AppResult<(fs::File, PathBuf)> {
        let directory = self.git_dir().join(TEMP_DIRECTORY);
        reject_symlink(&directory)?;
        fs::create_dir_all(&directory)?;
        let path = directory.join(Uuid::new_v4().simple().to_string());
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok((file, path))
    }
}

fn reject_symlink(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(AppError::InvalidPath(
            "Git sync refuses to follow a symbolic link in its repository".to_string(),
        )),
        _ => Ok(()),
    }
}

fn is_auth_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    [
        "authentication failed",
        "permission denied",
        "could not read username",
        "terminal prompts disabled",
        "403",
        "401",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

impl Backend for GitBackend {
    fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        let _guard = self.guard()?;
        let path = self.key_path(key)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            return Err(AppError::InvalidData(format!(
                "Git sync item is not a regular file: {key}"
            )));
        }
        if metadata.len() > MAX_OBJECT_BYTES {
            return Err(AppError::InvalidData(format!(
                "Git sync item is too large: {key}"
            )));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        fs::File::open(&path)?
            .take(MAX_OBJECT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_OBJECT_BYTES {
            return Err(AppError::InvalidData(format!(
                "Git sync item is too large: {key}"
            )));
        }
        Ok(Some(bytes))
    }

    fn put(&self, key: &str, bytes: &[u8], mode: PutMode) -> AppResult<PutOutcome> {
        let _guard = self.guard()?;
        let path = self.key_path(key)?;
        if mode == PutMode::CreateOnly && fs::symlink_metadata(&path).is_ok() {
            return Ok(PutOutcome::AlreadyExists);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Re-check after creating parents, which could race a checkout.
        let path = self.key_path(key)?;
        let (mut file, temporary) = self.temp_file()?;
        let written = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&temporary, &path));
        if let Err(error) = written {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        Ok(PutOutcome::Written)
    }

    fn list(&self, prefix: &str) -> AppResult<Vec<String>> {
        let _guard = self.guard()?;
        validate_prefix(prefix)?;
        let data = self.data_dir();
        reject_symlink(&data)?;
        if !data.is_dir() {
            return Ok(Vec::new());
        }
        let start = match prefix.rfind('/') {
            Some(index) => self.key_path(&prefix[..index])?,
            None => data.clone(),
        };
        if !start.is_dir() {
            return Ok(Vec::new());
        }
        let mut keys = Vec::new();
        for entry in walkdir::WalkDir::new(&start)
            .follow_links(false)
            .max_depth(32)
        {
            let entry = entry.map_err(|error| {
                AppError::State(format!("Git sync could not list its repository: {error}"))
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(&data) else {
                continue;
            };
            let Some(relative) = relative.to_str() else {
                continue;
            };
            let key = relative.replace('\\', "/");
            if key.starts_with(prefix) && validate_key(&key).is_ok() {
                keys.push(key);
                if keys.len() > MAX_LISTED_KEYS {
                    return Err(AppError::State(
                        "Git sync listing exceeded the item limit".to_string(),
                    ));
                }
            }
        }
        Ok(keys)
    }

    fn delete(&self, key: &str) -> AppResult<()> {
        let _guard = self.guard()?;
        let path = self.key_path(key)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn refresh(&self) -> AppResult<()> {
        let _guard = self.guard()?;
        self.ensure_repository()?;
        self.sync_remote()
    }

    fn flush(&self) -> AppResult<()> {
        let _guard = self.guard()?;
        self.ensure_repository()?;
        for _ in 0..MAX_PUSH_ATTEMPTS {
            self.sync_remote()?;
            let Some(head) = self.resolve("HEAD")? else {
                return Ok(());
            };
            if self.resolve(&self.remote_ref)?.as_deref() == Some(head.as_str()) {
                return Ok(());
            }
            if self.push()? {
                return Ok(());
            }
        }
        Err(AppError::State(
            "Git sync could not push because the remote repository kept changing. Sync will try again later."
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{plugins::git::resolve_git_executable, sync::backends::test_support::conformance};

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        remote: PathBuf,
        executable: PathBuf,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let remote = root.join("remote.git");
        let executable = resolve_git_executable(None).unwrap();
        fs::create_dir_all(root.join("hooks")).unwrap();
        fs::write(root.join("global-config"), b"").unwrap();
        let status = std::process::Command::new(&executable)
            .args(["init", "--quiet", "--bare"])
            .arg(&remote)
            .env("GIT_CONFIG_GLOBAL", root.join("global-config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success());
        Fixture {
            _directory: directory,
            root,
            remote,
            executable,
        }
    }

    fn config(fixture: &Fixture, name: &str) -> GitBackendConfig {
        GitBackendConfig {
            clone_dir: fixture.root.join(name),
            remote_url: fixture.remote.to_string_lossy().into_owned(),
            branch: "sync".to_string(),
            auth_mode: PluginGitAuthMode::Public,
            executable: fixture.executable.clone(),
            hooks_directory: fixture.root.join("hooks"),
            global_config: fixture.root.join("global-config"),
            vault_roots: vec![fixture.root.join("vault")],
            credentials: None,
            transport: GitTransportPolicy::AllowLocal,
        }
    }

    fn backend(fixture: &Fixture, name: &str) -> GitBackend {
        GitBackend::new(config(fixture, name)).unwrap()
    }

    #[test]
    fn conforms_against_a_local_bare_repository() {
        let fixture = fixture();
        let first = backend(&fixture, "first");
        conformance(&first);
        let second = backend(&fixture, "second");
        second.refresh().unwrap();
        assert_eq!(
            second.get("repository.json").unwrap().as_deref(),
            Some(&b"third"[..])
        );
        assert_eq!(second.list("heads/").unwrap().len(), 3);
        assert_eq!(second.list("objects/").unwrap().len(), 40);
    }

    #[test]
    fn concurrent_writers_converge_and_keep_the_remote_copy() {
        let fixture = fixture();
        let first = backend(&fixture, "first");
        let second = backend(&fixture, "second");
        first.refresh().unwrap();
        second.refresh().unwrap();
        let object = format!("objects/ab/ab{}", "1".repeat(62));
        first
            .put(&object, b"from-first", PutMode::CreateOnly)
            .unwrap();
        first
            .put("heads/aaaa/00000000000000000001", b"a", PutMode::CreateOnly)
            .unwrap();
        second
            .put(&object, b"from-second", PutMode::CreateOnly)
            .unwrap();
        second
            .put("heads/bbbb/00000000000000000001", b"b", PutMode::CreateOnly)
            .unwrap();
        first.flush().unwrap();
        // The second push is rejected, rebased with the conflict resolved
        // toward the remote, and retried.
        second.flush().unwrap();
        first.refresh().unwrap();
        for backend in [&first, &second] {
            assert_eq!(
                backend.get(&object).unwrap().as_deref(),
                Some(&b"from-first"[..])
            );
            let mut heads = backend.list("heads/").unwrap();
            heads.sort();
            assert_eq!(
                heads,
                vec![
                    "heads/aaaa/00000000000000000001",
                    "heads/bbbb/00000000000000000001"
                ]
            );
        }
        // An interrupted cycle's uncommitted writes are committed on refresh.
        second
            .put("retired/aaaa", b"r", PutMode::Overwrite)
            .unwrap();
        second.refresh().unwrap();
        second.flush().unwrap();
        first.refresh().unwrap();
        assert_eq!(
            first.get("retired/aaaa").unwrap().as_deref(),
            Some(&b"r"[..])
        );
    }

    #[test]
    fn refuses_unsafe_configuration() {
        let fixture = fixture();
        let mut inside_vault = config(&fixture, "vault/sync");
        inside_vault.vault_roots = vec![fixture.root.join("vault")];
        fs::create_dir_all(fixture.root.join("vault")).unwrap();
        assert!(GitBackend::new(inside_vault).is_err());
        let mut bad_branch = config(&fixture, "bad-branch");
        bad_branch.branch = "--force".to_string();
        assert!(GitBackend::new(bad_branch).is_err());
        let mut bad_remote = config(&fixture, "bad-remote");
        bad_remote.remote_url = "http://example.com/repo.git".to_string();
        assert!(GitBackend::new(bad_remote).is_err());
        let occupied = fixture.root.join("occupied");
        fs::create_dir_all(&occupied).unwrap();
        fs::write(occupied.join("file"), b"x").unwrap();
        assert!(GitBackend::new(config(&fixture, "occupied")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_symbolic_links_in_the_working_tree() {
        let fixture = fixture();
        let backend = backend(&fixture, "links");
        let outside = fixture.root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(backend.data_dir()).unwrap();
        std::os::unix::fs::symlink(&outside, backend.data_dir().join("objects")).unwrap();
        assert!(
            backend
                .put("objects/ab/x", b"x", PutMode::Overwrite)
                .is_err()
        );
        assert!(backend.get("objects/ab/x").is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }
}
