# Sync

The optional **Whole-app sync** plugin keeps several computers in step. Every
vault you choose to sync, your portable preferences, revision history, and
plugin data replicate through one storage location you control: a local or
mounted folder, WebDAV, an S3-compatible bucket, Google Drive, or a dedicated
Git repository. Sync is two-way: each device publishes its own changes and
merges what the others published.

Everything stored remotely is end-to-end encrypted with a **sync passphrase**
before it leaves the device. The storage provider sees only opaque object names
and ciphertext. Sync is separate from [Git vault versioning](<Optional plugins.md#other-optional-features>),
which keeps the history of one vault in that vault's own repository; the two
can be used together.

>![warning]
> Denote cannot recover or reset the sync passphrase. If every device forgets
> it, the data in your sync storage is unrecoverable. Keep it in a password
> manager.

## Turn sync on

1. Open **Settings → Plugins**, find **Whole-app sync**, and choose **Enable**.
   Its only permission, **Sync**, lets the plugin add the Sync panel. Denote's
   own host does all storage, encryption, and file work; the downloaded plugin
   code never sees your notes, paths, preferences, credentials, or passphrase.
   Enabling does not configure or start anything.
2. Open **Sync** from the activity rail (circular arrows), the command palette,
   the Welcome screen, or the locked-vault screen. Sync works with any vault
   open, and also before you open or unlock one.

### First device

1. Keep **Create new sync storage** selected and give the device a name you will
   recognize, such as “Work laptop”.
2. Choose the storage and fill in its settings (see below).
3. Enter a new sync passphrase of at least 12 characters, then confirm it. The
   strength hint is only a guide; several unrelated words make a good
   passphrase.
4. Leave **Remember the passphrase on this device** on to let background sync
   run after a restart. The passphrase is then kept in the operating-system
   keychain. Turn it off to type it again after each start.
5. Choose **Create sync**. Then enroll each vault you want to sync with
   **Sync this vault** in the **Vaults** list.

### More devices

1. Choose **Join sync from another device**, enter the same storage settings,
   and the passphrase you chose on the first device.
2. For every remote vault in the **Vaults** list choose one of:
   - **Choose new folder…** to download it into an empty or new folder;
   - **Merge** into an existing vault on this device that already holds a copy.
     Files that differ are kept as conflict copies, never overwritten;
   - **Skip on this device** to leave it out here.

## Storage settings

Use storage that only you control, and keep it outside every vault.

### Folder

Choose a folder with **Choose folder…** or type its path. A folder that a
desktop sync client (for example Syncthing, Dropbox, or OneDrive) replicates, or
a mounted network share, both work. The folder must not be inside a vault.

### WebDAV

Enter the full URL of a folder on the server, for example
`https://cloud.example.com/remote.php/dav/files/me/Denote`, and a username and
password. For Nextcloud and similar services, create an app password instead of
using your account password. Use `https://` unless the server is on a network
you trust: the data is encrypted either way, but the login is not.

### S3-compatible storage

Create a bucket and an access key limited to it, then enter:

| Setting | AWS S3 | Cloudflare R2 | MinIO | SeaweedFS |
| --- | --- | --- | --- | --- |
| Endpoint URL | `https://s3.<region>.amazonaws.com` | `https://<account-id>.r2.cloudflarestorage.com` | Your server, for example `https://minio.example.com:9000` | The S3 gateway, for example `http://seaweed.local:8333` |
| Region | The bucket's region | `auto` | `us-east-1` unless configured otherwise | `us-east-1` |
| Path-style addressing | Off | Off | On | On |

**Prefix** is optional and lets several stores share one bucket, for example
`denote`. For SeaweedFS, create the access key in its S3 identity configuration
(`s3.json` or `weed shell` → `s3.configure`) and grant it read, write, and list
on the bucket. For MinIO, attach a policy that allows `s3:GetObject`,
`s3:PutObject`, `s3:DeleteObject`, and `s3:ListBucket` on that bucket only.

### Google Drive

Choose **Connect Google Drive**. Your browser opens Google's sign-in page;
after you approve, the panel shows the connected account. Denote asks only for
access to files it creates, in a folder named **Denote Sync** unless you change
it.

If you prefer your own Google Cloud project, open **Use your own Google OAuth
client** and enter the client ID of a **Desktop app** OAuth client before
connecting. The client ID is not a secret; it only decides which project the
sign-in belongs to.

### Git

Create a **new, empty, private** repository used only for sync, and enter its
remote URL and branch. Never use one of your vault repositories: sync commits
only encrypted objects under `denote-sync/v1/` and never force-pushes, but it
would mix unrelated history into your vault. Authentication can use your system
Git credentials, the SSH agent, a public repository, or GitHub over HTTPS with a
token stored in the keychain.

## Daily use

The **Status** section shows the storage, this device, the schedule, progress,
and the last sync. Sync runs shortly after Denote starts, on the interval in
**Options** (15 minutes by default; `0` means only when you choose **Sync now**),
and whenever you choose **Sync now**. **Cancel sync** stops a running sync at the
next file; nothing is half-written. **Pause automatic sync** keeps **Sync now**
available.

Sync never runs while Denote is locked or in safe mode. Open tabs with unsaved
edits are never replaced: Denote reports a conflict instead, the same way it does
for changes made by other applications. Preferences received from another
device apply immediately, and plugins whose settings changed restart on their
own.

**Options** also choose whether revision history, preferences, and plugin
settings and data are synced. These choices stay on this device.

### Deleted files and missing folders

A file deleted on another device is moved to this device's **Trash**, never
erased, so you can restore it from **Trash** (restoring syncs it back). Denote's
own files inside a vault's `.denote` folder that another device deleted are kept
in `.denote/trash/sync/` instead.

If a vault's folder suddenly looks empty, for example because its drive is
unplugged or a cloud folder is offline, its `.denote` folder is gone too and the
vault shows **Folder unavailable** until the files are back. Nothing is deleted
on your other devices meanwhile.

When most of a vault's files disappear at once (at least 20 files and more than
half of the vault), sync pauses that vault and shows **Paused: most files are
missing** with the numbers. Restore the files to resume, or choose **Apply
deletions…** and confirm if you really removed them; the next sync deletes them
on every device, each of which keeps them in its Trash.

