//! Walks enrolled vault folders. See `docs/sync.md` ("Scope matrix" and step 3).

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{ErrorKind, Read},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::UNIX_EPOCH,
};

use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use super::{crypto::MAX_FILE_BYTES, model::valid_relative_path, store::Fingerprint};
use crate::{
    crypto::{EncryptionPhase, load_manifest},
    error::{AppError, AppResult},
};

/// Bounds one vault's walk; larger vaults are reported instead of synced.
pub(crate) const MAX_VAULT_FILES: usize = 500_000;
/// Name suffix of temporary files sync writes beside their destination.
pub(crate) const TEMPORARY_SUFFIX: &str = ".denote-sync";
const CANCEL_CHECK_INTERVAL: usize = 256;

pub(crate) struct LocalFile {
    /// Path relative to the vault root as it exists on disk.
    pub disk_path: PathBuf,
    pub fingerprint: Fingerprint,
    pub mode: u32,
    pub modified_ms: i64,
}

#[derive(Default)]
pub(crate) struct VaultWalk {
    /// Regular files by NFC relative path.
    pub files: BTreeMap<String, LocalFile>,
    /// Paths that exist but cannot be synced right now. They never become tombstones.
    pub held: BTreeSet<String>,
    /// False when part of the tree could not be read; nothing is tombstoned then.
    pub complete: bool,
    pub skipped: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Availability {
    Ready,
    Unavailable(String),
    /// Encryption is being enabled or disabled.
    Transition,
}

/// A missing or unreadable root, or a vault mid-encryption, is skipped entirely.
pub(crate) fn availability(root: &Path) -> Availability {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Availability::Unavailable("The vault folder is not a regular folder.".into());
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Availability::Unavailable("The vault folder is missing.".into());
        }
        Err(_) => return Availability::Unavailable("The vault folder is unreadable.".into()),
    }
    if fs::read_dir(root).is_err() {
        return Availability::Unavailable("The vault folder is unreadable.".into());
    }
    match load_manifest(root) {
        Ok(Some(manifest))
            if matches!(
                manifest.phase,
                EncryptionPhase::Encrypting | EncryptionPhase::Decrypting
            ) =>
        {
            Availability::Transition
        }
        Ok(_) => Availability::Ready,
        Err(_) => Availability::Unavailable("The vault encryption manifest is unreadable.".into()),
    }
}

fn eq_ignore_case(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn is_junk_name(name: &str) -> bool {
    const JUNK: [&str; 10] = [
        ".DS_Store",
        "Thumbs.db",
        "ehthumbs.db",
        "desktop.ini",
        ".Spotlight-V100",
        ".Trashes",
        ".fseventsd",
        ".TemporaryItems",
        "$RECYCLE.BIN",
        "System Volume Information",
    ];
    JUNK.iter().any(|junk| eq_ignore_case(name, junk))
        || name.starts_with("._")
        || name.starts_with("~$")
        || (name.starts_with(".~lock.") && name.ends_with('#'))
}

fn is_temporary_name(name: &str) -> bool {
    [
        ".denote-tmp",
        ".denote-create",
        ".denote-backup",
        TEMPORARY_SUFFIX,
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
}

/// Device-local paths sync never reads or writes: Git metadata at any depth,
/// Denote's trash, locks, and journals, temporary files, and OS junk.
pub(crate) fn is_excluded(relative: &str) -> bool {
    let parts: Vec<&str> = relative.split('/').collect();
    if parts.iter().any(|part| eq_ignore_case(part, ".git")) {
        return true;
    }
    if parts.iter().any(|part| is_junk_name(part)) {
        return true;
    }
    if parts.last().is_some_and(|name| is_temporary_name(name)) {
        return true;
    }
    matches!(
        parts.as_slice(),
        [denote, local, ..]
            if eq_ignore_case(denote, ".denote")
                && ["trash", "locks", "speech-attachments", "speech-pending.json"]
                    .iter()
                    .any(|name| eq_ignore_case(local, name))
    )
}

/// `.name.XXXXXX` beside `name`: an atomic write in progress.
fn is_atomic_write_temporary(directory: &Path, name: &str) -> bool {
    let Some(stem) = name.strip_prefix('.') else {
        return false;
    };
    let Some((base, suffix)) = stem.rsplit_once('.') else {
        return false;
    };
    !base.is_empty()
        && suffix.len() == 6
        && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && directory.join(base).exists()
}

pub(crate) fn nfc(value: &str) -> String {
    value.nfc().collect()
}

pub(crate) fn fingerprint(metadata: &fs::Metadata) -> Fingerprint {
    Fingerprint {
        size: metadata.len(),
        mtime_ns: modified_ns(metadata),
    }
}

fn modified_ns(metadata: &fs::Metadata) -> i64 {
    match metadata.modified() {
        Ok(time) => match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
            Err(error) => i64::try_from(error.duration().as_nanos())
                .map(|value| -value)
                .unwrap_or(i64::MIN),
        },
        Err(_) => 0,
    }
}

pub(crate) fn file_mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            return 0o755;
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    0o644
}

