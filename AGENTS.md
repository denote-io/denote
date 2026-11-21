# Agent guide

Read these documents before changing Denote:

- `docs/product.md` defines product behavior and constraints.
- `docs/design.md` defines interface and accessibility conventions.
- `docs/architecture.md` explains implementation and security boundaries.
- `docs/development.md` lists setup and validation commands.
- `docs/user-guide/` contains the canonical in-app documentation.

## TODO/FIXME: re-check vendored Rust fixes at every development startup

**MANDATORY: every agent must do this at the start of EVERY Denote development
session, before editing files, even when the requested task is unrelated to
Rust. Do not skip this check or assume the previous check is still current.**

Inspect `src-tauri/vendor/` and `[patch.crates-io]` in `src-tauri/Cargo.toml`.
Check official crate releases, upstream release notes, issues/pull requests, and
RustSec for **every vendored library**, currently `glib`, `glib-macros`,
`gtk3-macros`, `urlpattern`, `tauri-plugin-notification`, `tauri-plugin-opener`,
and `tauri-plugin-single-instance`. Look specifically for a published upstream
fix or maintained replacement that can remove each local backport.

Record the check date, versions examined, exact upstream links, and the outcome
in **Vendored-library availability check** in `docs/development.md`. Explicitly
report any compatible fix found; otherwise record why the backport remains
necessary. A newer version alone is not evidence of a compatible fix.

Do not remove a backport until its documented source-aware audit, optimized
regression tests, and all-platform checks pass. Preserve the Windows-safe Tauri
family and follow **Re-evaluate the Windows runtime pin** before changing any
related version. Never hide the remaining GLib advisory, relabel a crate, or
invent a patched version to make an audit look clean.

## Interface and copy

Before changing interface code or user-visible text, read the relevant section
of `docs/design.md` and inspect nearby components. Reuse Denote's existing
patterns instead of inventing a one-off treatment:

- Use the shared `primary-button`, `secondary-button`, and `icon-button`
  hierarchy. A screen or form gets one primary action; supporting actions stay
  secondary or quiet.
- Match the surrounding control density, spacing, borders, corner radius,
  typography, focus ring, disabled state, and forced-colors behavior.
- Write short sentence-case labels and status text. Use Denote's established
  terms and exact action names. Prefer `…` for progress and `→` for interface
  paths where those forms already appear in the app.
- Do not expose Markdown markers such as `**`, backticks, or list prefixes as
  literal UI. Route guides and formatted help through the existing safe
  host-rendered Markdown subset, and test emphasis, lists, and code when that
  rendering changes.
- Avoid generic, promotional, or explanatory filler. Text should tell the user
  what happened, what an action does, or how to recover.

## Windows Tauri runtime pin

Denote pins the complete Windows-safe Tauri 2.11 dependency family. Tauri 2.12,
including `tauri-utils` 2.10 and Wry 0.57, produced a completely blank Windows
webview and left `denote.exe` running after its last window closed.

Before changing any `@tauri-apps/*`, `tauri*`, Tao, Wry, WebView2, URL-pattern,
or related lockfile version, an agent must first check the current official
Tauri and Wry releases and issue trackers for an upstream fix. Follow
**Re-evaluate the Windows runtime pin** in `docs/development.md`, cite the
upstream release, issue, or pull request examined, and update its last-check
record when performing a real re-evaluation.

Never float or partially upgrade this family. A candidate upgrade must move the
matching npm and Cargo packages together and may replace the pin only after the
package-equivalent Windows renderer smoke test shows the Denote UI and confirms
that closing the last window terminates the process. If no verified upstream
fix exists or the smoke test fails, keep the exact known-good versions.

## Release versions

Keep the application version at the latest published release in feature and
dependency PRs. `./scripts/release.sh <version>` owns the root npm, Cargo, and
Tauri application-version bump and the release commit. Targeting a future
release or adopting a new plugin release in the catalog does not authorize
pre-bumping the application. Plugin versions remain independent.

## Plugins

Before changing plugin code or lifecycle behavior, read:

- `docs/plugins.md` for the complete package, runtime, permission, security,
  installation, update, disablement, and publication model.
- `docs/development.md` for the current catalog, validation, local
  development, and release commands.
- `docs/user-guide/docs/Optional plugins.md` for canonical user-visible
  behavior.

