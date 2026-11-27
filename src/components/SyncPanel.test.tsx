import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { SyncStatusView } from "../lib/sync";
import { SyncPanel } from "./SyncPanel";

const native = vi.hoisted(() => ({
  status: vi.fn(), configure: vi.fn(), connectGoogle: vi.fn(), unlock: vi.fn(), run: vi.fn(), cancel: vi.fn(),
  setOptions: vi.fn(), mapVault: vi.fn(), resolve: vi.fn(), retire: vi.fn(), rejoin: vi.fn(), changePassphrase: vi.fn(),
  disconnect: vi.fn(), chooseFolder: vi.fn(),
  statusHandler: null as ((status: SyncStatusView) => void) | null,
}));
vi.mock("../lib/api", async (original) => ({
  ...await original<typeof import("../lib/api")>(),
  api: {
    syncStatus: native.status, syncConfigure: native.configure, syncConnectGoogle: native.connectGoogle,
    syncUnlock: native.unlock, syncRun: native.run, syncCancel: native.cancel, syncSetOptions: native.setOptions,
    syncMapVault: native.mapVault, syncResolve: native.resolve, syncRetireDevice: native.retire, syncRejoin: native.rejoin,
    syncChangePassphrase: native.changePassphrase, syncDisconnect: native.disconnect, syncChooseFolder: native.chooseFolder,
    onSyncStatus: vi.fn(async (handler: (status: SyncStatusView) => void) => {
      native.statusHandler = handler;
      return () => { native.statusHandler = null; };
    }),
  },
}));

const PLUGIN = "denote.sync";
const SELF = "a".repeat(32);
const TABLET = "b".repeat(32);
const PASSPHRASE = "synthetic correct horse battery";
const SECRET_KEY = "synthetic-secret-access-key";

function view(overrides: Partial<SyncStatusView> = {}): SyncStatusView {
  return {
    state: "idle",
    backend: { kind: "s3", label: "synthetic-bucket at s3.synthetic.invalid" },
    rememberPassphrase: true,
    options: { intervalMinutes: 15, paused: false, syncHistory: true, syncPreferences: true, syncPluginData: true },
    progress: null,
    lastRun: {
      finishedAt: "2026-11-26T10:00:00Z", ok: true, message: "", uploaded: 1, downloaded: 2, applied: 2, conflicts: 0, skipped: [],
    },
    device: { writerId: SELF, name: "Synthetic laptop", removed: false },
    devices: [
      { writerId: SELF, name: "Synthetic laptop", lastGeneration: 3, lastSeenAt: "2026-11-26T10:00:00Z", retired: false, isSelf: true },
      { writerId: TABLET, name: "Synthetic tablet", lastGeneration: 7, lastSeenAt: "2026-11-25T09:00:00Z", retired: false, isSelf: false },
    ],
    vaults: [
      { syncId: "sync-alpha", name: "Alpha notes", scopeId: "scope-alpha", path: "/synthetic/alpha", state: "synced", message: null },
      { syncId: "sync-beta", name: "Beta notes", scopeId: null, path: null, state: "pending-mapping", message: null },
      { syncId: null, name: "Gamma notes", scopeId: "scope-gamma", path: "/synthetic/gamma", state: "not-enrolled", message: null },
    ],
    conflicts: [],
    pluginSuggestions: [],
    warnings: [],
    ...overrides,
  };
}

function secretInputs(): HTMLInputElement[] {
  return [...document.querySelectorAll<HTMLInputElement>("input[type=password]")];
}

beforeEach(() => {
  vi.clearAllMocks();
  native.statusHandler = null;
  native.status.mockResolvedValue(view({ state: "not-configured", backend: null, device: null, devices: [], vaults: [], lastRun: null }));
  native.configure.mockResolvedValue(view());
  native.unlock.mockResolvedValue(view());
  native.setOptions.mockImplementation(async (_plugin: string, options: SyncStatusView["options"]) => view({ options }));
  native.mapVault.mockResolvedValue(view());
  native.resolve.mockResolvedValue(view());
  native.retire.mockResolvedValue(view());
  native.disconnect.mockResolvedValue(view({ state: "not-configured", backend: null, device: null, devices: [], vaults: [] }));
  native.run.mockResolvedValue(view());
  native.cancel.mockResolvedValue(undefined);
});