/// Walks `root` without following links.
pub(crate) fn walk_vault(root: &Path, cancel: &AtomicBool) -> AppResult<VaultWalk> {
    let mut walk = VaultWalk {
        complete: true,
        ..VaultWalk::default()
    };
    let walker = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|item| {
            if item.depth() == 0 {
                return true;
            }
            let Ok(relative) = item.path().strip_prefix(root) else {
                return false;
            };
            !relative
                .to_str()
                .is_some_and(|relative| is_excluded(&nfc(&relative.replace('\\', "/"))))
        });
    for (position, item) in walker.enumerate() {
        if position % CANCEL_CHECK_INTERVAL == 0 && cancel.load(Ordering::SeqCst) {
            return Err(cancelled());
        }
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                if error.depth() == 0 {
                    return Err(AppError::NotFound("The vault folder is unreadable.".into()));
                }
                walk.complete = false;
                walk.skipped.push(format!(
                    "{}: unreadable folder",
                    error
                        .path()
                        .and_then(|path| path.strip_prefix(root).ok())
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "(part of the vault)".into())
                ));
                continue;
            }
        };
        if item.depth() == 0 || item.file_type().is_dir() {
            continue;
        }
        let Ok(relative) = item.path().strip_prefix(root) else {
            walk.complete = false;
            continue;
        };
        let Some(relative_text) = relative.to_str() else {
            walk.skipped.push(format!(
                "{}: the name is not valid Unicode",
                relative.to_string_lossy()
            ));
            walk.complete = false;
            continue;
        };
        let key = nfc(&relative_text.replace('\\', "/"));
        if !valid_relative_path(&key) {
            walk.skipped
                .push(format!("{key}: the name cannot be synced"));
            continue;
        }
        let name = item.file_name().to_string_lossy();
        if item.file_type().is_file()
            && item
                .path()
                .parent()
                .is_some_and(|parent| is_atomic_write_temporary(parent, &name))
        {
            walk.held.insert(key);
            continue;
        }
        if !item.file_type().is_file() {
            // Links, sockets, and devices are never followed, created, or deleted.
            walk.held.insert(key.clone());
            if item.file_type().is_symlink() {
                walk.skipped
                    .push(format!("{key}: symbolic links are not synced"));
            }
            continue;
        }
        let metadata = match item.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                walk.held.insert(key.clone());
                walk.skipped.push(format!("{key}: unreadable"));
                continue;
            }
        };
        if metadata.len() > MAX_FILE_BYTES {
            walk.held.insert(key.clone());
            walk.skipped.push(format!(
                "{key}: larger than the {} MiB sync limit",
                MAX_FILE_BYTES / (1024 * 1024)
            ));
            continue;
        }
        if walk.files.contains_key(&key) || walk.held.contains(&key) {
            walk.skipped.push(format!(
                "{key}: another file has the same name after Unicode normalization"
            ));
            continue;
        }
        if walk.files.len() >= MAX_VAULT_FILES {
            return Err(AppError::InvalidData(format!(
                "The vault has more than {MAX_VAULT_FILES} files, which exceeds the sync limit"
            )));
        }
        let fingerprint = fingerprint(&metadata);
        walk.files.insert(
            key,
            LocalFile {
                disk_path: relative.to_path_buf(),
                fingerprint,
                mode: file_mode(&metadata),
                modified_ms: fingerprint.mtime_ns / 1_000_000,
            },
        );
    }
    Ok(walk)
}

pub(crate) fn cancelled() -> AppError {
    AppError::State("Sync was cancelled.".into())
}

