//! Applies merged vault files to disk. See `docs/sync.md` (steps 5 and 6).

use std::{
    collections::HashMap,
    fs,
    io::{ErrorKind, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

use uuid::Uuid;

use super::{
    crypto::{Keys, MAX_FILE_BYTES},
    model::valid_relative_path,
    scan::{TEMPORARY_SUFFIX, fingerprint, is_excluded, nfc, read_file},
    store::Fingerprint,
};
use crate::error::{AppError, AppResult};

/// The disk state a write may replace, as observed by the scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Expected {
    Absent,
    File {
        disk_path: PathBuf,
        fingerprint: Fingerprint,
        id: String,
    },
}

pub(crate) enum Change<'a> {
    Write {
        staged: &'a Path,
        mode: u32,
        modified_ms: i64,
    },
    Remove,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Applied; the new disk fingerprint, or `None` after a removal.
    Applied(Option<Fingerprint>),
    /// Left alone and retried on a later cycle.
    Skipped(String),
}

/// Writes or removes one vault file. Immediately before changing anything, the
/// current file is revalidated against `expected`; a file that changed since
/// the scan is left alone.
pub(crate) fn apply_file(
    root: &Path,
    key_path: &str,
    expected: &Expected,
    change: Change<'_>,
    keys: &Keys,
    cache: &mut DirectoryCache,
) -> AppResult<Outcome> {
    if !valid_relative_path(key_path) || is_excluded(key_path) {
        return Ok(Outcome::Skipped(format!(
            "{key_path}: this path is never written by sync"
        )));
    }
    if let Some(reason) = platform_name_issue(key_path) {
        return Ok(Outcome::Skipped(format!("{key_path}: {reason}")));
    }
    let relative = match expected {
        Expected::File { disk_path, .. } => disk_path.clone(),
        Expected::Absent => match free_path(root, key_path, cache)? {
            Ok(path) => path,
            Err(reason) => return Ok(Outcome::Skipped(format!("{key_path}: {reason}"))),
        },
    };
    let target = super::scan::confined(root, &relative)?;
    if !matches_expected(root, &target, expected, keys)? {
        return Ok(Outcome::Skipped(format!(
            "{key_path}: changed on this device during sync; it will be retried"
        )));
    }
    match change {
        Change::Remove => {
            if matches!(expected, Expected::Absent) {
                return Ok(Outcome::Applied(None));
            }
            fs::remove_file(&target)?;
            remove_empty_parents(root, &relative, cache);
            Ok(Outcome::Applied(None))
        }
        Change::Write {
            staged,
            mode,
            modified_ms,
        } => {
            let parent = create_parents(root, &relative, cache)?;
            let name = target
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| AppError::InvalidPath("Invalid sync file name".into()))?;
            let temporary = parent.join(format!(".{name}.{}{TEMPORARY_SUFFIX}", Uuid::new_v4()));
            let written = write_temporary(&temporary, staged, mode, modified_ms);
            if let Err(error) = written {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
            // Revalidate once more right before the rename.
            if !matches_expected(root, &target, expected, keys)? {
                let _ = fs::remove_file(&temporary);
                return Ok(Outcome::Skipped(format!(
                    "{key_path}: changed on this device during sync; it will be retried"
                )));
            }
            let renamed = match expected {
                Expected::Absent => crate::vault::rename_no_replace(&temporary, &target),
                Expected::File { .. } => fs::rename(&temporary, &target),
            };
            if let Err(error) = renamed {
                let _ = fs::remove_file(&temporary);
                if error.kind() == ErrorKind::AlreadyExists {
                    return Ok(Outcome::Skipped(format!(
                        "{key_path}: created on this device during sync; it will be retried"
                    )));
                }
                return Err(error.into());
            }
            cache.forget(&parent);
            let metadata = fs::symlink_metadata(&target)?;
            Ok(Outcome::Applied(Some(fingerprint(&metadata))))
        }
    }
}

fn matches_expected(
    root: &Path,
    target: &Path,
    expected: &Expected,
    keys: &Keys,
) -> AppResult<bool> {
    match (expected, fs::symlink_metadata(target)) {
        (Expected::Absent, Err(error)) if error.kind() == ErrorKind::NotFound => Ok(true),
        (Expected::Absent, _) => Ok(false),
        (
            Expected::File {
                disk_path,
                fingerprint: scanned,
                id,
            },
            Ok(metadata),
        ) => {
            if !metadata.is_file() || fingerprint(&metadata) != *scanned {
                return Ok(false);
            }
            let bytes = match read_file(root, disk_path, MAX_FILE_BYTES) {
                Ok(bytes) => bytes,
                Err(_) => return Ok(false),
            };
            Ok(keys.object_id(&bytes)? == *id)
        }
        (Expected::File { .. }, Err(error)) if error.kind() == ErrorKind::NotFound => Ok(false),
        (Expected::File { .. }, Err(error)) => Err(error.into()),
    }
}