describe("SyncPanel setup", () => {
  it("validates S3 setup, sends the exact request, and keeps no secret after submitting", async () => {
    const user = userEvent.setup();
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    const form = await screen.findByRole("form", { name: "Set up sync" });
    await user.type(within(form).getByLabelText("Device name"), "Synthetic laptop");
    await user.selectOptions(within(form).getByLabelText("Storage"), "s3");
    await user.type(within(form).getByLabelText("Endpoint URL"), "https://s3.synthetic.invalid");
    await user.clear(within(form).getByLabelText("Region"));
    await user.type(within(form).getByLabelText("Region"), "auto");
    await user.type(within(form).getByLabelText("Bucket"), "synthetic-bucket");
    await user.type(within(form).getByLabelText("Prefix (optional)"), "/denote/");
    await user.click(within(form).getByLabelText(/path-style addressing/));
    await user.type(within(form).getByLabelText("Access key ID"), "SYNTHETICKEY");
    await user.type(within(form).getByLabelText("Secret access key"), SECRET_KEY);
    await user.type(within(form).getByLabelText("New sync passphrase"), "short");
    expect(screen.getByText(/Too short/)).toBeInTheDocument();
    await user.click(within(form).getByRole("button", { name: "Create sync" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("at least 12 characters");

    await user.clear(within(form).getByLabelText("New sync passphrase"));
    await user.type(within(form).getByLabelText("New sync passphrase"), PASSPHRASE);
    await user.type(within(form).getByLabelText("Confirm sync passphrase"), `${PASSPHRASE}x`);
    await user.click(within(form).getByRole("button", { name: "Create sync" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("The passphrases do not match.");
    expect(within(form).getByLabelText("Confirm sync passphrase")).toHaveValue("");
    expect(native.configure).not.toHaveBeenCalled();

    let finish: (status: SyncStatusView) => void = () => {};
    native.configure.mockReturnValueOnce(new Promise((resolve) => { finish = resolve; }));
    await user.type(within(form).getByLabelText("Confirm sync passphrase"), PASSPHRASE);
    expect(screen.getByText(/lost|unrecoverable/i)).toBeInTheDocument();
    await user.click(within(form).getByRole("button", { name: "Create sync" }));

    expect(native.configure).toHaveBeenCalledWith(PLUGIN, {
      mode: "create", deviceName: "Synthetic laptop", passphrase: PASSPHRASE, rememberPassphrase: true,
      backend: {
        kind: "s3", endpoint: "https://s3.synthetic.invalid", region: "auto", bucket: "synthetic-bucket", prefix: "denote",
        pathStyle: true, accessKeyId: "SYNTHETICKEY", secretAccessKey: SECRET_KEY,
      },
    });
    for (const input of secretInputs()) expect(input).toHaveValue("");
    expect(document.body.innerHTML).not.toContain(PASSPHRASE);
    expect(document.body.innerHTML).not.toContain(SECRET_KEY);
    await act(async () => { finish(view()); });
    expect(await screen.findByRole("heading", { name: "Status" })).toBeInTheDocument();
  });

  it("clears secrets when the host rejects setup", async () => {
    const user = userEvent.setup();
    native.configure.mockRejectedValueOnce(new Error("Synthetic storage refused the credentials."));
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    const form = await screen.findByRole("form", { name: "Set up sync" });
    await user.type(within(form).getByLabelText("Device name"), "Synthetic laptop");
    await user.selectOptions(within(form).getByLabelText("Storage"), "webdav");
    await user.type(within(form).getByLabelText("WebDAV URL"), "ftp://dav.synthetic.invalid");
    await user.type(within(form).getByLabelText("Password or app token"), "synthetic-dav-token");
    await user.type(within(form).getByLabelText("New sync passphrase"), PASSPHRASE);
    await user.type(within(form).getByLabelText("Confirm sync passphrase"), PASSPHRASE);
    await user.click(within(form).getByRole("button", { name: "Create sync" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("https:// or http://");
    expect(native.configure).not.toHaveBeenCalled();

    await user.clear(within(form).getByLabelText("WebDAV URL"));
    await user.type(within(form).getByLabelText("WebDAV URL"), "https://dav.synthetic.invalid/denote");
    await user.click(within(form).getByRole("button", { name: "Create sync" }));
    expect(await screen.findByText("Synthetic storage refused the credentials.")).toBeInTheDocument();
    expect(native.configure.mock.calls[0][1].backend).toEqual({
      kind: "webdav", url: "https://dav.synthetic.invalid/denote", username: "", password: "synthetic-dav-token",
    });
    for (const input of secretInputs()) expect(input).toHaveValue("");
  });

  it("joins through Google Drive with a custom client ID and a chosen folder backend", async () => {
    const user = userEvent.setup();
    native.connectGoogle.mockResolvedValue({ connectionToken: "synthetic-connection", accountLabel: "synthetic@example.invalid" });
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    const form = await screen.findByRole("form", { name: "Set up sync" });
    await user.click(within(form).getByLabelText("Join sync from another device"));
    expect(within(form).queryByLabelText("Confirm sync passphrase")).not.toBeInTheDocument();
    await user.type(within(form).getByLabelText("Device name"), "Synthetic tablet");
    await user.selectOptions(within(form).getByLabelText("Storage"), "google-drive");
    await user.type(within(form).getByLabelText("Sync passphrase"), PASSPHRASE);
    await user.click(within(form).getByRole("button", { name: "Join sync" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Connect Google Drive");

    await user.click(within(form).getByText("Use your own Google OAuth client"));
    await user.type(within(form).getByLabelText("OAuth client ID (optional)"), "synthetic-client.apps.invalid");
    await user.click(within(form).getByRole("button", { name: "Connect Google Drive" }));
    expect(native.connectGoogle).toHaveBeenCalledWith(PLUGIN, "synthetic-client.apps.invalid");
    expect(await within(form).findByText("Connected as synthetic@example.invalid.")).toBeInTheDocument();
    await user.click(within(form).getByLabelText(/Remember the passphrase/));
    await user.click(within(form).getByRole("button", { name: "Join sync" }));

    expect(native.configure).toHaveBeenCalledWith(PLUGIN, {
      mode: "join", deviceName: "Synthetic tablet", passphrase: PASSPHRASE, rememberPassphrase: false,
      backend: { kind: "google-drive", connectionToken: "synthetic-connection", folderName: "Denote Sync" },
    });
  });

  it("uses the host folder chooser for a folder backend", async () => {
    const user = userEvent.setup();
    native.chooseFolder.mockResolvedValue("/synthetic/mounted/denote-sync");
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    const form = await screen.findByRole("form", { name: "Set up sync" });
    await user.type(within(form).getByLabelText("Device name"), "Synthetic laptop");
    await user.click(within(form).getByRole("button", { name: "Choose folder…" }));
    expect(native.chooseFolder).toHaveBeenCalledWith(PLUGIN, "backend");
    await waitFor(() => expect(within(form).getByLabelText("Sync folder")).toHaveValue("/synthetic/mounted/denote-sync"));
    await user.type(within(form).getByLabelText("New sync passphrase"), PASSPHRASE);
    await user.type(within(form).getByLabelText("Confirm sync passphrase"), PASSPHRASE);
    await user.click(within(form).getByRole("button", { name: "Create sync" }));
    expect(native.configure.mock.calls[0][1].backend).toEqual({ kind: "folder", path: "/synthetic/mounted/denote-sync" });
  });

  it("collects a dedicated Git remote with its branch and authentication mode", async () => {
    const user = userEvent.setup();
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    const form = await screen.findByRole("form", { name: "Set up sync" });
    await user.type(within(form).getByLabelText("Device name"), "Synthetic laptop");
    await user.selectOptions(within(form).getByLabelText("Storage"), "git");
    await user.type(within(form).getByLabelText("Remote URL"), "git@git.synthetic.invalid:me/denote-sync.git");
    await user.clear(within(form).getByLabelText("Branch"));
    await user.type(within(form).getByLabelText("Branch"), "sync");
    await user.selectOptions(within(form).getByLabelText("Authentication"), "ssh-agent");
    await user.type(within(form).getByLabelText("New sync passphrase"), PASSPHRASE);
    await user.type(within(form).getByLabelText("Confirm sync passphrase"), PASSPHRASE);
    await user.click(within(form).getByRole("button", { name: "Create sync" }));
    expect(native.configure.mock.calls[0][1].backend).toEqual({
      kind: "git", remoteUrl: "git@git.synthetic.invalid:me/denote-sync.git", branch: "sync", authMode: "ssh-agent",
    });
  });
});

describe("SyncPanel when configured", () => {
  it("unlocks with the passphrase and forgets it after submitting", async () => {
    const user = userEvent.setup();
    native.status.mockResolvedValue(view({ state: "needs-passphrase", rememberPassphrase: false }));
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    const form = await screen.findByRole("form", { name: "Unlock sync" });
    expect(within(form).getByLabelText("Remember the passphrase on this device")).not.toBeChecked();
    await user.type(within(form).getByLabelText("Sync passphrase"), PASSPHRASE);
    await user.click(within(form).getByRole("button", { name: "Unlock sync" }));
    expect(native.unlock).toHaveBeenCalledWith(PLUGIN, PASSPHRASE, false);
    expect(await screen.findByRole("heading", { name: "Status" })).toBeInTheDocument();
    expect(document.body.innerHTML).not.toContain(PASSPHRASE);
  });

  it("follows status events, runs and cancels a sync, and pauses automatic sync", async () => {
    const user = userEvent.setup();
    native.status.mockResolvedValue(view());
    let finish: (status: SyncStatusView) => void = () => {};
    native.run.mockReturnValueOnce(new Promise((resolve) => { finish = resolve; }));
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    await user.click(await screen.findByRole("button", { name: "Sync now" }));
    expect(native.run).toHaveBeenCalledWith(PLUGIN);
    act(() => native.statusHandler?.(view({ state: "running", progress: { phase: "upload", done: 3, total: 9 } })));
    expect(screen.getByText("Uploading (3 of 9)")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Cancel sync" }));
    expect(native.cancel).toHaveBeenCalledWith(PLUGIN);
    await act(async () => { finish(view()); });
    expect(await screen.findByRole("button", { name: "Sync now" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Pause automatic sync" }));
    expect(native.setOptions).toHaveBeenCalledWith(PLUGIN, { ...view().options, paused: true });
    expect(await screen.findByRole("button", { name: "Resume automatic sync" })).toHaveAttribute("aria-pressed", "true");
  });

  it("resolves conflicts with an explicit local or remote choice", async () => {
    const user = userEvent.setup();
    const conflicted = view({
      conflicts: [
        {
          id: "conflict-setting", kind: "plugin-setting", vaultSyncId: null, pluginId: "denote.synthetic", key: "layout",
          localSummary: "compact", remoteSummary: "spacious", remoteDevice: "Synthetic tablet", detectedAt: "2026-11-26T09:00:00Z",
        },
        {
          id: "conflict-manifest", kind: "encryption-manifest", vaultSyncId: "sync-alpha", pluginId: null, key: null,
          localSummary: "Unencrypted", remoteSummary: "Encrypted", remoteDevice: "Synthetic tablet", detectedAt: "2026-11-26T09:05:00Z",
        },
      ],
    });
    native.status.mockResolvedValue(conflicted);
    native.resolve.mockResolvedValue(conflicted);
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    expect(await screen.findByRole("heading", { name: "Conflicts (2)" })).toBeInTheDocument();
    expect(screen.getByText("spacious")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Keep local version of Setting layout of denote.synthetic" }));
    expect(native.resolve).toHaveBeenCalledWith(PLUGIN, "conflict-setting", "local");
    await user.click(await screen.findByRole("button", { name: "Use remote version of Encryption settings of Alpha notes" }));
    expect(native.resolve).toHaveBeenCalledWith(PLUGIN, "conflict-manifest", "remote");
  });

  it("maps, skips, enrolls, and stops vaults", async () => {
    const user = userEvent.setup();
    native.status.mockResolvedValue(view());
    native.chooseFolder.mockResolvedValue("/synthetic/new-beta");
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    await user.click(await screen.findByRole("button", { name: "Choose a new folder for Beta notes" }));
    expect(native.chooseFolder).toHaveBeenCalledWith(PLUGIN, "vault");
    await waitFor(() => expect(native.mapVault).toHaveBeenCalledWith(PLUGIN, { action: "map-new", syncId: "sync-beta", path: "/synthetic/new-beta" }));

    await user.selectOptions(screen.getByLabelText("Existing vault"), "scope-gamma");
    await user.click(screen.getByRole("button", { name: "Merge Beta notes into the chosen vault" }));
    expect(native.mapVault).toHaveBeenLastCalledWith(PLUGIN, { action: "map-existing", syncId: "sync-beta", scopeId: "scope-gamma" });
    await user.click(await screen.findByRole("button", { name: "Skip Beta notes on this device" }));
    expect(native.mapVault).toHaveBeenLastCalledWith(PLUGIN, { action: "skip", syncId: "sync-beta" });
    await user.click(await screen.findByRole("button", { name: "Sync Gamma notes" }));
    expect(native.mapVault).toHaveBeenLastCalledWith(PLUGIN, { action: "enroll", scopeId: "scope-gamma" });

    await user.click(await screen.findByRole("button", { name: "Stop syncing Alpha notes" }));
    const confirm = screen.getByRole("group", { name: "Stop syncing Alpha notes?" });
    expect(confirm).toHaveTextContent("Local files stay");
    await user.click(within(confirm).getByRole("button", { name: "Stop syncing" }));
    expect(native.mapVault).toHaveBeenLastCalledWith(PLUGIN, { action: "stop", syncId: "sync-alpha" });
  });

  it("applies held deletions in a vault only after confirmation", async () => {
    const user = userEvent.setup();
    const message = "22 of 27 synced files are missing from this folder. Sync paused this vault instead of deleting them on your other devices.";
    native.status.mockResolvedValue(view({
      vaults: [{ syncId: "sync-alpha", name: "Alpha notes", scopeId: "scope-alpha", path: "/synthetic/alpha", state: "paused-deletions", message }],
    }));
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    expect(await screen.findByText("Paused: most files are missing")).toBeInTheDocument();
    expect(screen.getByText(message)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Apply deletions in Alpha notes" }));
    const confirm = screen.getByRole("group", { name: "Apply deletions in Alpha notes?" });
    expect(confirm).toHaveTextContent("vault trash");
    await user.click(within(confirm).getByRole("button", { name: "Keep paused" }));
    expect(native.mapVault).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Apply deletions in Alpha notes" }));
    await user.click(within(screen.getByRole("group", { name: "Apply deletions in Alpha notes?" }))
      .getByRole("button", { name: "Apply deletions" }));
    expect(native.mapVault).toHaveBeenCalledWith(PLUGIN, { action: "confirm-deletions", syncId: "sync-alpha" });
    expect(screen.getByRole("button", { name: "Stop syncing Alpha notes" })).toBeInTheDocument();
  });

  it("offers only Rejoin sync while this device is removed", async () => {
    const user = userEvent.setup();
    const message = "Application state error: This device was removed from sync. Rejoin to continue.";
    native.status.mockResolvedValue(view({
      state: "error",
      device: { writerId: SELF, name: "Synthetic laptop", removed: true },
      lastRun: { finishedAt: "2026-11-26T10:00:00Z", ok: false, message, uploaded: 0, downloaded: 0, applied: 0, conflicts: 0, skipped: [] },
    }));
    native.rejoin.mockResolvedValue(view());
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    expect(await screen.findByText("This device was removed from sync. Rejoin to continue.")).toBeInTheDocument();
    expect(screen.getByText("Needs attention")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Sync now" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Rejoin sync" }));
    expect(native.rejoin).toHaveBeenCalledWith(PLUGIN);
    expect(native.run).not.toHaveBeenCalled();
    expect(await screen.findByRole("button", { name: "Sync now" })).toBeInTheDocument();
    expect(screen.getByText(/rejoined sync/)).toBeInTheDocument();
  });

  it("removes another device only after confirmation and never offers to remove this device", async () => {
    const user = userEvent.setup();
    native.status.mockResolvedValue(view());
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    await screen.findByRole("heading", { name: "Devices" });
    expect(screen.queryByRole("button", { name: "Remove Synthetic laptop" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Remove Synthetic tablet" }));
    const confirm = screen.getByRole("group", { name: "Remove Synthetic tablet?" });
    await user.click(within(confirm).getByRole("button", { name: "Keep device" }));
    expect(native.retire).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Remove Synthetic tablet" }));
    await user.click(within(screen.getByRole("group", { name: "Remove Synthetic tablet?" })).getByRole("button", { name: "Remove device" }));
    expect(native.retire).toHaveBeenCalledWith(PLUGIN, TABLET);
  });

  it("disconnects only after explaining that local files and remote data are kept", async () => {
    const user = userEvent.setup();
    native.status.mockResolvedValue(view());
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    await user.click(await screen.findByRole("button", { name: "Disconnect this device…" }));
    const confirm = screen.getByRole("group", { name: "Disconnect this device from sync?" });
    expect(confirm).toHaveTextContent("Local vault files and the data in your sync storage are kept");
    await user.click(within(confirm).getByRole("button", { name: "Stay connected" }));
    expect(native.disconnect).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Disconnect this device…" }));
    await user.click(within(screen.getByRole("group", { name: "Disconnect this device from sync?" })).getByRole("button", { name: "Disconnect" }));
    expect(native.disconnect).toHaveBeenCalledWith(PLUGIN);
    expect(await screen.findByRole("form", { name: "Set up sync" })).toBeInTheDocument();
  });

  it("saves validated options and links plugin suggestions to settings without enabling them", async () => {
    const user = userEvent.setup();
    const openSettings = vi.fn();
    native.status.mockResolvedValue(view({
      pluginSuggestions: [{ pluginId: "denote.synthetic", fromDevice: "Synthetic tablet" }],
      warnings: ["Synthetic warning about a skipped file."],
    }));
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} onOpenPluginSettings={openSettings} />);
    expect(await screen.findByText("Synthetic warning about a skipped file.")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Review denote.synthetic in plugin settings" }));
    expect(openSettings).toHaveBeenCalledWith("denote.synthetic");

    await user.click(screen.getByText("Options"));
    const form = screen.getByRole("form", { name: "Sync options" });
    await user.clear(within(form).getByLabelText("Sync every (minutes)"));
    await user.type(within(form).getByLabelText("Sync every (minutes)"), "2000");
    await user.click(within(form).getByRole("button", { name: "Save options" }));
    expect(within(form).getByRole("alert")).toHaveTextContent("from 0 to 1440");
    expect(native.setOptions).not.toHaveBeenCalled();
    await user.clear(within(form).getByLabelText("Sync every (minutes)"));
    await user.type(within(form).getByLabelText("Sync every (minutes)"), "0");
    await user.click(within(form).getByLabelText("Sync revision history"));
    await user.click(within(form).getByRole("button", { name: "Save options" }));
    expect(native.setOptions).toHaveBeenCalledWith(PLUGIN, {
      intervalMinutes: 0, paused: false, syncHistory: false, syncPreferences: true, syncPluginData: true,
    });
  });

  it("changes the passphrase only when the new one is confirmed, then clears every field", async () => {
    const user = userEvent.setup();
    native.status.mockResolvedValue(view());
    native.changePassphrase.mockResolvedValue(view());
    render(<SyncPanel pluginId={PLUGIN} onClose={vi.fn()} />);
    await user.click(await screen.findByText("Change passphrase", { selector: "summary" }));
    const form = screen.getByRole("form", { name: "Change sync passphrase" });
    await user.type(within(form).getByLabelText("Current passphrase"), PASSPHRASE);
    await user.type(within(form).getByLabelText("New passphrase"), "synthetic new passphrase words");
    await user.type(within(form).getByLabelText("Confirm new passphrase"), "synthetic new passphrase words");
    await user.click(within(form).getByRole("button", { name: "Change passphrase" }));
    expect(native.changePassphrase).toHaveBeenCalledWith(PLUGIN, PASSPHRASE, "synthetic new passphrase words");
    for (const input of secretInputs()) expect(input).toHaveValue("");
  });
});
