//! Folder backend: a host-selected absolute folder, e.g. a mounted or synced drive.

use std::{
    fs,
    io::{ErrorKind, Read, Write},
    path::{Component, Path, PathBuf},
};

use uuid::Uuid;
use walkdir::WalkDir;

use crate::{
    error::{AppError, AppResult},
    sync::backend::{Backend, PutMode, PutOutcome, REMOTE_PREFIX, validate_key, validate_prefix},
};

const MAX_VALUE_BYTES: u64 = 300 * 1024 * 1024;
const TEMPORARY_DIRECTORY: &str = "denote-sync/.tmp";

pub(crate) struct FolderBackend {
    root: PathBuf,
}

impl FolderBackend {
    /// `root` must be an existing absolute folder that is not a symbolic link.
    pub(crate) fn open(root: &Path) -> AppResult<Self> {
        validate_folder(root)?;
        Ok(Self { root: root.to_path_buf() })
    }

    fn data_root(&self) -> PathBuf {
        self.root.join(REMOTE_PREFIX.trim_end_matches('/'))
    }

    fn path(&self, key: &str) -> AppResult<PathBuf> {
        validate_key(key)?;
        let path = self.data_root().join(key);
        self.ensure_no_links(&path)?;
        Ok(path)
    }

    fn ensure_no_links(&self, path: &Path) -> AppResult<()> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| AppError::InvalidPath("Sync storage path escaped its folder".into()))?;
        let mut current = self.root.clone();
        for component in relative.components() {
            let Component::Normal(part) = component else {
                return Err(AppError::InvalidPath("Invalid sync storage path".into()));
            };
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(AppError::InvalidPath(
                        "The sync folder contains a symbolic link, which is not supported".into(),
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn temporary(&self) -> AppResult<PathBuf> {
        let directory = self.root.join(TEMPORARY_DIRECTORY);
        self.ensure_no_links(&directory)?;
        fs::create_dir_all(&directory)?;
        Ok(directory.join(format!("{}.tmp", Uuid::new_v4())))
    }

    fn ensure_root(&self) -> AppResult<()> {
        validate_folder(&self.root).map_err(|_| {
            AppError::NotFound("The sync folder is unavailable. Reconnect the drive or choose it again.".into())
        })
    }
}

fn validate_folder(root: &Path) -> AppResult<()> {
    if !root.is_absolute() {
        return Err(AppError::InvalidPath("The sync folder must be an absolute path".into()));
    }
    let metadata = fs::symlink_metadata(root)
        .map_err(|_| AppError::NotFound("The sync folder does not exist".into()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::InvalidPath(
            "The sync folder must be a real folder, not a file or symbolic link".into(),
        ));
    }
    Ok(())
}

impl Backend for FolderBackend {
    fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        self.ensure_root()?;
        let path = self.path(key)?;
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            return Err(AppError::InvalidData(format!("Sync storage entry {key} is not a file")));
        }
        let mut bytes = Vec::new();
        file.take(MAX_VALUE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_VALUE_BYTES {
            return Err(AppError::InvalidData(format!("Sync storage entry {key} is too large")));
        }
        Ok(Some(bytes))
    }

    fn put(&self, key: &str, bytes: &[u8], mode: PutMode) -> AppResult<PutOutcome> {
        self.ensure_root()?;
        let path = self.path(key)?;
        if mode == PutMode::CreateOnly && fs::symlink_metadata(&path).is_ok() {
            return Ok(PutOutcome::AlreadyExists);
        }
        let parent = path
            .parent()
            .ok_or_else(|| AppError::InvalidPath("Invalid sync storage path".into()))?;
        fs::create_dir_all(parent)?;
        self.ensure_no_links(&path)?;
        let temporary = self.temporary()?;
        let written = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        let result = match mode {
            PutMode::Overwrite => fs::rename(&temporary, &path).map(|()| PutOutcome::Written),
            PutMode::CreateOnly => match crate::vault::rename_no_replace(&temporary, &path) {
                Ok(()) => Ok(PutOutcome::Written),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(PutOutcome::AlreadyExists),
                Err(error) => Err(error),
            },
        };
        if !matches!(result, Ok(PutOutcome::Written)) {
            let _ = fs::remove_file(&temporary);
        }
        Ok(result?)
    }

    fn list(&self, prefix: &str) -> AppResult<Vec<String>> {
        validate_prefix(prefix)?;
        self.ensure_root()?;
        let data_root = self.data_root();
        // Walk from the deepest complete directory in the prefix, then filter.
        let directory_part = match prefix.rfind('/') {
            Some(index) => &prefix[..index],
            None => "",
        };
        let start = if directory_part.is_empty() { data_root.clone() } else { data_root.join(directory_part) };
        self.ensure_no_links(&start)?;
        if !start.exists() {
            return Ok(Vec::new());
        }
        let mut keys = Vec::new();
        for item in WalkDir::new(&start).follow_links(false) {
            let item = item.map_err(|error| AppError::Io(error.into()))?;
            if item.file_type().is_symlink() {
                return Err(AppError::InvalidPath(
                    "The sync folder contains a symbolic link, which is not supported".into(),
                ));
            }
            if !item.file_type().is_file() {
                continue;
            }
            let relative = item
                .path()
                .strip_prefix(&data_root)
                .map_err(|_| AppError::InvalidPath("Sync storage path escaped its folder".into()))?;
            let Some(key) = relative.to_str().map(|value| value.replace('\\', "/")) else {
                continue;
            };
            if key.starts_with(prefix) && validate_key(&key).is_ok() {
                keys.push(key);
            }
        }
        Ok(keys)
    }

    fn delete(&self, key: &str) -> AppResult<()> {
        self.ensure_root()?;
        let path = self.path(key)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_backend_meets_the_contract() {
        let directory = tempfile::tempdir().unwrap();
        crate::sync::backend_tests::conformance(&FolderBackend::open(directory.path()).unwrap());
        assert!(!directory.path().join(TEMPORARY_DIRECTORY).read_dir().unwrap().any(|_| true));
    }

    #[test]
    fn folder_backend_rejects_relative_missing_and_linked_folders() {
        assert!(FolderBackend::open(Path::new("relative")).is_err());
        let directory = tempfile::tempdir().unwrap();
        assert!(FolderBackend::open(&directory.path().join("missing")).is_err());
        #[cfg(unix)]
        {
            let target = directory.path().join("target");
            fs::create_dir(&target).unwrap();
            let link = directory.path().join("link");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(FolderBackend::open(&link).is_err());

            let backend = FolderBackend::open(&target).unwrap();
            fs::create_dir_all(target.join("denote-sync/v1")).unwrap();
            let outside = directory.path().join("outside");
            fs::create_dir(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, target.join("denote-sync/v1/objects")).unwrap();
            assert!(backend.put("objects/ab/x", b"x", PutMode::Overwrite).is_err());
            assert!(backend.list("objects/").is_err());
            assert!(!outside.join("ab").exists());
        }
    }

    #[test]
    fn folder_backend_reports_a_disconnected_drive_as_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("drive");
        fs::create_dir(&root).unwrap();
        let backend = FolderBackend::open(&root).unwrap();
        fs::remove_dir(&root).unwrap();
        assert!(backend.list("heads/").is_err());
        assert!(backend.get("repository.json").is_err());
    }
}
