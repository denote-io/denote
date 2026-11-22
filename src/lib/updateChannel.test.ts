import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import type { RuntimeInfo } from "../types";
import {
  getUpdateChannel, UPDATE_CHANNEL_KEY, useUpdateChannelPreference, withUpdateChannel,
} from "./updateChannel";

beforeEach(() => {
  vi.restoreAllMocks();
  localStorage.clear();
});

it("defaults to stable and rejects an unknown channel", () => {
  expect(getUpdateChannel()).toBe("stable");
  localStorage.setItem(UPDATE_CHANNEL_KEY, "beta");
  expect(getUpdateChannel()).toBe("beta");
  localStorage.setItem(UPDATE_CHANNEL_KEY, "nightly");
  expect(() => getUpdateChannel()).toThrow(/invalid.*stable/);
});

it("falls back to stable when the saved channel is invalid or cannot be saved", async () => {
  localStorage.setItem(UPDATE_CHANNEL_KEY, "nightly");
  const reportError = vi.fn();
  const { result } = renderHook(() => useUpdateChannelPreference(reportError));
  await waitFor(() => expect(reportError).toHaveBeenCalledOnce());
  expect(result.current.channel).toBe("stable");

  act(() => result.current.update("beta"));
  expect(result.current.channel).toBe("beta");
  expect(localStorage.getItem(UPDATE_CHANNEL_KEY)).toBe("beta");

  vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new DOMException("Synthetic unavailable storage", "SecurityError");
  });
  act(() => result.current.update("beta"));
  expect(result.current.channel).toBe("stable");
  expect(reportError).toHaveBeenLastCalledWith(expect.objectContaining({
    message: expect.stringContaining("stable releases for this session"),
  }));
});

it("reports the selected channel only for configured release builds", () => {
  const release: RuntimeInfo = {
    operatingSystem: "macos", architecture: "aarch64", bundleType: "app",
    updateChannel: "stable", updaterConfigured: true,
  };
  expect(withUpdateChannel(release, "beta")?.updateChannel).toBe("beta");
  expect(withUpdateChannel(release, "stable")).toBe(release);
  const development = { ...release, updateChannel: "development", updaterConfigured: false };
  expect(withUpdateChannel(development, "beta")).toBe(development);
  expect(withUpdateChannel(null, "beta")).toBeNull();
});
