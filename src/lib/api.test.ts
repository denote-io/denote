import { beforeEach, describe, expect, test, vi } from "vitest";

const { invoke, listen } = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen }));

import { api, errorMessage } from "./api";

describe("project configuration API", () => {
  beforeEach(() => {
    invoke.mockReset();
  });

  describe("PDF API", () => {
    beforeEach(() => {
      invoke.mockReset();
    });

    test("reads a PDF through its byte-preserving native command", async () => {
      invoke.mockResolvedValue({
        path: "Synthetic.pdf",
        dataBase64: "JVBERi0xLjcK",
        contentHash: "synthetic-hash",
        stats: {},
      });

      await api.readPdf("Synthetic.pdf");

      expect(invoke).toHaveBeenCalledWith("read_pdf", {
        path: "Synthetic.pdf",
      });
    });
  });

  test("includes the originating vault path in every mutation", async () => {
    invoke.mockResolvedValue({});
    const vaultPath = "/synthetic/vault";

    await api.markProjectRoot(vaultPath, "app");
    await api.unmarkProjectRoot(vaultPath, "project-id");
    await api.markProjectWorkspace(vaultPath, "packages");
    await api.unmarkProjectWorkspace(vaultPath, "workspace-id");
    await api.dismissGitProjectSuggestion(vaultPath);
    await api.refreshProjectConfiguration(vaultPath);
    await api.refreshGitignoreStatus(vaultPath, ["app", "packages/ui"]);

    expect(invoke.mock.calls).toEqual([
      [
        "mark_project_root",
        { expectedVaultPath: vaultPath, path: "app" },
      ],
      [
        "unmark_project_root",
        { expectedVaultPath: vaultPath, projectRootId: "project-id" },
      ],
      [
        "mark_project_workspace",
        { expectedVaultPath: vaultPath, path: "packages" },
      ],
      [
        "unmark_project_workspace",
        {
          expectedVaultPath: vaultPath,
          projectWorkspaceId: "workspace-id",
        },
      ],
      ["dismiss_git_project_suggestion", { expectedVaultPath: vaultPath }],
      ["refresh_project_configuration", { expectedVaultPath: vaultPath }],
      [
        "refresh_gitignore_status",
        {
          expectedVaultPath: vaultPath,
          scopePaths: ["app", "packages/ui"],
        },
      ],
    ]);
  });

  test("reads clipboard text through the native clipboard command", async () => {
    invoke.mockResolvedValue("Synthetic clipboard text");

    await expect(api.readClipboardText()).resolves.toBe(
      "Synthetic clipboard text",
    );
    expect(invoke).toHaveBeenCalledWith("read_clipboard_text");
  });

  test("normalizes Windows verbatim paths in native errors", () => {
    expect(
      errorMessage(
        new Error(String.raw`Unable to open \\?\C:\workspace\note.md`),
      ),
    ).toBe(String.raw`Unable to open C:\workspace\note.md`);
  });
});

describe("sync API", () => {
  const pluginId = "denote.sync";
  const options = { intervalMinutes: 0, paused: false, syncHistory: true, syncPreferences: true, syncPluginData: false };
  const status = {
    state: "idle", backend: null, rememberPassphrase: true, options, progress: null, lastRun: null, device: null,
    devices: [], vaults: [], conflicts: [], pluginSuggestions: [], warnings: [],
  };

  beforeEach(() => {
    invoke.mockReset().mockResolvedValue(undefined);
    listen.mockReset();
  });

  test("invokes every sync command with the documented payload", async () => {
    const request = {
      mode: "join" as const, deviceName: "Synthetic laptop", passphrase: "synthetic passphrase", rememberPassphrase: false,
      backend: { kind: "folder" as const, path: "/synthetic/sync" },
    };
    await api.syncStatus(pluginId);
    await api.syncConfigure(pluginId, request);
    await api.syncConnectGoogle(pluginId, null);
    await api.syncUnlock(pluginId, "synthetic passphrase", true);
    await api.syncRun(pluginId);
    await api.syncCancel(pluginId);
    await api.syncSetOptions(pluginId, options);
    await api.syncMapVault(pluginId, { action: "skip", syncId: "sync-alpha" });
    await api.syncResolve(pluginId, "conflict-1", "remote");
    await api.syncPutPreferences(pluginId, { "denote-theme": "light", "denote-show-dotfiles": null });
    await api.syncRetireDevice(pluginId, "b".repeat(32));
    await api.syncChangePassphrase(pluginId, "synthetic old", "synthetic new");
    await api.syncDisconnect(pluginId);
    await api.syncChooseFolder(pluginId, "vault");

    expect(invoke.mock.calls).toEqual([
      ["sync_status", { pluginId }],
      ["sync_configure", { pluginId, request }],
      ["sync_connect_google", { pluginId, clientId: null }],
      ["sync_unlock", { pluginId, passphrase: "synthetic passphrase", remember: true }],
      ["sync_run", { pluginId }],
      ["sync_cancel", { pluginId }],
      ["sync_set_options", { pluginId, options }],
      ["sync_map_vault", { pluginId, request: { action: "skip", syncId: "sync-alpha" } }],
      ["sync_resolve", { pluginId, conflictId: "conflict-1", choice: "remote" }],
      ["sync_put_preferences", { pluginId, preferences: { "denote-theme": "light", "denote-show-dotfiles": null } }],
      ["sync_retire_device", { pluginId, writerId: "b".repeat(32) }],
      ["sync_change_passphrase", { pluginId, current: "synthetic old", next: "synthetic new" }],
      ["sync_disconnect", { pluginId }],
      ["sync_choose_folder", { pluginId, purpose: "vault" }],
    ]);
  });

  test("delivers only well-formed sync events", async () => {
    const handlers = new Map<string, (event: { payload: unknown }) => void>();
    listen.mockImplementation(async (name: string, handler: (event: { payload: unknown }) => void) => {
      handlers.set(name, handler);
      return () => {};
    });
    const onStatus = vi.fn();
    const onApplied = vi.fn();
    await api.onSyncStatus(onStatus);
    await api.onSyncApplied(onApplied);

    handlers.get("denote://sync-status")?.({ payload: { ...status, state: "exfiltrating" } });
    handlers.get("denote://sync-status")?.({ payload: status });
    handlers.get("denote://sync-applied")?.({ payload: { vaultRoots: [1], preferences: {}, pluginIds: [] } });
    const applied = { vaultRoots: ["/synthetic/alpha"], preferences: { "denote-theme": "dark" }, pluginIds: ["denote.synthetic"] };
    handlers.get("denote://sync-applied")?.({ payload: applied });

    expect(onStatus.mock.calls).toEqual([[status]]);
    expect(onApplied.mock.calls).toEqual([[applied]]);
  });
});
