# Whole-app sync

Denote can replicate every synced vault, portable preferences, revision
history, and plugin data between devices through storage the user controls:
a dedicated Git repository, Google Drive, a local or mounted folder, WebDAV, or
an S3-compatible bucket. Remote data is always end-to-end encrypted with a sync
passphrase; the storage provider sees only opaque object names and ciphertext.

The feature is enabled by the optional `denote.sync` plugin from
[denote-io/denote-sync](https://github.com/denote-io/denote-sync). Like
`speech` and `credential-store`, its `sync` permission is an isolated
sensitive-tool permission: the downloaded worker only registers the tool. The
trusted host in `src-tauri/src/sync/` owns enrollment, credentials, encryption,
transports, scanning, merging, and file application, and the renderer owns the
host-rendered **Sync** panel. Downloaded code never receives vault content,
paths, preferences, plugin data, secrets, or the passphrase.

This is separate from the per-vault `denote.git` plugin. Git versioning keeps
the history of one vault in that vault's own repository. Sync replicates the
whole application through one separate store and never reads or writes any
`.git` entry.

## Scope matrix

| Data | Synced | Notes |
| --- | --- | --- |
| Files of each enrolled vault | Yes | Byte-exact. Encrypted vaults replicate ciphertext. |
| `.denote/encryption.json` | Yes, as control state | Concurrent changes pause that vault for an explicit choice. |
| `.denote/note-comments.json`, `.denote/credentials.dncred` | Yes, whole-file | Concurrent changes keep both versions. |
| `.denote/trash/`, `.denote/locks/`, speech journals, temporary files, OS junk (`.DS_Store`, `._*`, `Thumbs.db`, `desktop.ini`, Office lock files) | No | Device-local operational state. |
| `.git` directories or files at any depth | No | Use `denote.git` for repository history. |
| Symbolic links, sockets, devices | No | Never followed or created. |
| Vault name | For display | Each snapshot lists its writer's vault names so a joining device can offer them. A vault's name is its local folder name, so it is never applied. |
| Markdown view mode, tab-restore flag, welcome page | Yes | Stored per vault sync ID. |
| Bookmarks, file ordering and pins, tag colors, explicit project roots | Yes | Keyed by vault sync ID and relative path. |
| Revision history | Yes | Append-only set union; encrypted rows stay encrypted. |
| Theme, editor display, default Markdown view, dotfile visibility, activity-rail layout, dictation shortcut | Yes | Portable renderer preferences allowlist. |
| Sidebar and outline widths | No | Device layout. |
| Approved external domains, remote-image consent, update channel, automatic plugin updates | No | Trust, network consent, and standing automation stay local. |
| Plugin settings | Yes, portable keys only | Executable, path, authentication, signing, and automation keys stay local. Imported through validated settings import. |
| Plugin state values | Yes, per key | Concurrent values are preserved for an explicit choice. |
| Enabled plugins | As suggestions only | Never downloads, approves, or enables a plugin. |
| Plugin packages and permission approvals | No | Each device approves its own plugins. |
| App lock, last/default vault, tab sessions, caches, pending operations | No | Device-local. |
| Keychain secrets, Git signing passphrases, sync credentials | No | Never leave the operating-system credential store. |

Disabled or unregistered plugins keep their synced settings and state in the
local sync index; nothing is applied until the plugin is enabled locally.
Plugin settings are applied only when the local plugin uses the same settings
schema version; otherwise they wait for the plugin to be updated.

Values equal to their default are treated as absent, so a freshly installed
device never outvotes a configured one: vault preferences that match Denote's
defaults and plugin settings that match the manifest default publish nothing,
and a deletion resets the value to its default.

## Remote layout

Every backend is used as a flat key-value store under the prefix
`denote-sync/v1/`. Keys contain only `[a-z0-9._/-]`.

| Key | Content | Writer |
| --- | --- | --- |
| `repository.json` | Plaintext header: format, repository ID, KDF parameters, salt, wrapped master key | First device; replaced only by passphrase change |
| `objects/<hh>/<id>` | Encrypted content object, immutable | Any device |
| `heads/<writer>/<generation>` | Encrypted snapshot generation, immutable | Only `<writer>` |
| `retired/<writer>` | Encrypted retirement record | Any device |

`<id>` is 64 lowercase hex characters, `<hh>` its first two. `<writer>` is a
32-hex-character random writer ID. `<generation>` is a 20-digit zero-padded
decimal counter. A retirement record is the sealed JSON
`{ writer, retiredBy, retiredAt }`.

### Backend contract

`src-tauri/src/sync/backend.rs` defines:

```rust
pub(crate) enum PutMode { CreateOnly, Overwrite }
pub(crate) enum PutOutcome { Written, AlreadyExists }

pub(crate) trait Backend: Send + Sync {
    fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>>;
    fn put(&self, key: &str, bytes: &[u8], mode: PutMode) -> AppResult<PutOutcome>;
    /// Every key under `prefix`, following all pagination. Order is unspecified.
    fn list(&self, prefix: &str) -> AppResult<Vec<String>>;
    fn delete(&self, key: &str) -> AppResult<()>;
    /// Make remote state visible locally (Git: fetch and rebase).
    fn refresh(&self) -> AppResult<()>;
    /// Publish buffered writes (Git: commit and push). No-op elsewhere.
    fn flush(&self) -> AppResult<()>;
}
```

Requirements every backend meets and the conformance suite in
`src-tauri/src/sync/backend_tests.rs` checks:

- `put` never exposes a partially written value under its key.
- `CreateOnly` returns `AlreadyExists` instead of replacing a value. Backends
  without a native conditional write (some WebDAV and S3-compatible servers)
  check, write, and re-read; the protocol never depends on `CreateOnly` for
  safety except for `repository.json`, which is re-read and adopted.
- `list` follows every page. A missing key is never interpreted as deletion.
- Network failures, throttling, and authentication failures are errors, never
  empty results.

| Backend | Identity | Transport | Secrets in keychain |
| --- | --- | --- | --- |
| Folder | Host-selected absolute folder | Temp file + rename | None |
| WebDAV | HTTPS base URL | `PROPFIND`, `MKCOL`, `PUT` (`If-None-Match: *`), `GET`, `DELETE` | Username and password |
| S3-compatible | Endpoint, region, bucket, prefix, path-style flag | AWS Signature V4, `ListObjectsV2`, conditional `PutObject` | Access key ID and secret |
| Google Drive | App folder created by Denote | OAuth 2.0 PKCE loopback, `drive.file` scope; keys stored in `appProperties`; duplicates resolved by oldest `createdTime`, then file ID | Refresh token |
| Git | Remote URL of a dedicated repository | Hardened host Git transport in a private clone under application data | Existing Git authentication modes |

The Git backend commits only under `denote-sync/v1/`, rebases onto the remote
before every push, resolves an add/add conflict on an `objects/` key by keeping
the remote copy (both decrypt to the same plaintext), retries a rejected push a
bounded number of times, and never force-pushes. Its repository must not be any
vault's repository: the clone may not overlap a vault, and a remote configured
in a vault's own repository is refused. Each clone is bound to one remote URL
and branch.

WebDAV, S3, and Google Drive require `https://`; plain `http://` is accepted
only for loopback hosts such as a local MinIO or SeaweedFS. Requests never
follow redirects, retry throttling and transient server errors with bounded
backoff, and bound responses (300 MiB per object, 16 MiB per listing page).
Google Drive signs in with the OAuth client built in from
`DENOTE_GOOGLE_OAUTH_CLIENT_ID` (and `DENOTE_GOOGLE_OAUTH_CLIENT_SECRET`, which
Google treats as public for desktop clients) unless the user supplies a client
ID.

## Cryptography

- **Master key:** 32 random bytes, generated by the first device.
- **Passphrase key:** Argon2id over the passphrase with a 16-byte salt. KDF
  parameters read from `repository.json` are bounded before any work
  (memory 19 MiB to 1 GiB, 1 to 10 iterations, parallelism 1 to 8).
- **Wrapping:** XChaCha20-Poly1305 with associated data
  `denote-sync/v1/repository` and the repository ID.
- **Subkeys:** HMAC-SHA256(master, label) for `denote-sync/v1/enc`,
  `denote-sync/v1/mac`, and `denote-sync/v1/head`.
- **Object ID:** HMAC-SHA256(mac key, plaintext), so equal content
  deduplicates without revealing a plain content hash.
- **Envelope:** `DSO1` or `DSH1` magic, 24-byte random nonce, then
  XChaCha20-Poly1305 ciphertext. The associated data binds the magic,
  repository ID, and object ID or writer and generation. After decryption the
  object ID is recomputed and compared in constant time.
- **Limits:** a single file may be at most 256 MiB; a decoded snapshot at most
  64 MiB. Larger vault files are reported and skipped.
- **Passphrase change** rewraps the same master key with a new salt. It does
  not rotate the master key; a device that once knew the passphrase can still
  decrypt everything it downloaded or can download. Retiring a device prevents
  its future publications from being merged; it does not revoke access.
- The passphrase is never stored remotely. **Remember on this device** stores
  the unwrapped master key in the operating-system credential store under the
  reserved host namespace `host-sync.` (`host-sync.master-key`, and the
  transport credentials as `host-sync.backend`); plugins cannot read it. A
  master key unlocked without **Remember** stays in memory until Denote quits
  or sync is disconnected. Sync passphrases need at least 12 characters.

## Snapshot model

A generation snapshot is the full state a writer has observed and accepted:

```text
Snapshot { format, repository, writer, generation, created_at, app_version,
           device_name, vaults: Map<vault-sync-id, name>,
           entries: Map<EntryKey, Entry> }
EntryKey = "v/<vault-sync-id>/f/<relative path>"   vault file
         | "v/<vault-sync-id>/meta"                vault preferences
         | "v/<vault-sync-id>/note/<path>"         bookmark, order, pin
         | "v/<vault-sync-id>/tag/<tag>"           tag color
         | "v/<vault-sync-id>/project/<root>"      explicit project root
         | "v/<vault-sync-id>/history/<id>"        revision (append-only)
         | "app/preferences/<key>"                 portable renderer preference
         | "plugin/<plugin-id>/settings/<key>"     portable plugin setting
         | "plugin/<plugin-id>/state/<key>"        plugin state value
         | "plugin/<plugin-id>/enabled"            enablement suggestion
Entry { value: Object(id, size, mode) | Inline(json) | Tombstone,
        modified_at, origin, vv: Map<writer, counter> }
```

A snapshot is Deflate-compressed JSON before sealing. Tags may contain `/`, so
the `<tag>` segment escapes `%` as `%25` and `/` as `%2F`. Inline values are:
vault preferences `{ markdownViewMode, restoreTabs, welcomePagePath }`, note
placement `{ bookmarked, position, pinned }`, tag colors `{ color }`, project
roots `{ explicit: true }`, preferences `{ value }`, plugin settings
`{ version, value }` (`version` is the plugin's settings schema version),
plugin state `{ value }`, and enablement `{ enabled: true }`. A revision is an
object whose plaintext is the JSON revision row (path, content, encoding, line
ending, encrypted flag, content hash, reason, creation time); its entry ID is
that object's ID, so the same revision has the same key on every device.

Unknown entry kinds from a newer format are carried through unchanged, never
dropped, so an older client cannot delete what it does not understand.

## Synchronization

Each device keeps `sync/sync.sqlite3` in application data: its writer ID,
counter, and committed generation, the last generation applied and the highest
seen from every known writer with its device name, the vault mapping (remote
vault sync ID to local vault scope ID), the local index, pending conflicts,
plugin suggestions, the renderer's portable preferences, options, and a crash
journal. Counter, index, and journal changes commit in one transaction.

For every entry key the index holds the merged entry this device publishes and
the **observed** state: the value the local disk, database, or plugin last had,
the version that value corresponds to, and (for files) its size and
modification time. A scan creates a new version only when the observed value
changes, and that version descends from the observed version, not from the
merged one. When the merged entry was never applied here (for example, a file
changed during the previous cycle), the new local version is therefore
concurrent with it and both are kept instead of the local edit silently
replacing the remote one. Values that cannot be applied locally (a plugin that
is not enabled, a suggestion) never turn into local versions.

One cycle:

1. **Lock and unlock.** Requires the plugin to be enabled with an approved
   `sync` permission, the application unlocked, not in safe mode, and the
   master key available. One cycle runs at a time.
2. **Refresh.** `backend.refresh()`, then list `heads/` and `retired/`. For
   each non-retired writer pick the highest generation not lower than the last
   one applied (a lower one is a rollback and is refused). If its snapshot or
   any object it references cannot be read and verified, fall back to that
   writer's previous complete generation and report it; never treat missing
   data as deletion.
3. **Scan.** Walk every enrolled vault without following links, normalize
   paths to NFC, and compare size and modification time with the index,
   hashing only changed files. A changed or removed entry gets
   `vv[self] = ++counter`; removal becomes a tombstone. A vault whose root is
   missing or unreadable is skipped entirely and never produces tombstones;
   neither does a partially unreadable tree, a file that cannot be read, or one
   over the size limit. A vault whose encryption manifest is in the
   `encrypting` or `decrypting` phase is skipped until the transition
   finishes. New objects are encrypted and uploaded as soon as they are hashed,
   from the same bytes, so an upload never races a later edit. Revisions are
   read incrementally from a per-vault row watermark.
4. **Merge** every entry key across the local index and each remote snapshot:
   - equal version vectors, or equal values: keep, take the pointwise maximum;
   - one vector dominates: the dominating value wins;
   - concurrent:
     - edit versus tombstone: the edit wins (no silent deletion);
     - two different file versions: the winner is the higher
       `(modified_at, object id)` and keeps the path. The other version is
       written beside it as `<stem>.sync-conflict-<first 8 hex of its object
       id>.<ext>`, a deterministic name, so every device materializes the same
       copy exactly once. The conflict copy is an ordinary new file;
     - `.denote/encryption.json`: any concurrent change, including deletion,
       pauses that vault and asks the user to keep the local or remote
       manifest; nothing in the vault is applied meanwhile;
     - plugin state or plugin setting: keep the local entry, version vector
       included, and record the remote value as a pending conflict the user
       resolves in the panel. Because neither side absorbs the other's
       version, both devices keep their own value without overwriting each
       other until someone chooses; the choice is a new version that dominates
       both. A device without a value of its own keeps the later one;
     - revision history: union, never conflicts.
   The merged vector is the pointwise maximum of both inputs, without a local
   increment, so devices resolving the same conflict converge on the same
   entry.
5. **Prepare.** Download and verify every object needed locally into a staging
   directory in application data.
6. **Apply** under the vault's native mutation lock. Immediately before each
   write the current file is revalidated against the scanned size,
   modification time, and hash. A file changed since the scan is left alone
   and retried next cycle. Files are written with a temporary file and rename,
   with case-insensitive and Unicode collision checks and confined resolution
   under the vault root. Denote's existing external-change detection then
   reloads clean tabs and reports a conflict for dirty ones instead of
   overwriting. Preferences and plugin data are applied through their normal
   validated import paths, after which affected plugin workers restart.
7. **Publish.** Upload every object referenced by the merged state that the
   remote does not already have (re-reading it from this device's own disk when
   needed); an entry whose object is not available is left out of this
   snapshot, which tells other devices nothing rather than something wrong.
   The journal records the generation and counter, then the cycle writes
   `heads/<writer>/<generation + 1>` and calls `backend.flush()`. Only after
   flush succeeds does one transaction commit the new generation, counter,
   index, and cleared journal. A cycle that changed nothing publishes nothing,
   so settled devices stop writing generations. If a device finds its own
   writer's generation that the journal says it was publishing when it was
   interrupted, it merges that snapshot as its own instead of re-enrolling.

### Writer identity

A writer ID is a random value generated when a device enrolls. If, before
publishing, a device finds a remote generation for its own writer that is
higher than its own counter state, its application data was restored or
copied. It then abandons the writer ID, enrolls a fresh one, and treats its
whole local state as unconfirmed: every local entry is compared as
concurrent, so nothing is lost and divergent files become conflict copies.

### Device retirement

**Remove device** in the panel writes `retired/<writer>`. Retired writers'
heads are ignored. A retired device that syncs again detects the record and
re-enrolls under a new writer ID, with the same unconfirmed comparison. Tombstones are retained
indefinitely and objects are never deleted automatically in format 1.

### New devices and vault mapping

Joining an existing repository requires the passphrase. Each remote vault is
offered in the panel. The user chooses a new empty folder, an existing folder
that already contains that vault, or **Skip on this device**. Existing folders
are merged with the unconfirmed comparison. Overlapping vault folders and a
folder backend inside a vault are rejected. **Stop syncing this vault** keeps
local files and stops publishing it; forgetting a vault in the vault switcher
never deletes it remotely.

## Scheduling and lifecycle

The host scheduler runs a cycle on the configured interval (default 15
minutes, 0 for manual only), shortly after the application starts, and when
**Sync now** is pressed. It never runs while the application is locked, in
safe mode, or after the plugin is disabled, crashes, or is being updated;
`stop_sensitive_tools`, revoking a plugin, locking the app, or locking or
switching a vault cancels a running cycle at the next file boundary, and the
journal makes the cancelled cycle restartable. A cancelled or failed cycle
commits nothing; files it already applied are recognized by content on the
next scan. Disabling the plugin keeps
the sync configuration; **Disconnect** removes the configuration, the local
index, and the keychain entries, and never deletes remote data or local files.

## Native commands

All commands take the calling `plugin_id`, require its enabled `sync`
permission, and are registered in `src-tauri/src/lib.rs`:

| Command | Purpose |
| --- | --- |
| `sync_status` | Configuration summary without secrets, state, last result, devices, vaults, pending conflicts, suggestions |
| `sync_configure` | Create or join a repository with backend settings, credentials, passphrase, and remember flag |
| `sync_connect_google` | Run the Google OAuth PKCE flow and return the account label |
| `sync_unlock` | Unlock with the passphrase when it is not remembered |
| `sync_run` / `sync_cancel` | Manual cycle and cancellation |
| `sync_set_options` | Interval, paused flag, history and preferences toggles |
| `sync_map_vault` | Map, merge, or skip a remote vault; enroll or stop a local vault |
| `sync_resolve` | Resolve a pending encryption-manifest, plugin-setting, or plugin-state conflict |
| `sync_put_preferences` | Renderer pushes the portable preferences allowlist |
| `sync_retire_device` | Retire another writer |
| `sync_change_passphrase` | Rewrap the master key |
| `sync_disconnect` | Remove local configuration and secrets |
| `sync_choose_folder` | Open the host folder chooser for a folder backend (`purpose: "backend"`) or a `map-new` vault folder (`purpose: "vault"`); returns the absolute path or `null` when cancelled |

Events: `denote://sync-status` carries the status view after every state
change (setup, unlock, options, mapping, resolution, retirement, passphrase
change, disconnect, and a cycle's start, progress, and end), and
`denote://sync-applied` lists the vault roots (the exact `vaults.path` /
`WorkspaceSnapshot.vaultPath` string), preference keys, and plugin IDs a cycle
changed so the renderer can refresh trees, apply preferences, and restart
affected plugin workers. Once configured, only the plugin that set sync up may
call the commands. `sync_status` answers in every state, including before
setup, with no vault open, and while the open vault is locked.

`sync_put_preferences` receives the whole allowlist each time; a missing key
or `null` means the preference is not set on this device. Only keys whose value
differs from the recorded one become local changes. Transports keep private
state only under `sync/backends/<kind>/` in application data, which
**Disconnect** removes together with `sync/staging/`.

### Command payloads

Rust types use `#[serde(rename_all = "camelCase")]`; Tauri maps `pluginId` to
`plugin_id`. Secrets travel only renderer to host and are never returned.

```ts
type SyncBackendRequest =
  | { kind: "folder"; path: string }
  | { kind: "webdav"; url: string; username: string; password: string }
  | { kind: "s3"; endpoint: string; region: string; bucket: string; prefix: string;
      pathStyle: boolean; accessKeyId: string; secretAccessKey: string }
  | { kind: "google-drive"; connectionToken: string; folderName: string }
  | { kind: "git"; remoteUrl: string; branch: string;
      authMode: "system" | "public" | "ssh-agent" | "github-https" };

interface SyncSetupRequest {
  mode: "create" | "join";
  deviceName: string;
  passphrase: string;
  rememberPassphrase: boolean;
  backend: SyncBackendRequest;
}

interface SyncOptions {
  intervalMinutes: number;      // 0 = manual only, max 1440
  paused: boolean;
  syncHistory: boolean;
  syncPreferences: boolean;
  syncPluginData: boolean;
}

type SyncVaultRequest =
  | { action: "map-new"; syncId: string; path: string }      // empty or new folder
  | { action: "map-existing"; syncId: string; scopeId: string }
  | { action: "skip"; syncId: string }
  | { action: "enroll"; scopeId: string }
  | { action: "stop"; syncId: string };

interface SyncStatusView {
  state: "not-configured" | "needs-passphrase" | "idle" | "running" | "paused" | "error";
  backend: null | { kind: SyncBackendRequest["kind"]; label: string };
  rememberPassphrase: boolean;
  options: SyncOptions;
  progress: null | { phase: "refresh" | "scan" | "merge" | "download" | "apply" | "upload" | "publish";
                     done: number; total: number };
  lastRun: null | { finishedAt: string; ok: boolean; message: string; uploaded: number;
                    downloaded: number; applied: number; conflicts: number; skipped: string[] };
  device: null | { writerId: string; name: string };
  devices: { writerId: string; name: string; lastGeneration: number; lastSeenAt: string | null;
             retired: boolean; isSelf: boolean }[];
  vaults: { syncId: string | null; name: string; scopeId: string | null; path: string | null;
            state: "synced" | "not-enrolled" | "pending-mapping" | "skipped" | "paused-conflict"
                 | "paused-encryption" | "unavailable";
            message: string | null }[];
  conflicts: { id: string; kind: "encryption-manifest" | "plugin-setting" | "plugin-state";
               vaultSyncId: string | null; pluginId: string | null; key: string | null;
               localSummary: string; remoteSummary: string; remoteDevice: string;
               detectedAt: string }[];
  pluginSuggestions: { pluginId: string; fromDevice: string }[];
  warnings: string[];
}

// sync_status(pluginId) -> SyncStatusView
// sync_configure(pluginId, request: SyncSetupRequest) -> SyncStatusView
// sync_connect_google(pluginId, clientId: string | null) -> { connectionToken: string; accountLabel: string }
// sync_unlock(pluginId, passphrase: string, remember: boolean) -> SyncStatusView
// sync_run(pluginId) -> SyncStatusView           (resolves when the cycle ends)
// sync_cancel(pluginId) -> void
// sync_set_options(pluginId, options: SyncOptions) -> SyncStatusView
// sync_map_vault(pluginId, request: SyncVaultRequest) -> SyncStatusView
// sync_resolve(pluginId, conflictId: string, choice: "local" | "remote") -> SyncStatusView
// sync_put_preferences(pluginId, preferences: Record<string, string | null>) -> void
// sync_retire_device(pluginId, writerId: string) -> SyncStatusView
// sync_change_passphrase(pluginId, current: string, next: string) -> SyncStatusView
// sync_disconnect(pluginId) -> SyncStatusView
// sync_choose_folder(pluginId, purpose: "backend" | "vault") -> string | null
// event "denote://sync-status": SyncStatusView
// event "denote://sync-applied": { vaultRoots: string[];
//        preferences: Record<string, string | null>; pluginIds: string[] }
```

The portable preference allowlist is `denote-theme`, `denote-editor-display`,
`denote-markdown-view-mode`, `denote-show-dotfiles`,
`denote.speech-shortcut.v1`, and `denote.plugin-rail.v1`. The host rejects
other keys.
