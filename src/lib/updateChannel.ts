import { useCallback, useEffect, useState } from "react";
import type { RuntimeInfo } from "../types";

export type UpdateChannel = "stable" | "beta";

export const UPDATE_CHANNEL_KEY = "denote-update-channel";

export function getUpdateChannel(
  storage: Pick<Storage, "getItem"> = localStorage,
): UpdateChannel {
  const value = storage.getItem(UPDATE_CHANNEL_KEY);
  if (value === null || value === "stable") return "stable";
  if (value === "beta") return "beta";
  throw new Error("The update channel setting is invalid. Denote uses stable releases.");
}

/** Reports the channel Denote actually checks, for About and bug reports. */
export function withUpdateChannel(
  runtimeInfo: RuntimeInfo | null,
  channel: UpdateChannel,
): RuntimeInfo | null {
  if (!runtimeInfo?.updaterConfigured || runtimeInfo.updateChannel === "development") {
    return runtimeInfo;
  }
  return runtimeInfo.updateChannel === channel
    ? runtimeInfo
    : { ...runtimeInfo, updateChannel: channel };
}

export function useUpdateChannelPreference(reportError: (error: unknown) => void) {
  const [channel, setChannel] = useState<UpdateChannel>("stable");
  useEffect(() => {
    const refresh = () => {
      try {
        setChannel(getUpdateChannel());
      } catch (cause) {
        setChannel("stable");
        reportError(Object.assign(
          new Error("The update channel setting could not be read. Denote uses stable releases."),
          { cause },
        ));
      }
    };
    const changed = (event: StorageEvent) => {
      if (event.key === null || event.key === UPDATE_CHANNEL_KEY) refresh();
    };
    refresh();
    window.addEventListener("storage", changed);
    return () => window.removeEventListener("storage", changed);
  }, [reportError]);
  const update = useCallback((next: UpdateChannel) => {
    try {
      localStorage.setItem(UPDATE_CHANNEL_KEY, next);
      setChannel(next);
    } catch (cause) {
      setChannel("stable");
      reportError(Object.assign(
        new Error("The update channel setting could not be saved. Denote uses stable releases for this session."),
        { cause },
      ));
    }
  }, [reportError]);
  return { channel, update };
}
