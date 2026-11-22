//! Release-hardening coverage for incompatible, corrupted, interrupted, and
//! rolled-back plugin packages. Every archive, note, and manifest is synthetic.

use std::{collections::BTreeMap, fs, path::Path};

use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};
use tar::Builder;
use tempfile::TempDir;

use super::{
    catalog::{catalog_fingerprint, compatibility_error_for_host},
    sandbox::current_platform,
    tests::{catalog, manager, package_bytes},
    *,
};

fn with_artifact(mut catalog: PluginCatalogEntry, bytes: &[u8]) -> PluginCatalogEntry {
    catalog.artifact.size_bytes = bytes.len() as u64;
    catalog.artifact.sha256 = hex::encode(Sha256::digest(bytes));
    catalog
}

fn versioned(version: &str) -> PluginCatalogEntry {
    let mut entry = catalog();
    entry.manifest.version = version.to_string();
    entry
}

/// Builds a gzip tar whose entry names are written verbatim, so hostile
/// paths that `tar::Builder` would refuse can still be exercised.
fn raw_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for (path, content) in entries {
        let mut header = tar::Header::new_old();
        let name = &mut header.as_old_mut().name;
        name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, *content).expect("append raw entry");
    }
    builder
        .into_inner()
        .expect("archive")
        .finish()
        .expect("gzip")
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).expect("gzip payload");
    encoder.finish().expect("gzip")
}

fn manifest_bytes(manifest: &PluginManifest) -> Vec<u8> {
    serde_json::to_vec(manifest).expect("manifest")
}

fn package_with(manifest: &PluginManifest, entrypoint: Option<&[u8]>) -> Vec<u8> {
    let manifest_json = manifest_bytes(manifest);
    let mut entries: Vec<(&str, &[u8])> = vec![("plugin.json", &manifest_json)];
    if let Some(entrypoint) = entrypoint {
        entries.push((manifest.entrypoint.as_str(), entrypoint));
    }
    raw_archive(&entries)
}

fn record_enabled(
    manager: &PluginManager,
    installed: &PluginCatalogEntry,
    entrypoint_hash: String,
) {
    let id = installed.manifest.id.clone();
    manager
        .update_state(|state| {
            state.enabled.insert(id.clone());
            state.approved_permissions.insert(
                id.clone(),
                installed.manifest.permissions.iter().cloned().collect(),
            );
            state
                .artifact_hashes
                .insert(id.clone(), installed.artifact.sha256.clone());
            state
                .catalog_fingerprints
                .insert(id.clone(), catalog_fingerprint(installed)?);
            state.entrypoint_hashes.insert(id.clone(), entrypoint_hash);
            state
                .installed_manifests
                .insert(id.clone(), installed.manifest.clone());
            Ok(())
        })
        .expect("record enabled plugin");
}

