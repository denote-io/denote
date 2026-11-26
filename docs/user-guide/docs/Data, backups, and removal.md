# Data, backups, and removal

## Your files stay yours

A vault is an ordinary folder. Denote edits its files in place and never moves
them into an account, server, or proprietary database. The optional
[Sync](<Sync.md>) plugin copies end-to-end encrypted data only to storage you
choose; your vault folders stay ordinary folders on every device. Notes, boards, task
markers, decks, and voice recordings stay readable without Denote or its
plugins. Removing a plugin or uninstalling Denote never deletes vault content.

## Where Denote keeps data

| Data | Location | Vault encryption | Removed by |
| --- | --- | --- | --- |
| Notes, folders, attachments, and plugin-authored files such as `assets/voice-<UUID>.wav` | The vault folder | Encrypted | Only you |
| Denote Trash | `.denote/trash` in the vault | Encrypted | **Empty Trash** |
| Encryption manifest | `.denote/encryption.json` in the vault | Wrapped keys only | Disabling encryption |
| Note comments | `.denote/note-comments.json` in the vault | Encrypted | Only you |
| Credential store | `.denote/credentials.dncred` in the vault | Separately encrypted | Only you |
| Git history | `.git` in the vault | Commits contain ciphertext; repository metadata is plaintext | Only you |
| Known vaults, counters, revision history, tab session, preferences, and app lock settings | `denote.sqlite3` in application data | Revision contents are encrypted; other metadata is not | Uninstall cleanup |
| Built-in **Denote Welcome** vault | `Denote Welcome` in application data | Optional | Uninstall cleanup |
| Plugin code, approvals, settings, and generated data such as reminders and time-tracking sessions | `plugins` in application data | Not encrypted | **Disable and remove code** removes code; **Delete saved data** removes settings and generated data |
| Speech model and engine | `speech` in application data | Public model files | **Remove local model and engine** |
| Temporary plugin downloads and update recovery copies | Application cache | Not applicable | Automatically after use |
| Sync settings, device identity, and sync index | `sync` in application data | Configuration and index are local metadata; nothing is uploaded unencrypted | **Disconnect** in the Sync panel |
| Encrypted sync copies of enrolled vaults, preferences, history, and plugin data | The folder, server, bucket, Google Drive folder, or Git repository you chose | End-to-end encrypted with the sync passphrase | Only you |
| Plugin credentials, saved Git passphrases, sync storage credentials, and a remembered sync passphrase | macOS Keychain, Windows Credential Manager, or Linux Secret Service | Operating-system protection | **Delete credentials**, **Delete saved passphrase**, or sync **Disconnect** |

Application data and cache folders use the identifier `dev.mbianchi.denote`:

| System | Application data | Cache |
| --- | --- | --- |
| macOS | `~/Library/Application Support/dev.mbianchi.denote` | `~/Library/Caches/dev.mbianchi.denote` |
| Windows | `%APPDATA%\dev.mbianchi.denote` | `%LOCALAPPDATA%\dev.mbianchi.denote` |
| Linux | `~/.local/share/dev.mbianchi.denote` | `~/.cache/dev.mbianchi.denote` |

Linux uses `$XDG_DATA_HOME` and `$XDG_CACHE_HOME` instead when they are set.
**Denote Development** builds use the separate `dev.mbianchi.denote.development`
identifier and never share data with an installed release.

## Back up a vault

1. Quit Denote, or lock an encrypted vault, so pending saves finish.
2. Copy the entire vault folder, including hidden `.denote` and any `.git`
   folder.
3. For an encrypted vault, keep `.denote/encryption.json` with the copy, and
   store your recovery codes outside the vault.
4. Keep the credential store's master password separately. **Encrypted
   backup** in the credential store copies only its encrypted container.

Revision history, counters, reminders, and time-tracking sessions live in
application data, not in the vault. A vault copy does not include them. Use
**Export CSV** or **Export JSON** for time tracking, or export plugin settings as
JSON, before moving to another computer. Exports are plaintext files.

## Sync between devices

The optional [Sync](<Sync.md>) plugin replicates enrolled vaults, portable
preferences, revision history, and plugin data between your devices through a
folder, WebDAV, S3-compatible storage, Google Drive, or a dedicated Git
repository. Everything leaves the device end-to-end encrypted with a sync
passphrase that Denote cannot recover. Sync is not a backup: a deletion or a
mistaken edit reaches every device, so keep separate backups too.

You can also keep a vault in a folder your own sync or backup tool manages.
When sync or another application changes an open file, Denote reloads a clean
tab and reports a conflict for unsaved edits instead of overwriting either
version.

## Restore a vault

Copy the vault folder back, then choose **Open another folder** in the vault
switcher. An encrypted vault unlocks with its password or an unused recovery
code. Restore `.denote/note-comments.json` and `.denote/credentials.dncred`
together with the notes they belong to.

## Remove a plugin

**Disable and remove code** stops the plugin and deletes its downloaded package,
cache, and staging files. Its settings and generated data stay until you choose
**Delete saved data**; saved credentials stay until **Delete credentials**. No
plugin action deletes notes, boards, comments, recordings, or other files you
created. **Disable all plugins** in **Plugin recovery** applies the same removal
to every plugin.

## Remove a vault from Denote

In the vault switcher, the remove action forgets a vault and leaves its folder
untouched. Only the optional checkbox moves the folder to the operating system
Trash.

## Uninstall Denote

Uninstalling removes the application. Your vault folders stay where they are.

- **macOS:** Quit Denote and move `Denote.app` to the Trash. Application data,
  the cache, and `~/Library/WebKit/dev.mbianchi.denote` remain until you delete
  them.
- **Windows:** Uninstall Denote from **Settings → Apps**. For the `.exe`
  installer, select **Delete the application data** to remove both Denote
  folders listed above. The `.msi` package and an uninstall without that option
  leave them; delete them yourself.
- **Linux:** Remove the package with your package manager, or delete the
  AppImage. Application data and the cache remain until you delete them.

Deleting application data also deletes the built-in Denote Welcome vault,
revision history, plugin settings, reminders, time-tracking sessions, and the
speech model. Copy anything you want to keep first. Before uninstalling, use
**Delete credentials** for plugins with saved credentials and **Disconnect** in
the Sync panel; otherwise remove their `dev.denote.plugin` entries with your
system's keychain tool. Uninstalling never deletes data in your sync storage.

[Next: Sync](<Sync.md>)

#guide #recovery #privacy
