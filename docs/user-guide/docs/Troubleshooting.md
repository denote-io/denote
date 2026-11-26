# Troubleshooting

Each section names the message or symptom you see and the safest way to
recover. None of these steps deletes notes.

## A plugin prevents normal use

1. Quit Denote completely.
2. Start it in [safe mode](<Optional plugins.md#startup-safe-mode>). Enabled
   plugins show **Paused in safe mode** and run no code.
3. Open **Settings → Plugins** and choose **Disable and remove code** for the
   suspected plugin, or **Disable all plugins** under **Plugin recovery**.
4. Restart Denote normally.

Safe mode keeps settings and enablement, so restarting without the flag
resumes every plugin you did not disable.

## A plugin will not install or update

| Message | What it means | What to do |
| --- | --- | --- |
| **HTTP 404 Not Found** | The package named by this Denote build is not published | Install the latest Denote release; retrying cannot fix a missing package |
| **failed SHA-256 verification** or **size does not match catalog metadata** | The download was incomplete or changed in transit | Retry on a trusted network. Never replace a plugin package by hand |
| **requires Denote … or newer**, **requires a Denote version below …**, or **requires API version …** | The plugin and this Denote version are incompatible | Update Denote. Incompatible packages are never downloaded |
| **does not support process execution on …** | The plugin has no approved tool for this operating system | Use a supported system or another plugin |
| **was revoked** | Denote withdrew that plugin version | Update Denote; a revoked version is removed before it runs |
| **Open or unlock a vault before enabling this plugin.** | The plugin needs note content | Open or unlock a vault, then retry |

A failed update keeps the installed version running. You do not need to disable
a plugin to retry its update. Report a repeated failure with the plugin ID,
version, and message.

## A plugin stopped or was removed

- **A plugin crashed.** Denote stops it, removes its code, and keeps the error
  visible. Its settings remain. Enable it again; if it keeps failing, report it.
- **The installed plugin package is missing, incompatible, revoked, or failed
  its integrity check.** At startup, Denote found plugin code it could not
  trust and removed it before it ran. Enable the plugin again to download a
  verified copy.
- **Recovered an interrupted plugin enablement.** Denote quit while a plugin
  was being enabled or updated. The incomplete package was removed and any
  earlier version kept. Retry the action.
- **Plugins stop when you lock a vault.** Plugins that read note content pause
  while an encrypted vault is locked and resume after unlock. Metadata-only
  clocks and reminders keep running; Git and native tools refuse vault
  operations until unlock.

## Denote reports a plugin manager problem

**Another Denote process is managing plugins** means a second Denote process is
still running. Quit every Denote window, wait for it to exit, and start it
again.

**Plugin recovery failed. Check application-data permissions and restart**
means Denote could not repair its plugin folder. Check that your account can
write the application data folder listed in
[Data, backups, and removal](<Data, backups, and removal.md>), then restart.
Plugins stay disabled until recovery succeeds; the editor keeps working.

## An application update fails

Open **About Denote** and choose **Check for updates**. An update that fails
verification is not installed, and the current version keeps running. **The
stable update channel is not configured with a trusted public key** means this
build cannot update itself; download the latest release instead and verify its
checksum. **GitHub returned HTTP 403 while listing Denote beta releases** usually
means GitHub's request limit was reached; try again later, or clear **Include
beta releases** to use the stable channel. On macOS, see
[Opening an unsigned macOS build](<Getting started.md#opening-an-unsigned-macos-build>)
when the system reports that the app is damaged.

## A save conflicts with another application

When another application changes an open file, Denote reports a conflict
instead of overwriting either version. Compare both versions and keep the one
you want. Use **History** to restore one of the last ten saved versions.

## Sync reports a problem

| Symptom | What to do |
| --- | --- |
| **Locked until you enter the sync passphrase** | Unlock in the Sync panel. Turn on **Remember the passphrase on this device** to skip this after restarts. |
| Authentication, network, or storage errors | Check the storage URL, credentials, and the computer's clock. Sync retries later and never treats an unreachable storage as deleted data. |
| A vault shows **Folder unavailable** | Reconnect the drive or folder. Nothing is deleted remotely while it is missing. |
| A vault is paused for encryption | Finish enabling or disabling vault encryption, or resolve its encryption conflict under **Conflicts**. |
| Files named `.sync-conflict-` appear | Two devices changed the same file. Keep the version you want and delete the other. |
| The passphrase is forgotten on every device | The remote data cannot be decrypted. **Disconnect**, then create new storage from a device that still has your files. |

See [Sync](<Sync.md>) for details.

## An encrypted vault will not unlock

Use an unused recovery code when you forget the password. Each code works once.
Without the password or a recovery code, the contents cannot be recovered.
An interrupted encryption or decryption resumes after the next unlock. See
[Vault encryption](<Vault encryption.md>).

## Report a problem

Choose **Report a bug** in **About Denote**. Denote opens an editable draft
with redacted diagnostics and no note contents or vault paths. Add the steps
that reproduce the problem and any plugin ID and version. Describe the problem
with an invented example; never attach real notes, credentials, or recovery
codes.

#guide #recovery