/// Joins `relative` under `root`, refusing any existing link along the way.
/// Missing trailing components are allowed.
pub(crate) fn confined(root: &Path, relative: &Path) -> AppResult<PathBuf> {
    let mut current = root.to_path_buf();
    let mut missing = false;
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(AppError::InvalidPath("A sync path leaves its vault".into()));
        };
        current.push(part);
        if missing {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AppError::InvalidPath(
                    "A sync path crosses a symbolic link".into(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => missing = true,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

/// Reads a regular file under `root`, at most `limit` bytes.
pub(crate) fn read_file(root: &Path, relative: &Path, limit: u64) -> AppResult<Vec<u8>> {
    let path = confined(root, relative)?;
    let file = fs::File::open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(AppError::InvalidData("Not a regular file".into()));
    }
    if metadata.len() > limit {
        return Err(AppError::InvalidData(format!(
            "Larger than the {} MiB sync limit",
            limit / (1024 * 1024)
        )));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(AppError::InvalidData(format!(
            "Larger than the {} MiB sync limit",
            limit / (1024 * 1024)
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_cover_git_trash_locks_temporary_files_and_os_junk() {
        for path in [
            ".git/config",
            "project/.git",
            "project/.GIT/HEAD",
            ".denote/trash/x/a.md",
            ".denote/locks/vault.lock",
            ".denote/speech-attachments",
            ".denote/speech-pending.json",
            "notes/.DS_Store",
            "notes/._a.md",
            "Thumbs.db",
            "notes/.a.md.1234.denote-tmp",
            "notes/.a.md.denote-sync",
            "~$report.docx",
            ".~lock.sheet.ods#",
        ] {
            assert!(is_excluded(path), "{path}");
        }
        for path in [
            "notes/a.md",
            ".denote/encryption.json",
            ".denote/note-comments.json",
            ".denote/credentials.dncred",
            "notes/a.sync-conflict-1234abcd.md",
            ".gitignore",
            "git/notes.md",
        ] {
            assert!(!is_excluded(path), "{path}");
        }
    }

    #[test]
    fn walks_without_following_links_and_normalizes_names() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("notes/.git")).unwrap();
        fs::create_dir_all(root.join(".denote/trash/1")).unwrap();
        fs::write(root.join("notes/a.md"), b"a").unwrap();
        fs::write(root.join("notes/.git/HEAD"), b"ref").unwrap();
        fs::write(root.join(".denote/trash/1/old.md"), b"old").unwrap();
        fs::write(root.join(".denote/encryption.json"), b"{}").unwrap();
        fs::write(root.join("Cafe\u{301}.md"), b"nfd").unwrap();
        fs::write(root.join("notes/a.md.bak"), b"x").unwrap();
        fs::write(root.join("notes/.a.md.Ab12Cd"), b"in-flight").unwrap();
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("secret.md"), b"secret").unwrap();
            std::os::unix::fs::symlink(outside.path(), root.join("linked")).unwrap();
            std::os::unix::fs::symlink(outside.path().join("secret.md"), root.join("l.md"))
                .unwrap();
        }
        let walk = walk_vault(root, &AtomicBool::new(false)).unwrap();
        let keys: Vec<&str> = walk.files.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                ".denote/encryption.json",
                "Café.md",
                "notes/a.md",
                "notes/a.md.bak"
            ]
        );
        assert!(walk.complete);
        assert!(walk.held.contains("notes/.a.md.Ab12Cd"));
        #[cfg(unix)]
        {
            assert!(walk.held.contains("linked") && walk.held.contains("l.md"));
            assert!(confined(root, Path::new("linked/secret.md")).is_err());
            assert!(read_file(root, Path::new("l.md"), 1024).is_err());
        }
        let file = &walk.files["Café.md"];
        assert_eq!(read_file(root, &file.disk_path, 1024).unwrap(), b"nfd");
        assert!(read_file(root, Path::new("notes/a.md"), 0).is_err());
        assert!(walk_vault(root, &AtomicBool::new(true)).is_err());
    }

    #[test]
    fn missing_roots_and_encryption_transitions_are_skipped() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            availability(&directory.path().join("missing")),
            Availability::Unavailable(_)
        ));
        assert_eq!(availability(directory.path()), Availability::Ready);
        fs::create_dir(directory.path().join(".denote")).unwrap();
        fs::write(
            directory.path().join(".denote/encryption.json"),
            b"not json",
        )
        .unwrap();
        assert!(matches!(
            availability(directory.path()),
            Availability::Unavailable(_)
        ));
    }
}
