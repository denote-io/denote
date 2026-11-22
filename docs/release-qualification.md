# v1.0.0 release qualification

This record tracks the Phase 7 release-hardening gates from the
[v1.0.0 roadmap](https://github.com/denote-io/denote/issues/41). A gate is
signed off only when its automated evidence passes on `main` and every manual
row below has a recorded result. Passing automation never substitutes for the
manual platform, installer, and assistive-technology checks.

Use disposable operating-system profiles and invented vaults for every manual
check. Never record real notes, paths, credentials, or personal data here.

## Gate status

| Gate | Automated evidence | Manual evidence | Status |
| --- | --- | --- | --- |
| Safe mode, failed update, interrupted install, incompatible plugin, corrupted package, rollback | [Failure-mode tests](#failure-mode-coverage) on all CI platforms | [Platform checklist](#platform-checklist) rows 4 to 9 | Automated complete; manual pending |
| Install, update, disable, delete, recovery on macOS, Windows, Linux | Staged native lifecycle and published-download tests | [Platform checklist](#platform-checklist) | Automated complete; manual pending |
| Encrypted-vault behavior for every content plugin | [Encrypted-vault matrix](#encrypted-vault-matrix) | Lock/unlock rows in the platform checklist | Automated complete; manual pending |
| Dependency, integrity, provenance, secret, vulnerability checks | [Supply-chain checks](#supply-chain-checks) | Release asset verification | Automated complete for this revision |
| User, plugin, developer, troubleshooting, data, backup, removal docs | [Documentation inventory](#documentation-inventory) | Review on the published website | Complete |
| Telemetry and external data transfer | [External connections](#external-connections) | Network capture on one platform | Automated complete; manual pending |
| Large vault, repeated enable/disable, long-running, restart, crash, leak | [Resource coverage](#resource-coverage) | Long-running and sleep/restart rows | Automated complete; manual pending |
| v1 plugin API freeze, compatibility, deprecation | [API freeze](#api-freeze) | None | Complete |
| Release candidate and fix-only freeze | [Release candidate](#release-candidate) | Candidate smoke on every package | Pending |

Accessibility review is tracked separately in the roadmap; see
[Release-hardening checks](development.md#release-hardening-checks) for its
assistive-technology matrix.

## Failure-mode coverage

Native tests run on macOS, Windows, and Linux in CI:

```bash
cargo test --manifest-path src-tauri/Cargo.toml --locked --lib plugins::failure_mode_tests
cargo test --manifest-path src-tauri/Cargo.toml --locked --lib plugins::tests
npm run fetch:plugins
cargo test --manifest-path src-tauri/Cargo.toml --locked --lib \
  staged_catalog_packages_complete_native_lifecycle -- --ignored
```

| Scenario | Evidence |
| --- | --- |
| Safe mode blocks execution, installation, and updates while keeping packages, enablement, and recovery | `safe_mode_blocks_execution_and_installation_but_preserves_packages_and_recovery`, `keeps approved plugins stopped in startup safe mode…`, browser `startup safe mode keeps approved code stopped…` |
| Minimum, exclusive maximum, API, invalid version, unsupported platform, revoked version | `version_api_and_platform_bounds_refuse_incompatible_plugins`, `revoked_catalog_version_is_incompatible_before_install`, `every_catalog_plugin_supports_api_one_hosts_throughout_denote_one_x` |
| Incompatible entry refused before download | `incompatible_catalog_entry_is_refused_before_any_download_or_install` |
| Incompatible installed manifest at startup | `incompatible_installed_manifest_is_disabled_and_removed_at_startup` |
| Corrupted package: size, checksum, truncated gzip, non-gzip, invalid tar, traversal, absolute path, link entry, missing or invalid manifest, ID/version/permission mismatch, missing entrypoint | `corrupted_packages_are_rejected_without_partial_installation_or_escape`, `rejects_checksum_mismatch_without_partial_install`, `rejects_link_entries_in_plugin_archives` |
| Tampered installed code | `tampered_entrypoint_is_removed_during_startup_recovery`, `read_entrypoint_rejects_changes_after_preparation`, `mismatched_approved_permissions_disable_and_remove_installed_code` |
| Interrupted download, extraction, update staging, uncommitted first install, and atomic removal | `interrupted_install_update_and_removal_residue_is_recovered_on_restart` restarts a real `PluginManager` over the residue |
| Corrupt state file | `corrupt_state_is_quarantined_without_blocking_core_startup`, `credential_ledger_survives_corrupt_main_state` |
| Failed update keeps the installed version | `failed_update_verification_keeps_the_installed_version_running`, `failed_update_removes_only_the_new_package`, `restarts the installed version when an explicit update fails`, `…update download returns 404…` |
| Rollback after runtime start failure or vault lock | `stops the runtime and rolls back without committing…`, `rolls back a prepared content plugin when the vault locks…` |
| Rollback when the previous package disappeared | `rollback_with_a_missing_previous_package_recovers_without_reviving_code` |
| Permission-changing updates never apply automatically | `never auto-updates a plugin whose update requests different permissions` |

The installer does not compare a package's guide with the catalog at runtime.
`npm run check:plugin-downloads` verifies every published archive's manifest
and guide against its catalog entry before release.

A release candidate sorts before its final release. A plugin whose
`minimumDenoteVersion` is `1.0.0` is therefore incompatible with `1.0.0-rc.1`.
Every current catalog plugin declares a minimum of `0.12.0` or lower.

## Encrypted-vault matrix

`applies the locked-vault contract to <plugin>'s real catalog permissions` in
`src/plugins/usePlugins.test.tsx` runs once per catalog plugin with its real
permissions. A separate test fails when a catalog plugin lacks a declared
contract. Content plugins never start while a vault is locked, start after
unlock, and stop on the next lock. Native commands refuse locked vaults
independently of the renderer.

| Plugin | Content it touches | Locked vault | Encrypted, unlocked | Additional evidence |
| --- | --- | --- | --- | --- |
| Reference | Note paths and event kinds only | Keeps running | No content access | Contract test |
| Git | Reads, commits, pushes | Worker runs; native Git refuses | Commits ciphertext only | `blocks_operations_while_the_vault_is_locked_or_mid_maintenance`, automatic-commit tests |
| Emoji picker | Inserts into the open editor | Keeps running; editors are closed | Ordinary editor write | Contract test |
| JSON and YAML viewer | Reads the open file | Stopped | Host snapshot only | Contract and per-capability lock tests |
| Mermaid | Reads and renders diagrams | Stopped | Local render | Contract test |
| Kanban | Reads and writes boards | Stopped | Versioned host writes | Contract test |
| Note graph | Indexes links | Stopped; index withdrawn | Bounded snapshots | Contract test, vault-switch restart test |
| Calendar | Indexes dates | Stopped; models withdrawn | Bounded snapshots | `withdraws calendar models immediately on a vault switch or lock` |
| Task lists | Indexes and toggles tasks | Stopped | Versioned host writes | Contract test |
| Reminders | Note paths and names in application data | Keeps running | Metadata outside vault encryption, documented | Contract test |
| Code intelligence | Executes language servers | Stopped | Native tooling refused | `codeHost` encrypted refusal |
| Time tracking | Session metadata in application data | Clocks keep running | Metadata outside vault encryption; exports are plaintext, documented | Contract test, `keeps combined clock workers running…` |
| Text styling | Reads and writes styling | Stopped | Host snapshot only | Contract test |
| Note comments | Encrypted sidecar | Stopped; counts cleared | Ciphertext sidecar | `clears counts immediately on lock…` |
| Presentations | Reads decks | Stopped | Host snapshot only | Contract test |
| Speech | Records, transcribes, exports audio | Stopped; sessions revoked | Explicit plaintext export only | Contract test, sensitive browser gate |
| Credential store | Separately encrypted store | Stopped; sessions revoked | Explicit plaintext export only | Credential container tests |
| Linter | Executes tools | Stopped | Native tooling refused | `encrypted_tool_contexts_refuse_both_locked_and_unlocked_vaults` |
| Build and compiler | Executes builds | Stopped | Native tooling refused | Same native refusal |
| Ghostty terminal | Executes a shell | Stopped | Terminals refused | `terminalHost` encrypted refusal |

## Supply-chain checks

Run these from a clean checkout. Results below were recorded for this branch on
2026-11-22 on macOS arm64; CI repeats them on every platform.

```bash
npm run check:secrets
npm audit --audit-level=high
node scripts/audit-rust-dependencies.mjs
npm run check:plugins
npm run check:plugin-downloads
```

| Check | Result |
| --- | --- |
| Gitleaks 8.30.1 history scan (checksum-verified binary) | No leaks in 559 commits |
| npm audit, high severity | 0 vulnerabilities |
| Source-aware Rust audit | Passes. The version-based GLib 0.18.5 [RUSTSEC-2024-0429](https://rustsec.org/advisories/RUSTSEC-2024-0429) warning stays visible; release-mode regression tests verify the [local source fix](development.md#rust-dependency-audits-and-linux-backports) |
| Catalog, bundles, trusted publisher, release URLs, import boundary | 20 plugins and bundles valid |
| Every published plugin archive against its size, SHA-256, manifest, and guide | 20 archives verified |

Release builds additionally publish SHA-256 checksum files, SBOMs, and build
provenance for every package. The vendored-library availability check in
[development](development.md#vendored-library-availability-check) records why
each local backport remains.

## Documentation inventory

| Audience | Documents |
| --- | --- |
| Users | [Getting started](user-guide/docs/Getting%20started.md), [Feature reference](user-guide/docs/Feature%20reference.md), [Optional plugins](user-guide/docs/Optional%20plugins.md), [Data, backups, and removal](user-guide/docs/Data,%20backups,%20and%20removal.md), [Troubleshooting](user-guide/docs/Troubleshooting.md), [Vault encryption](user-guide/docs/Vault%20encryption.md), [App lock](user-guide/docs/App%20lock.md) |
| Plugin users | Each catalog plugin's versioned in-app guide and the [plugin examples](user-guide/plugins/README.md) |
| Developers | [Development](development.md), [Architecture](architecture.md), [Plugin host](plugins.md), [Design system](design.md) |
| Security | [Sensitive plugin security design](phase-6-security.md) |

## External connections

Denote contains no analytics, telemetry, or crash-reporting integration, and
plugins receive no telemetry API. Every outbound path below is disclosed in
[Optional plugins](user-guide/docs/Optional%20plugins.md#privacy-and-external-connections)
or [Getting started](user-guide/docs/Getting%20started.md#about-updates-and-bug-reports).

| Path | Trigger | Control |
| --- | --- | --- |
| Application update check | Automatic after startup in configured release builds | Signed channel; installation waits for **Restart to update** |
| Plugin package download | Explicit enable or update | Permission approval; separate opt-in for automatic updates |
| Speech model and runtime | Explicit download | Separate approval of model, license, origin, and size |
| Remote Markdown images | Rendering an HTTP(S) image | **Load remote images**, off by default |
| Git remotes | Explicit clone, fetch, pull, or push | User action and Git credentials |
| Plugin network capability | Plugin request to a declared HTTPS host | Declared and approved permission; no current catalog plugin requests it |
| External links and bug reports | Explicit click | External-domain confirmation; bug drafts are never submitted automatically |

Automated evidence: `downloads a signed update after startup and waits for the
user to restart`, remote-image blocking in `MarkdownEditor.test.tsx`, bug-report
redaction in `bugReport.test.ts`, and the Content-Security-Policy assertion in
`diagramRenderers.test.ts`.

## Resource coverage

| Area | Evidence |
| --- | --- |
| Large vault | `benchmarks/indexes.perf.ts` indexes 20,004 synthetic notes with typing p95 below 250 ms and single-file updates; graph, task, and calendar projections reuse unchanged items below 200 ms |
| Repeated enable/disable | `repeated enable and disable cycles release workers, ports, closures, listeners and DOM nodes` runs six representative plugins through one warm-up and five measured cycles. On 2026-11-22 it retained zero workers, message ports, functions, maps, promises, listeners, and DOM nodes. Heap grew about 1.5 MB, which heap snapshots attribute to V8 compiled code and bounded browser performance-entry buffers rather than retained objects |
| Catalog-wide lifecycle | Browser journey enables and disables every catalog worker; staged native lifecycle repeats enable, disable, update, rollback, and retry for every archive on each platform |
| Long-running processes | Terminal job-group shutdown, Git cancellation, and code-intelligence cancellation tests; hours-long sessions are a manual row |
| Restart | Interrupted-residue restart test, startup recovery tests, and Git crash-residue recovery |
| Crash | `tears down a worker that throws an uncaught error…`, `disables and removes a running plugin whose worker crashes`, runtime-error teardown |

## API freeze

The plugin SDK's `api/v1` declaration baseline is checked by `npm run check:api`
in its CI. Denote pins the SDK release
[`v0.1.1`](https://github.com/denote-io/denote-plugin-sdk/releases/tag/v0.1.1).
Compatibility bounds and the 1.x deprecation policy are defined in
[API 1 freeze and deprecation policy](plugins.md#api-1-freeze-and-deprecation-policy).

## Platform checklist

Record each row for macOS (arm64 and x86_64), Windows x86_64 (`.exe` and
`.msi`), and Linux x86_64 (`.AppImage`, `.deb`, `.rpm`) with the tester, date,
package, and result.

1. Fresh install opens **Denote Welcome** and creates no plugin package.
2. Upgrade from the latest stable release keeps vaults, settings, revisions,
   and enabled plugins.
3. Enable, use, and disable every catalog plugin; its package folder is removed
   and invented notes keep their bytes.
4. Start with `--safe-mode`; plugins show **Paused in safe mode**, editing works,
   and disabling still removes code.
5. Disconnect the network during a plugin install; the error is visible, no
   package remains, and a retry succeeds.
6. Quit Denote during a plugin update; the next launch keeps the earlier version.
7. Start an older Denote build against a profile whose plugin requires a newer
   version; the plugin is disabled with an incompatibility message.
8. Replace one installed plugin file in application data; the next launch
   removes that plugin with an integrity message.
9. Interrupt an application update download; the current version keeps running
   and **Check for updates** retries.
10. Encrypt an invented vault, enable content plugins, lock, and unlock; content
    plugins stop and resume, and no plaintext appears in application data.
11. Keep a terminal, language server, and build running for at least one hour,
    then sleep and wake the computer; output resumes or reports a clear error,
    and disabling leaves no child process.
12. Capture network traffic during startup and ordinary editing; only the
    disclosed update check appears.
13. Uninstall with and without application-data removal; vault folders remain.

## Release candidate

After the qualification pull request merges and `main` CI passes, a maintainer
runs `./scripts/release.sh 1.0.0-rc.1` from a clean `main`. The release workflow
publishes it as a GitHub prerelease, so the stable updater channel, which reads
the latest full release, does not offer it.

From that candidate until v1.0.0, only release-blocking fixes are accepted:
regressions, data loss, security, crashes, broken installation or update, and
failed gates above. New capabilities, permission expansion, catalog additions,
and unrelated refactors wait for 1.0.1 or later. Each fix produces the next
candidate number, and the platform checklist is repeated for the affected
packages before `./scripts/release.sh 1.0.0`.