fn write_temporary(temporary: &Path, staged: &Path, mode: u32, modified_ms: i64) -> AppResult<()> {
    let bytes = fs::read(staged)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)?;
    file.write_all(&bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    if let Ok(milliseconds) = u64::try_from(modified_ms) {
        file.set_modified(UNIX_EPOCH + Duration::from_millis(milliseconds))?;
    }
    file.sync_all()?;
    Ok(())
}

fn folded(name: &str) -> String {
    nfc(&nfc(name).to_lowercase())
}

/// Directory listings consulted for collision checks during one apply pass.
#[derive(Default)]
pub(crate) struct DirectoryCache {
    listings: HashMap<PathBuf, Vec<String>>,
}

impl DirectoryCache {
    fn names(&mut self, directory: &Path) -> AppResult<&[String]> {
        if !self.listings.contains_key(directory) {
            let mut names = Vec::new();
            match fs::read_dir(directory) {
                Ok(items) => {
                    for item in items {
                        names.push(item?.file_name().to_string_lossy().into_owned());
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            self.listings.insert(directory.to_path_buf(), names);
        }
        Ok(self
            .listings
            .get(directory)
            .map(Vec::as_slice)
            .unwrap_or_default())
    }

    fn forget(&mut self, directory: &Path) {
        self.listings.remove(directory);
    }
}

/// Resolves a new file's path, refusing a component that differs from an
/// existing sibling only by case or Unicode normalization. Listings are
/// compared exactly, so case-insensitive file systems are handled too.
fn free_path(
    root: &Path,
    key_path: &str,
    cache: &mut DirectoryCache,
) -> AppResult<Result<PathBuf, String>> {
    let mut directory = root.to_path_buf();
    let mut relative = PathBuf::new();
    let mut exists = true;
    for part in key_path.split('/') {
        if exists {
            let wanted = folded(part);
            let names = cache.names(&directory)?;
            if !names.iter().any(|name| name == part) {
                if let Some(name) = names.iter().find(|name| folded(name) == wanted) {
                    return Ok(Err(format!(
                        "the name collides with the existing \"{name}\" on this device"
                    )));
                }
                exists = false;
            }
        }
        relative.push(part);
        directory.push(part);
    }
    Ok(Ok(relative))
}

fn create_parents(root: &Path, relative: &Path, cache: &mut DirectoryCache) -> AppResult<PathBuf> {
    let mut current = root.to_path_buf();
    let parent = relative.parent().unwrap_or(Path::new(""));
    for component in parent.components() {
        let Component::Normal(part) = component else {
            return Err(AppError::InvalidPath("A sync path leaves its vault".into()));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(AppError::InvalidPath(
                    "A sync folder is occupied by a file or link".into(),
                ));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => match fs::create_dir(&current) {
                Ok(()) => {
                    if let Some(parent) = current.parent() {
                        cache.forget(parent);
                    }
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(&current)?;
                    if !metadata.is_dir() || metadata.file_type().is_symlink() {
                        return Err(AppError::InvalidPath(
                            "A sync folder is occupied by a file or link".into(),
                        ));
                    }
                }
                Err(error) => return Err(error.into()),
            },
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

/// Removes folders a remote deletion emptied, never the vault root or `.denote`.
fn remove_empty_parents(root: &Path, relative: &Path, cache: &mut DirectoryCache) {
    if let Some(parent) = relative.parent() {
        cache.forget(&root.join(parent));
    }
    let mut current = relative.parent();
    while let Some(parent) = current {
        if parent.as_os_str().is_empty() || parent == Path::new(".denote") {
            break;
        }
        if fs::remove_dir(root.join(parent)).is_err() {
            break;
        }
        if let Some(grandparent) = parent.parent() {
            cache.forget(&root.join(grandparent));
        }
        current = parent.parent();
    }
}

/// Names Windows cannot represent are skipped there instead of being mangled.
fn platform_name_issue(key_path: &str) -> Option<&'static str> {
    if !cfg!(windows) {
        return None;
    }
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    for part in key_path.split('/') {
        let stem = part.split('.').next().unwrap_or(part);
        if RESERVED.iter().any(|name| stem.eq_ignore_ascii_case(name))
            || part.ends_with('.')
            || part.ends_with(' ')
            || part
                .chars()
                .any(|character| matches!(character, '<' | '>' | '"' | '|' | '?' | '*'))
        {
            return Some("the name is not valid on Windows");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::crypto::test_keys;

    fn staged(directory: &Path, bytes: &[u8]) -> PathBuf {
        let path = directory.join(format!("staged-{}", Uuid::new_v4()));
        fs::write(&path, bytes).unwrap();
        path
    }

    fn expected_for(root: &Path, relative: &str) -> Expected {
        let metadata = fs::metadata(root.join(relative)).unwrap();
        Expected::File {
            disk_path: PathBuf::from(relative),
            fingerprint: fingerprint(&metadata),
            id: test_keys()
                .object_id(&fs::read(root.join(relative)).unwrap())
                .unwrap(),
        }
    }

    #[test]
    fn writes_replace_and_remove_only_after_revalidation() {
        let keys = test_keys();
        let vault = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = vault.path();
        let new = staged(staging.path(), b"remote content");
        let write = |expected: &Expected, staged: &Path| {
            apply_file(
                root,
                "notes/deep/a.md",
                expected,
                Change::Write {
                    staged,
                    mode: 0o644,
                    modified_ms: 1_700_000_000_000,
                },
                &keys,
                &mut DirectoryCache::default(),
            )
            .unwrap()
        };
        assert!(matches!(
            write(&Expected::Absent, &new),
            Outcome::Applied(Some(_))
        ));
        assert_eq!(
            fs::read(root.join("notes/deep/a.md")).unwrap(),
            b"remote content"
        );
        assert!(fs::read_dir(root.join("notes/deep")).unwrap().count() == 1);

        // A file created since the scan is never replaced.
        assert!(matches!(
            write(&Expected::Absent, &new),
            Outcome::Skipped(_)
        ));

        let expected = expected_for(root, "notes/deep/a.md");
        fs::write(root.join("notes/deep/a.md"), b"edited locally!").unwrap();
        assert!(matches!(write(&expected, &new), Outcome::Skipped(_)));
        assert_eq!(
            fs::read(root.join("notes/deep/a.md")).unwrap(),
            b"edited locally!"
        );

        let expected = expected_for(root, "notes/deep/a.md");
        let next = staged(staging.path(), b"second remote");
        assert!(matches!(write(&expected, &next), Outcome::Applied(Some(_))));
        assert_eq!(
            fs::read(root.join("notes/deep/a.md")).unwrap(),
            b"second remote"
        );

        let expected = expected_for(root, "notes/deep/a.md");
        assert_eq!(
            apply_file(
                root,
                "notes/deep/a.md",
                &expected,
                Change::Remove,
                &keys,
                &mut DirectoryCache::default()
            )
            .unwrap(),
            Outcome::Applied(None)
        );
        assert!(!root.join("notes").exists(), "emptied folders are removed");
    }

    #[test]
    fn refuses_excluded_paths_links_and_case_collisions() {
        let keys = test_keys();
        let vault = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = vault.path();
        let bytes = staged(staging.path(), b"x");
        let write = |path: &str| {
            apply_file(
                root,
                path,
                &Expected::Absent,
                Change::Write {
                    staged: &bytes,
                    mode: 0o644,
                    modified_ms: 0,
                },
                &keys,
                &mut DirectoryCache::default(),
            )
        };
        for path in [
            ".git/config",
            "a/.git/hooks/post-commit",
            ".denote/trash/a.md",
            "../escape",
        ] {
            assert!(
                matches!(write(path).unwrap(), Outcome::Skipped(_)),
                "{path}"
            );
        }
        fs::create_dir(root.join("Notes")).unwrap();
        assert!(matches!(write("notes/a.md").unwrap(), Outcome::Skipped(_)));
        fs::write(root.join("Readme.md"), b"local").unwrap();
        assert!(matches!(write("README.md").unwrap(), Outcome::Skipped(_)));
        assert!(matches!(write("Notes/a.md").unwrap(), Outcome::Applied(_)));
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), root.join("linked")).unwrap();
            assert!(write("linked/a.md").is_err());
            assert!(!outside.path().join("a.md").exists());
        }
        assert!(
            !fs::read_dir(root.join("Notes")).unwrap().any(|item| item
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(TEMPORARY_SUFFIX)),
            "no temporary files remain"
        );
    }
}
