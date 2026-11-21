---
name: denote-plugin-workflow
description: Create, change, release, or adopt Denote plugins across denote-io repositories, including new plugin repositories, plugin SDK changes and releases, SDK pin bumps, minimumDenoteVersion, plugin releases, and catalog adoption with update:plugin. Use for any denote.* plugin or @denote/plugin-sdk work.
---

# Denote plugin and SDK workflow

The canonical procedure is **Plugin and SDK workflow** and **Adopt a plugin
release** in `docs/development.md`; the package, permission, and security
contract is `docs/plugins.md`. Read the sections that apply before acting. This
skill adds the decision logic, ordering, and traps. If it disagrees with those
docs, follow the docs and correct this skill in the same change.

## Where each change belongs

| Repository | Owns |
| --- | --- |
| `denote-io/denote-plugin-sdk` | API 1 contract, manifest and catalog validation, the `denote-plugin` command |
| `denote-io/denote-<name>` | Plugin `denote.<name>`: source, tests, CI, **Release** workflow |
| `denote-io/denote` (this repository) | Host implementation, SDK pin, `plugins/catalog.json`, `plugins/bundles.json` |

Each repository changes through its own pull request. This repository never
holds plugin source or archives, and `src/` never imports a plugin package.
Trusted native Git support belongs in `src-tauri/src/plugins/git/`.

## Choose the path

Classify the change before writing code; each kind uses only some steps:

| Change | Steps | Notes |
| --- | --- | --- |
| Existing plugin, released host capabilities only | 4, 5 | Raise `minimumDenoteVersion` only for a capability from a later published release |
| Plugin plus a new manifest field, permission, capability, or host operation | 1, 2, 4, 5 | Two Denote pull requests: host support, then adoption after a Denote release ships it |
| Plugin plus `denote-plugin` tooling or SDK helpers bundled into the plugin, host unchanged | 1, 4, 5 | Move the plugin to the new SDK tag; no Denote release needed |
| SDK only | 1 | Denote and plugins keep their pins; move each only when it needs the change |
| New plugin | 3, 4, 5 | Add 1 and 2 when it needs new host support |

Develop SDK and plugin changes together with a linked local SDK checkout (see
**Local development loops**) before releasing the SDK.

Look up the current SDK release rather than assuming one:

```bash
gh release view -R denote-io/denote-plugin-sdk --json tagName --jq .tagName
```

## Steps

1. **Release the SDK** (in the SDK repository). Run `npm run check`. Record a
   public declaration change with `npm run check:api -- --update-baseline` only
   after the API 1 freeze policy in `docs/plugins.md` allows it; never
   regenerate the baseline to silence a failure. Bump `package.json`, update
   the tag in its README examples, run `npm run build` so `dist/` matches,
   merge with CI passing, then tag that merge commit `v<version>` with a
   GitHub release.
2. **Support it in Denote.** `npm install github:denote-io/denote-plugin-sdk#v<version>`,
   then implement and test the frontend and the native validation under
   `src-tauri/src/plugins/` together. `npm run check:plugins` revalidates every
   catalog entry with the new SDK, so a stricter validator surfaces entries it
   would reject. Plugins may depend on new host support only after a Denote
   release includes it.
3. **Create the plugin repository.** Scaffold with
   `npx --package github:denote-io/denote-plugin-sdk#v<version> denote-plugin create denote-<name> denote.<name> "<Name>"`,
   run `npm install` to create `package-lock.json`, which the scaffold omits
   and its CI's `npm ci` requires, commit both, and create
   `denote-io/denote-<name>`. Set
   `compatibility.minimumDenoteVersion` in `plugin.json` (see the rules below).
4. **Release the plugin.** To move it to a new SDK tag, run
   `npm install github:denote-io/denote-plugin-sdk#v<version>` in its
   repository. Bump `version` in `plugin.json` and `package.json` together in
   the same pull request as any change to the archive, merge with CI passing,
   and run the plugin repository's **Release** workflow once its minimum
   Denote release is published.
