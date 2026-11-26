import { useEffect, useRef } from "react";
import { api, errorMessage } from "./api";
import { isSyncStatusView, type SyncAppliedEvent, type SyncStatusView } from "./sync";
import { SyncPreferencePusher, applySyncPreferences, type SyncPreferenceKey } from "./syncPreferences";

export interface SyncBridgeOptions {
  /** The plugin that registered the host Sync tool, or null when none is registered. */
  pluginId: string | null;
  /** Runs after received preferences were written to local storage. */
  onApplied: (event: SyncAppliedEvent, changedPreferences: SyncPreferenceKey[]) => void;
  onError: (error: unknown) => void;
  debounceMs?: number;
}

function acceptsPreferences(status: SyncStatusView): boolean {
  return status.state !== "not-configured" && status.options.syncPreferences;
}

/**
 * Connects renderer preferences to host sync. Allowlisted local changes are
 * pushed, debounced, while sync is configured with preferences enabled; the
 * full map is pushed once whenever that becomes true so a newly configured
 * device publishes its current values. Every `denote://sync-applied` event is
 * written to local storage without being echoed back.
 */
export function useSyncBridge({ pluginId, onApplied, onError, debounceMs }: SyncBridgeOptions): void {
  const callbacks = useRef({ onApplied, onError });
  callbacks.current = { onApplied, onError };

  useEffect(() => {
    let active = true;
    const stop = api.onSyncApplied((event) => {
      if (!active) return;
      const changed = applySyncPreferences(event.preferences);
      callbacks.current.onApplied(event, changed);
    });
    return () => {
      active = false;
      void stop.then((unlisten) => unlisten()).catch(() => console.error("Sync apply listener cleanup failed."));
    };
  }, []);

  useEffect(() => {
    if (!pluginId) return;
    let active = true;
    let ready = false;
    const pusher = new SyncPreferencePusher(
      async (preferences) => { if (ready) await api.syncPutPreferences(pluginId, preferences); },
      (error) => { if (active) callbacks.current.onError(error); },
      debounceMs,
    );
    const observe = (status: SyncStatusView) => {
      if (!active) return;
      const next = acceptsPreferences(status);
      const became = next && !ready;
      ready = next;
      if (became) void pusher.flush(true);
    };
    void api.syncStatus(pluginId).then((status) => { if (isSyncStatusView(status)) observe(status); })
      .catch((error) => console.error(`Sync status is unavailable: ${errorMessage(error)}`));
    const stop = api.onSyncStatus(observe);
    return () => {
      active = false;
      pusher.dispose();
      void stop.then((unlisten) => unlisten()).catch(() => console.error("Sync status listener cleanup failed."));
    };
  }, [debounceMs, pluginId]);
}
