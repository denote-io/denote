# Sync practice

Use a throwaway folder and an invented passphrase for this exercise. Opening
this note does not enable a plugin, create sync storage, or upload anything.

Enable **Whole-app sync** in **Settings → Plugins**, then choose **Sync** in the
activity rail or command palette. Under **Start**, choose **Create new sync
storage** and name this device. For **Storage**, keep **Local or mounted
folder** and use **Choose folder…** to pick a new, empty folder outside every
vault, such as `Sync practice` in your Documents folder. Enter an invented
passphrase such as `invented practice passphrase` in both passphrase fields.
A forgotten real passphrase cannot be recovered, and the stored data cannot be
read without it.

In the vault list, choose **Sync this vault**, then **Sync now**. The practice
folder fills with opaque files under `denote-sync/v1/`: no note name or text is
readable there. Edit this note, sync again, and check **Last sync**.

On a second computer, **Join sync from another device** with the same storage
and passphrase, then map this vault to a folder. When both devices change one
note before syncing, both versions stay: the other appears beside it as
`<name>.sync-conflict-<id>.md`. Use Tab, Shift-Tab, Enter, Space, and Escape.

**Disconnect this device…** removes this device's sync settings and saved
secrets. It never deletes local files or the practice folder; delete that
folder yourself afterwards.

[Sync guide](<../docs/Sync.md>) | [Optional plugin guide](<../docs/Optional plugins.md#whole-app-sync>)