5. **Adopt it in Denote.** Run `npm run update:plugin -- denote.<name> <version>`
   and review `plugins/bundles.json`. Update the plugin's sections in
   `docs/plugins.md`, `docs/development.md`, and the user guide when behavior
   or permissions changed, and complete the new-plugin checklist below for a
   new plugin.

## Rules and why they exist

- **Name a published release in `minimumDenoteVersion`.** Denote compares it
  with its own application version, which stays at the latest published release
  until `scripts/release.sh` runs. An unreleased minimum makes the plugin
  incompatible in development builds, tests, and the adoption pull request.
- **Never change a published version's bytes.** The catalog pins each archive's
  size, SHA-256 digest, manifest, guide, and source commit, and plugin CI checks
  that the released version still rebuilds byte for byte. Any source,
  dependency, compiler, or SDK pin change needs a new plugin version.
- **Bump in the pull request that changes the archive.** Plugin CI rebuilds on
  every pull request and fails when the current version is already published
  with different bytes. README, test, and CI changes leave the archive alone
  and need no bump; later changes before the next release reuse the bumped
  version.
- **Expect an SDK pin change to change archive bytes.** The archived
  `package.json` records the SDK version, so even an otherwise identical plugin
  needs a new version. Plugin repositories pin the SDK independently; an SDK
  release changes no existing plugin.
- **Explain permission changes.** Automatic updates skip a version whose
  requested permissions differ; users approve it through **Review and update**
  or **Update all**, so the plugin guide must explain what changed.
- **Users get adopted versions with a Denote release.** The application
  compiles `plugins/catalog.json` in, so adoption reaches users only when the
  next Denote release ships.
- **Leave the application version alone.** Host-support and adoption pull
  requests keep the latest published version; only a release bumps it.
- **Keep archives out of Git and installers.** Plugin `.tgz` files are GitHub
  Release assets of their plugin repository only.
- **Keep the production downloader strict.** Test unpublished builds with the
  debug-only local archive loader, never with `file:` or localhost support.
- **Respect import boundaries.** Plugin source may import `@denote/plugin-sdk`,
  its own files, and declared third-party packages, never Denote's `src/`,
  `@tauri-apps/*`, or another plugin.
- **Use synthetic test data** in every repository.

## Local development loops

- **Plugin:** in the plugin repository run `npm ci` and `npm run dev`, which
  rebuilds `.plugin-dev/denote.<name>.tgz`. Here run `npm run dev:desktop`, then
  **Settings → Plugins → Load local plugin archive**. Disable the plugin before
  loading rebuilt bytes with the same ID. For a macOS bundle, follow **Preview a
  local plugin build on macOS** in `docs/development.md`.
- **Unreleased SDK:** build the SDK checkout (`npm ci && npm run build`), then
  run `npm install --no-save --ignore-scripts ../denote-plugin-sdk` in the
  plugin repository or here. This links the checkout without editing
  `package.json`; `npm ci` restores the pinned tag. Never commit a local path.

## Validate the Denote side

```bash
npm run check:plugins
npm run check:plugin-downloads
npm run fetch:plugins
npm test
```

`fetch:plugins` stages the published archives that tests and benchmarks load,
so run it before `npm test`. Host-support changes also need the matching Rust
tests from `docs/development.md`.

## New-plugin checklist

- [ ] Catalog entry added by `update:plugin`, and roles in `plugins/bundles.json`
      when the plugin fills one.
- [ ] Contract section in `docs/plugins.md`, development section in
      `docs/development.md`, and user section in
      `docs/user-guide/docs/Optional plugins.md`.
- [ ] Icon in `denote-io/denote.synctune.it`.
- [ ] Row in the plugins table of `denote-io/.github` `profile/README.md`.
- [ ] No archive committed and no application version bump.