## What is synced

Synced: the files of enrolled vaults (encrypted vaults stay encrypted), vault
names and view options, bookmarks, file order and pins, tag colors, project
roots, revision history, theme, editor display, default Markdown view, dotfile
visibility, activity-rail layout, the dictation shortcut, portable plugin
settings and plugin data, and which plugins are enabled on other devices.

Not synced: `.git` folders, the contents of Denote Trash, locks and temporary files, symbolic
links, panel widths, approved link domains and remote-image consent, the update
channel and automatic plugin updates, plugin packages and permission approvals,
plugin settings for executables, paths, authentication, signing, or automation,
app lock, open tabs, caches, and every keychain secret.

Plugins enabled on another device appear under **Plugins on other devices**.
Sync never installs, approves, or enables them; **Review in plugin settings**
opens **Settings → Plugins** so you can decide yourself.

## Conflicts

Deleting never wins over a concurrent edit. When two devices change the same
file before syncing, the newer version keeps the name and the other is saved
beside it as `<name>.sync-conflict-<id>.<ext>`. Compare them, keep what you
need, and delete the other file.

Concurrent changes to a plugin setting, a plugin data value, or a vault's
encryption settings appear under **Conflicts**. Choose **Keep local** or **Use
remote** for each. A vault with an encryption conflict pauses until you choose.

## Devices

**Devices** lists every device that has synced, with the time it last synced.
**Remove** (with confirmation) makes the other devices ignore what a lost or
retired device publishes from now on. It does not take back what that device
already downloaded.

A removed device that tries to sync again stops and shows **This device was
removed from sync. Rejoin to continue.** It does not sync, automatically or with
**Sync now**, until you choose **Rejoin sync** on it. Rejoining makes it a new
device: its files are compared with your other devices, and versions that
differ are kept as conflict copies, so nothing it holds is lost.

## Passphrase

**Change passphrase** re-protects the same encryption key with a new
passphrase; other devices ask for the new one. Devices that knew the old
passphrase could already read the data, so to rotate completely, disconnect
every device and create new storage from a device that has all files.

If you forget the passphrase on one device, unlock with it from another device
that remembers it, or **Change passphrase** there. If no device remembers it,
the remote data cannot be decrypted: disconnect, then create new storage from a
device that still has your files.

## Stop or disconnect

- **Stop syncing** on a vault keeps its local files and stops publishing it.
  Forgetting a vault in the vault switcher never deletes it remotely.
- Disabling the plugin stops sync and keeps its configuration, so re-enabling
  resumes.
- **Disconnect this device…** removes this device's sync settings, sync index,
  saved storage credentials, and remembered passphrase after you confirm. Local
  files and the data in your storage are kept; delete the remote folder, bucket,
  or repository yourself if you no longer need it.

The protocol and remote format are described in the
[sync reference](https://github.com/denote-io/denote/blob/main/docs/sync.md).

[Next: Troubleshooting](<Troubleshooting.md>)

#guide #sync #privacy
