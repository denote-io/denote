import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { SyncAppliedEvent, SyncStatusView } from "./sync";
import {
  SYNC_PREFERENCE_KEYS, SyncPreferencePusher, applySyncPreferences, notifySyncPreferenceChanged, readSyncPreferences,
} from "./syncPreferences";
import { saveThemePreference } from "./theme";
import { useSyncBridge } from "./useSyncBridge";

const native = vi.hoisted(() => ({
  status: vi.fn(),
  put: vi.fn(),
  statusHandler: null as ((status: SyncStatusView) => void) | null,
  appliedHandler: null as ((event: SyncAppliedEvent) => void) | null,
}));
vi.mock("./api", async (original) => ({
  ...await original<typeof import("./api")>(),
  api: {
    syncStatus: native.status,
    syncPutPreferences: native.put,
    onSyncStatus: vi.fn(async (handler: (status: SyncStatusView) => void) => { native.statusHandler = handler; return () => {}; }),
    onSyncApplied: vi.fn(async (handler: (event: SyncAppliedEvent) => void) => { native.appliedHandler = handler; return () => {}; }),
  },
}));

function status(state: SyncStatusView["state"], syncPreferences = true): SyncStatusView {
  return {
    state, backend: null, rememberPassphrase: true, progress: null, lastRun: null, device: null,
    options: { intervalMinutes: 15, paused: false, syncHistory: true, syncPreferences, syncPluginData: true },
    devices: [], vaults: [], conflicts: [], pluginSuggestions: [], warnings: [],
  };
}

const allNull = Object.fromEntries(SYNC_PREFERENCE_KEYS.map((key) => [key, null]));

beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
  native.statusHandler = null;
  native.appliedHandler = null;
  native.put.mockResolvedValue(undefined);
});

afterEach(() => vi.useRealTimers());

describe("sync preference allowlist", () => {
  it("applies only allowlisted string or null values without announcing them as local changes", () => {
    localStorage.setItem("denote-show-dotfiles", "false");
    localStorage.setItem("denote-sidebar-width", "320");
    const local = vi.fn();
    const applied = vi.fn();
    window.addEventListener("denote:sync-preference-changed", local);
    window.addEventListener("denote:sync-preferences-applied", applied);

    const changed = applySyncPreferences({
      "denote-theme": "light",
      "denote-show-dotfiles": null,
      "denote-editor-display": 42,
      "denote-sidebar-width": "999",
      "denote-markdown-view-mode": "x".repeat(64 * 1024 + 1),
    });

    window.removeEventListener("denote:sync-preference-changed", local);
    window.removeEventListener("denote:sync-preferences-applied", applied);
    expect(changed).toEqual(["denote-theme", "denote-show-dotfiles"]);
    expect(localStorage.getItem("denote-theme")).toBe("light");
    expect(localStorage.getItem("denote-show-dotfiles")).toBeNull();
    expect(localStorage.getItem("denote-sidebar-width")).toBe("320");
    expect(localStorage.getItem("denote-editor-display")).toBeNull();
    expect(local).not.toHaveBeenCalled();
    expect(applied).toHaveBeenCalledTimes(1);
  });

  it("debounces local changes into one push of the allowlist and never echoes applied values", async () => {
    vi.useFakeTimers();
    const push = vi.fn(async () => {});
    const pusher = new SyncPreferencePusher(push, vi.fn(), 500);
    saveThemePreference("light");
    localStorage.setItem("denote-show-dotfiles", "false");
    notifySyncPreferenceChanged("denote-show-dotfiles");
    localStorage.setItem("denote-sidebar-width", "320");
    await vi.advanceTimersByTimeAsync(499);
    expect(push).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(push).toHaveBeenCalledTimes(1);
    expect(push).toHaveBeenCalledWith({ ...allNull, "denote-theme": "light", "denote-show-dotfiles": "false" });

    applySyncPreferences({ "denote-theme": "dark" });
    notifySyncPreferenceChanged("denote-theme");
    await vi.advanceTimersByTimeAsync(500);
    expect(push).toHaveBeenCalledTimes(1);
    pusher.dispose();
  });
});

describe("useSyncBridge", () => {
  it("publishes the full allowlist once sync accepts preferences, then debounced local changes", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    localStorage.setItem("denote-theme", "light");
    native.status.mockResolvedValue(status("not-configured"));
    renderHook(() => useSyncBridge({ pluginId: "denote.sync", onApplied: vi.fn(), onError: vi.fn(), debounceMs: 200 }));
    await waitFor(() => expect(native.status).toHaveBeenCalledWith("denote.sync"));
    await act(async () => { saveThemePreference("dark"); await vi.advanceTimersByTimeAsync(250); });
    expect(native.put).not.toHaveBeenCalled();

    act(() => native.statusHandler?.(status("idle")));
    await waitFor(() => expect(native.put).toHaveBeenCalledTimes(1));
    expect(native.put).toHaveBeenLastCalledWith("denote.sync", { ...allNull, "denote-theme": "dark" });

    await act(async () => { saveThemePreference("system"); await vi.advanceTimersByTimeAsync(250); });
    expect(native.put).toHaveBeenCalledTimes(2);
    expect(native.put).toHaveBeenLastCalledWith("denote.sync", { ...allNull, "denote-theme": "system" });

    act(() => native.statusHandler?.(status("idle", false)));
    await act(async () => { saveThemePreference("light"); await vi.advanceTimersByTimeAsync(250); });
    expect(native.put).toHaveBeenCalledTimes(2);
  });

  it("applies received preferences and reports affected vaults and plugins without pushing them back", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    native.status.mockResolvedValue(status("idle"));
    const onApplied = vi.fn();
    renderHook(() => useSyncBridge({ pluginId: "denote.sync", onApplied, onError: vi.fn(), debounceMs: 200 }));
    await waitFor(() => expect(native.put).toHaveBeenCalledTimes(1));
    const event: SyncAppliedEvent = {
      vaultRoots: ["/synthetic/alpha"],
      preferences: { "denote-theme": "light", "denote.plugin-rail.v1": "{\"order\":[]}", "denote-update-channel": "beta" },
      pluginIds: ["denote.synthetic"],
    };

    act(() => native.appliedHandler?.(event));
    await act(async () => { await vi.advanceTimersByTimeAsync(250); });

    expect(onApplied).toHaveBeenCalledWith(event, ["denote-theme", "denote.plugin-rail.v1"]);
    expect(localStorage.getItem("denote-theme")).toBe("light");
    expect(localStorage.getItem("denote-update-channel")).toBeNull();
    expect(native.put).toHaveBeenCalledTimes(1);
  });

  it("does nothing until a sync tool is registered", async () => {
    renderHook(() => useSyncBridge({ pluginId: null, onApplied: vi.fn(), onError: vi.fn() }));
    await act(async () => { await Promise.resolve(); });
    expect(native.status).not.toHaveBeenCalled();
    expect(readSyncPreferences()["denote-theme"]).toBeNull();
  });
});