fn directory_names(path: &Path) -> Vec<String> {
    if !path.exists() {
        return vec![];
    }
    let mut names: Vec<String> = fs::read_dir(path)
        .expect("read directory")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn version_api_and_platform_bounds_refuse_incompatible_plugins() {
    let compatible = catalog();
    let error = |mutate: &dyn Fn(&mut PluginCatalogEntry), host: &str| {
        let mut entry = compatible.clone();
        mutate(&mut entry);
        compatibility_error_for_host(&entry, host)
    };

    assert!(
        error(
            &|entry| entry.manifest.compatibility.minimum_denote_version = "9.0.0".into(),
            "1.0.0",
        )
        .is_some_and(|message| message.contains("requires Denote 9.0.0 or newer"))
    );
    // A release candidate sorts before its release, so a plugin that requires
    // the final version must wait for it.
    assert!(
        error(
            &|entry| entry.manifest.compatibility.minimum_denote_version = "1.0.0".into(),
            "1.0.0-rc.1",
        )
        .is_some_and(|message| message.contains("requires Denote 1.0.0 or newer"))
    );
    assert_eq!(
        error(
            &|entry| entry.manifest.compatibility.minimum_denote_version = "1.0.0-rc.1".into(),
            "1.0.0-rc.1",
        ),
        None
    );
    // The maximum bound is exclusive.
    assert!(
        error(
            &|entry| entry.manifest.compatibility.maximum_denote_version = Some("1.0.0".into()),
            "1.0.0",
        )
        .is_some_and(|message| message.contains("below 1.0.0"))
    );
    assert_eq!(
        error(
            &|entry| entry.manifest.compatibility.maximum_denote_version = Some("2.0.0".into()),
            "1.99.0",
        ),
        None
    );
    assert!(
        error(
            &|entry| entry.manifest.compatibility.api_version = 2,
            "1.0.0"
        )
        .is_some_and(|message| message.contains("requires API version 2"))
    );
    assert!(
        error(
            &|entry| entry.manifest.compatibility.minimum_denote_version = "not-a-version".into(),
            "1.0.0",
        )
        .is_some_and(|message| message.contains("invalid minimum Denote version"))
    );
    let unsupported_platform = |entry: &mut PluginCatalogEntry| {
        let other = ["macos", "linux", "windows"]
            .into_iter()
            .find(|platform| *platform != current_platform())
            .expect("another platform");
        entry.manifest.permissions.push(PluginPermission {
            capability: "process".to_string(),
            hosts: vec![],
            executables: BTreeMap::from([(other.to_string(), vec!["/synthetic/tool".to_string()])]),
        });
    };
    assert!(
        error(&unsupported_platform, "1.0.0")
            .is_some_and(|message| message.contains("does not support process execution"))
    );
}

#[test]
fn incompatible_catalog_entry_is_refused_before_any_download_or_install() {
    let data = TempDir::new().expect("data");
    let cache = TempDir::new().expect("cache");
    let mut entry = catalog();
    entry.manifest.compatibility.minimum_denote_version = "99.0.0".to_string();
    let manager = manager(entry.clone(), &data, &cache);

    let error = manager
        .prepare(&entry.manifest.id, entry.manifest.permissions.clone())
        .expect_err("incompatible plugin");

    assert!(
        error
            .to_string()
            .contains("requires Denote 99.0.0 or newer")
    );
    assert!(!manager.plugin_root(&entry.manifest.id).exists());
    assert!(directory_names(&cache.path().join("plugin-downloads")).is_empty());
    assert!(
        manager
            .pending_transactions()
            .expect("transactions")
            .is_empty()
    );
    assert!(
        !manager
            .state()
            .expect("state")
            .enabled
            .contains(&entry.manifest.id)
    );
}

#[test]
fn incompatible_installed_manifest_is_disabled_and_removed_at_startup() {
    let data = TempDir::new().expect("data");
    let cache = TempDir::new().expect("cache");
    let mut installed = catalog();
    installed.manifest.compatibility.minimum_denote_version = "99.0.0".to_string();
    let bytes = package_bytes(&installed);
    let installed = with_artifact(installed, &bytes);
    let manager = manager(catalog(), &data, &cache);
    let hash = manager
        .install_package(&installed, &bytes)
        .expect("install synthetic package");
    record_enabled(&manager, &installed, hash);

    manager.reconcile_packages().expect("startup recovery");

    let id = &installed.manifest.id;
    let state = manager.state().expect("state");
    assert!(!state.enabled.contains(id));
    assert!(!state.installed_manifests.contains_key(id));
    assert!(state.approved_permissions.contains_key(id));
    assert!(
        state
            .errors
            .get(id)
            .is_some_and(|error| error.contains("incompatible"))
    );
    drop(state);
    assert!(!manager.plugin_root(id).exists());
}

#[test]
fn corrupted_packages_are_rejected_without_partial_installation_or_escape() {
    let base = catalog();
    let entrypoint: &[u8] = b"export default {};";
    let mut wrong_id = base.manifest.clone();
    wrong_id.id = "denote.synthetic-other".to_string();
    let mut wrong_version = base.manifest.clone();
    wrong_version.version = "9.9.9".to_string();
    let mut wrong_permissions = base.manifest.clone();
    wrong_permissions.permissions.push(PluginPermission {
        capability: "network".to_string(),
        hosts: vec!["synthetic.invalid".to_string()],
        executables: BTreeMap::new(),
    });
    let valid = package_bytes(&base);
    let manifest = manifest_bytes(&base.manifest);

    let cases: Vec<(&str, Vec<u8>, bool, &str)> = vec![
        ("size mismatch", valid.clone(), true, "size does not match"),
        (
            "truncated gzip",
            valid[..valid.len() / 2].to_vec(),
            false,
            "",
        ),
        (
            "not gzip",
            b"synthetic bytes that are not an archive".to_vec(),
            false,
            "plugin package",
        ),
        ("invalid tar", gzip(&[0x5a; 1536]), false, "plugin package"),
        (
            "parent traversal",
            raw_archive(&[("plugin.json", &manifest), ("../escape.js", entrypoint)]),
            false,
            "",
        ),
        (
            "absolute path",
            raw_archive(&[("plugin.json", &manifest), ("/escape.js", entrypoint)]),
            false,
            "",
        ),
        (
            "missing manifest",
            raw_archive(&[(base.manifest.entrypoint.as_str(), entrypoint)]),
            false,
            "",
        ),
        (
            "invalid manifest",
            raw_archive(&[
                ("plugin.json", b"{not json"),
                (base.manifest.entrypoint.as_str(), entrypoint),
            ]),
            false,
            "Invalid packaged plugin manifest",
        ),
        (
            "manifest ID mismatch",
            package_with(&wrong_id, Some(entrypoint)),
            false,
            "does not match catalog metadata",
        ),
        (
            "manifest version mismatch",
            package_with(&wrong_version, Some(entrypoint)),
            false,
            "does not match catalog metadata",
        ),
        (
            "manifest permission mismatch",
            package_with(&wrong_permissions, Some(entrypoint)),
            false,
            "does not match catalog metadata",
        ),
        (
            "missing entrypoint",
            package_with(&base.manifest, None),
            false,
            "",
        ),
    ];

    for (label, bytes, wrong_size, expected) in cases {
        let data = TempDir::new().expect("data");
        let cache = TempDir::new().expect("cache");
        let vault = data.path().join("vault");
        fs::create_dir(&vault).expect("vault");
        fs::write(vault.join("Note.md"), "# Invented note\n").expect("note");
        let mut entry = with_artifact(base.clone(), &bytes);
        if wrong_size {
            entry.artifact.size_bytes += 1;
        }
        let manager = manager(entry.clone(), &data, &cache);

        let error = manager
            .install_package(&entry, &bytes)
            .expect_err(label)
            .to_string();

        assert!(error.contains(expected), "{label}: {error}");
        let plugin_root = manager.plugin_root(&entry.manifest.id);
        assert!(!manager.install_dir(&entry).exists(), "{label}");
        assert!(
            directory_names(&plugin_root).is_empty(),
            "{label}: leftover {:?}",
            directory_names(&plugin_root)
        );
        for escaped in [
            data.path().join("escape.js"),
            data.path().join("plugins/escape.js"),
            data.path().join("plugins/packages/escape.js"),
            Path::new("/escape.js").to_path_buf(),
        ] {
            assert!(!escaped.exists(), "{label}: {}", escaped.display());
        }
        assert_eq!(
            fs::read_to_string(vault.join("Note.md")).expect("note"),
            "# Invented note\n",
            "{label}"
        );
    }
}

#[test]
fn failed_update_verification_keeps_the_installed_version_running() {
    let data = TempDir::new().expect("data");
    let cache = TempDir::new().expect("cache");
    let installed_bytes = package_bytes(&versioned("0.0.9"));
    let installed = with_artifact(versioned("0.0.9"), &installed_bytes);
    let latest_bytes = package_bytes(&catalog());
    let latest = with_artifact(catalog(), &latest_bytes);
    let manager = manager(latest.clone(), &data, &cache);
    let hash = manager
        .install_package(&installed, &installed_bytes)
        .expect("installed package");
    record_enabled(&manager, &installed, hash.clone());

    let mut tampered = latest_bytes.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    let checksum = manager
        .install_package(&latest, &tampered)
        .expect_err("tampered update");
    let truncated = manager
        .install_package(&latest, &latest_bytes[..latest_bytes.len() - 1])
        .expect_err("truncated update");

    assert!(checksum.to_string().contains("SHA-256"));
    assert!(truncated.to_string().contains("size does not match"));
    assert!(!manager.install_dir(&latest).exists());
    assert_eq!(
        sha256_file_hex(
            &manager
                .install_dir(&installed)
                .join(&installed.manifest.entrypoint)
        ),
        hash
    );
    manager.reconcile_packages().expect("startup recovery");
    let state = manager.state().expect("state");
    assert!(state.enabled.contains(&installed.manifest.id));
    assert_eq!(
        state
            .installed_manifests
            .get(&installed.manifest.id)
            .map(|manifest| manifest.version.as_str()),
        Some("0.0.9")
    );
    assert!(!state.errors.contains_key(&installed.manifest.id));
}

fn sha256_file_hex(path: &Path) -> String {
    hex::encode(Sha256::digest(fs::read(path).expect("entrypoint")))
}

#[test]
fn interrupted_install_update_and_removal_residue_is_recovered_on_restart() {
    let data = TempDir::new().expect("data");
    let cache = TempDir::new().expect("cache");
    let vault = data.path().join("vault");
    fs::create_dir(&vault).expect("vault");
    fs::write(vault.join("Note.md"), "# Invented note\n").expect("note");

    // The embedded catalog version is the installed, previously approved one.
    let installed_bytes = package_bytes(&catalog());
    let installed = with_artifact(catalog(), &installed_bytes);
    let staged_update_bytes = package_bytes(&versioned("9.0.0"));
    let staged_update = with_artifact(versioned("9.0.0"), &staged_update_bytes);
    let entries: Vec<PluginCatalogEntry> = serde_json::from_str(CATALOG_JSON).expect("catalog");
    let never_committed_bytes = package_bytes(&entries[3]);
    let never_committed = with_artifact(entries[3].clone(), &never_committed_bytes);
    {
        let before_crash = manager(installed.clone(), &data, &cache);
        let hash = before_crash
            .install_package(&installed, &installed_bytes)
            .expect("installed package");
        record_enabled(&before_crash, &installed, hash);
        // Crash after an update was staged beside the running version.
        before_crash
            .install_package(&staged_update, &staged_update_bytes)
            .expect("staged update");
        // Crash after a first-time package was installed but before commit.
        before_crash
            .install_package(&never_committed, &never_committed_bytes)
            .expect("uncommitted install");
    }
    let id = installed.manifest.id.clone();
    let packages = data.path().join("plugins/packages");
    let plugin_root = packages.join(&id);
    // Crash during extraction and during atomic removal.
    fs::create_dir_all(plugin_root.join(".staging-synthetic/dist")).expect("staging");
    fs::write(
        plugin_root.join(".staging-synthetic/dist/index.js"),
        "partial",
    )
    .expect("partial");
    fs::create_dir_all(plugin_root.join(".0.0.1.removing-synthetic")).expect("version backup");
    fs::create_dir_all(packages.join(format!(".{id}.removing-synthetic"))).expect("root backup");
    // Crash during download.
    let downloads = cache.path().join("plugin-downloads").join(&id);
    fs::create_dir_all(&downloads).expect("download cache");
    fs::write(
        downloads.join("9.0.0-synthetic.tgz"),
        &staged_update_bytes[..64],
    )
    .expect("partial");

    let restarted = PluginManager::new(data.path().to_path_buf(), cache.path().to_path_buf());

    assert_eq!(
        directory_names(&plugin_root),
        vec![installed.manifest.version.clone()]
    );
    assert_eq!(
        sha256_file_hex(
            &restarted
                .install_dir(&installed)
                .join(&installed.manifest.entrypoint)
        ),
        restarted.state().expect("state").entrypoint_hashes[&id]
    );
    assert_eq!(directory_names(&packages), vec![id.clone()]);
    assert!(directory_names(&cache.path().join("plugin-downloads")).is_empty());
    let state = restarted.state().expect("state");
    assert!(state.enabled.contains(&id));
    assert!(!state.errors.contains_key(&id));
    assert!(!state.enabled.contains(&never_committed.manifest.id));
    assert!(
        !state
            .installed_manifests
            .contains_key(&never_committed.manifest.id)
    );
    drop(state);
    assert!(
        restarted
            .pending_transactions()
            .expect("transactions")
            .is_empty()
    );
    assert_eq!(
        fs::read_to_string(vault.join("Note.md")).expect("note"),
        "# Invented note\n"
    );
}

#[test]
fn rollback_with_a_missing_previous_package_recovers_without_reviving_code() {
    let data = TempDir::new().expect("data");
    let cache = TempDir::new().expect("cache");
    let installed_bytes = package_bytes(&versioned("0.0.9"));
    let installed = with_artifact(versioned("0.0.9"), &installed_bytes);
    let latest_bytes = package_bytes(&catalog());
    let latest = with_artifact(catalog(), &latest_bytes);
    let manager = manager(latest.clone(), &data, &cache);
    let installed_hash = manager
        .install_package(&installed, &installed_bytes)
        .expect("installed package");
    record_enabled(&manager, &installed, installed_hash);
    let prepared_hash = manager
        .install_package(&latest, &latest_bytes)
        .expect("prepared update");
    manager
        .pending_transactions()
        .expect("transactions")
        .insert(
            "synthetic-update".to_string(),
            PreparedPluginTransaction {
                plugin_id: latest.manifest.id.clone(),
                permissions: latest.manifest.permissions.iter().cloned().collect(),
                artifact_sha256: latest.artifact.sha256.clone(),
                catalog_fingerprint: catalog_fingerprint(&latest).expect("fingerprint"),
                entrypoint_sha256: Some(prepared_hash),
                diagram_renderer_sha256: None,
                previously_enabled: true,
            },
        );
    fs::remove_dir_all(manager.install_dir(&installed)).expect("lose previous package");

    manager
        .rollback_enable("synthetic-update", Some("Synthetic update failure".into()))
        .expect("rollback");
    assert!(!manager.install_dir(&latest).exists());
    manager.reconcile_packages().expect("startup recovery");

    let id = &latest.manifest.id;
    let state = manager.state().expect("state");
    assert!(!state.enabled.contains(id));
    assert!(!state.installed_manifests.contains_key(id));
    assert!(
        state
            .errors
            .get(id)
            .is_some_and(|error| error.contains("missing"))
    );
    drop(state);
    assert!(!manager.plugin_root(id).exists());
    assert!(manager.read_entrypoint(id).is_err());
}