Each optional plugin is developed, tested, versioned, and released in its own
repository: plugin ID `denote.<name>` lives in `denote-io/denote-<name>`, such as
[denote-io/denote-kanban](https://github.com/denote-io/denote-kanban). Plugin
repositories build, check, and package with the `denote-plugin` command from the
[plugin SDK](https://github.com/denote-io/denote-plugin-sdk), which also owns the
public API 1 contract and its frozen baseline. Denote installs the SDK from its
release tag. This repository keeps only the embedded catalog in
`plugins/catalog.json` and curated bundles in `plugins/bundles.json`; `src/`
must not import a plugin implementation package. Trusted native Git integration
belongs in `src-tauri/src/plugins/git/`, not in downloadable plugin code.

The application embeds catalog metadata only. Plugin `.tgz` files are GitHub
Release assets of their plugin repositories and must never be committed, added
to Tauri resources, or included in desktop installers. Published versions are
immutable: never automatically bump a version or replace bytes at an existing
version. Catalog `provenance.sourceCommit` names the source that built the
archive: the `mbianchidev/denote` commit for versions published before the move
to plugin repositories, and the release tag commit in the plugin repository
for later versions.

After a plugin repository publishes `v<version>`, adopt that release with:

```bash
npm run update:plugin -- denote.example 0.2.0
npm run check:plugins
npm run check:plugin-downloads
```

`update:plugin` downloads the release asset, reads its manifest and guide,
records its URL, size, SHA-256, and tag commit, and atomically rewrites only
that catalog entry. It refuses different bytes for a version already in the
catalog. Review `plugins/bundles.json` before committing. `check:plugins` type
checks `scripts/*.ts` and validates the catalog, bundles, trusted publisher,
release URLs, and editor import boundary offline. `check:plugin-downloads`
verifies every published archive against its catalog entry. Run
`npm run fetch:plugins` before `npm test` and browser gates: it applies the same
verification, then stages the archives and their extracted files in ignored
`.plugin-artifacts/` for browser gates, host tests, and native lifecycle tests.
Scaffold a new plugin repository with the SDK's `denote-plugin create`, as
described in `docs/development.md`. Disabling a plugin must remove package
code, cached archives, staging content, and removal backups without deleting
vault content.

For local plugin development, run `npm ci` and `npm run dev` in the plugin
repository, which rebuilds the ignored `.plugin-dev/<plugin-id>.tgz`, and
`npm run dev:desktop` here. Load that archive from **Settings → Plugins → Load
local plugin archive**. The local adapter is debug-only, visibly untrusted, and
must continue to reuse the production manifest, permission, extraction,
entrypoint-integrity, worker, rollback, and cleanup boundaries. Never add
`file:` or localhost support to the production downloader. Disable a local
plugin before loading rebuilt bytes with the same ID.

For a local macOS handoff without Developer ID credentials, build with
`CI=true APPLE_SIGNING_IDENTITY=- npm run tauri build -- --bundles app,dmg`,
verify the retained app with `codesign --verify --deep --strict`, then open the
generated `.dmg` from `src-tauri/target/release/bundle/dmg/`. The explicit ad-hoc
identity must sign the main app as well as the helper; a linker-only signature
does not apply the microphone entitlement or seal bundle resources. Use the
configured Developer ID identity instead of `-` for a signed release. Follow
**Preview a local plugin build on macOS** in `docs/development.md` for the
isolated Development bundle.

Preserve the one-time, non-destructive Denote Welcome vault behavior. Existing
Welcome vaults must not be overwritten. When adding, removing, or moving a
Welcome file, update `SEED_FILES` in `src-tauri/src/default_vault.rs`. Content
changes affect new Welcome vaults unless a separate non-destructive migration is
implemented. The versioned `examples-v1` addition may add only missing
`examples/` and `code/` files to older Welcome vaults; it preserves existing
paths, defers while encrypted, and writes ciphertext after unlock. The separate
`plugins-v1` addition applies the same guarantees only to the original Kanban
sample. `plugins-v2` adds its fixed new `plugins/` example inventory, including
to vaults that already applied v1. Preserve earlier marker bytes and inventories;
do not restore samples deleted after their version was applied or enable
plugins or initialize Git as part of seeding.

Tests and test fixtures must use synthetic mock data. Never copy filenames,
paths, note text, customer data, personal data, or production data from a real
vault into tests. Reduce regressions to the smallest invented example that
preserves the behavior being tested.
