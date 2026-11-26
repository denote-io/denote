// The portable renderer preference allowlist from docs/sync.md. Device layout,
// trust, network consent, and automation preferences are never listed here.
export const SYNC_PREFERENCE_KEYS = [
  "denote-theme",
  "denote-editor-display",
  "denote-markdown-view-mode",
  "denote-show-dotfiles",
  "denote.speech-shortcut.v1",
  "denote.plugin-rail.v1",
] as const;

export type SyncPreferenceKey = (typeof SYNC_PREFERENCE_KEYS)[number];
export type SyncPreferences = Record<SyncPreferenceKey, string | null>;

/** Dispatched on `window` after a local write to an allowlisted key. */
export const SYNC_PREFERENCE_CHANGED_EVENT = "denote:sync-preference-changed";
/** Dispatched on `window` after received preferences were written locally. */
export const SYNC_PREFERENCES_APPLIED_EVENT = "denote:sync-preferences-applied";

const MAX_PREFERENCE_LENGTH = 64 * 1024;

export function isSyncPreferenceKey(key: string): key is SyncPreferenceKey {
  return (SYNC_PREFERENCE_KEYS as readonly string[]).includes(key);
}

/** Called by each preference writer after it stores an allowlisted key. */
export function notifySyncPreferenceChanged(key: SyncPreferenceKey): void {
  if (typeof window === "undefined") return;
  window.dispatchEvent(new CustomEvent(SYNC_PREFERENCE_CHANGED_EVENT, { detail: { key } }));
}

export function readSyncPreferences(storage: Pick<Storage, "getItem"> = localStorage): SyncPreferences {
  const preferences = {} as SyncPreferences;
  for (const key of SYNC_PREFERENCE_KEYS) {
    try { preferences[key] = storage.getItem(key); }
    catch { preferences[key] = null; }
  }
  return preferences;
}

/**
 * Writes received preferences without announcing them as local changes, so
 * they are not pushed back. Keys outside the allowlist and non-string values
 * are ignored. Returns the keys whose stored value changed.
 */
export function applySyncPreferences(
  received: Record<string, unknown>,
  storage: Pick<Storage, "getItem" | "setItem" | "removeItem"> = localStorage,
): SyncPreferenceKey[] {
  const changed: SyncPreferenceKey[] = [];
  for (const key of SYNC_PREFERENCE_KEYS) {
    if (!Object.prototype.hasOwnProperty.call(received, key)) continue;
    const value = received[key];
    if (value !== null && (typeof value !== "string" || value.length > MAX_PREFERENCE_LENGTH)) continue;
    try {
      if (storage.getItem(key) === value) continue;
      if (value === null) storage.removeItem(key);
      else storage.setItem(key, value);
      changed.push(key);
    } catch (error) {
      console.error(`Unable to apply the synced ${key} preference:`, error);
    }
  }
  if (changed.length && typeof window !== "undefined") {
    window.dispatchEvent(new CustomEvent(SYNC_PREFERENCES_APPLIED_EVENT, { detail: { keys: changed } }));
  }
  return changed;
}

export function observeAppliedSyncPreferences(onApplied: (keys: SyncPreferenceKey[]) => void): () => void {
  const listener = (event: Event) => {
    const keys = (event as CustomEvent<{ keys?: unknown }>).detail?.keys;
    if (Array.isArray(keys)) onApplied(keys.filter((key): key is SyncPreferenceKey => typeof key === "string" && isSyncPreferenceKey(key)));
  };
  window.addEventListener(SYNC_PREFERENCES_APPLIED_EVENT, listener);
  return () => window.removeEventListener(SYNC_PREFERENCES_APPLIED_EVENT, listener);
}

/**
 * Debounces local preference changes into one `sync_put_preferences` call.
 * The baseline starts at the current values and follows received
 * preferences, so neither startup nor an applied remote change is echoed.
 */
export class SyncPreferencePusher {
  private baseline: string;
  private timer: number | null = null;
  private readonly removeListeners: () => void;

  constructor(
    private readonly push: (preferences: SyncPreferences) => Promise<void>,
    private readonly onError: (error: unknown) => void,
    private readonly delayMs = 1500,
    private readonly storage: Pick<Storage, "getItem"> = localStorage,
  ) {
    this.baseline = JSON.stringify(readSyncPreferences(storage));
    const changed = () => this.schedule();
    const applied = () => { this.baseline = JSON.stringify(readSyncPreferences(this.storage)); };
    window.addEventListener(SYNC_PREFERENCE_CHANGED_EVENT, changed);
    window.addEventListener(SYNC_PREFERENCES_APPLIED_EVENT, applied);
    this.removeListeners = () => {
      window.removeEventListener(SYNC_PREFERENCE_CHANGED_EVENT, changed);
      window.removeEventListener(SYNC_PREFERENCES_APPLIED_EVENT, applied);
    };
  }

  schedule(): void {
    if (this.timer !== null) window.clearTimeout(this.timer);
    this.timer = window.setTimeout(() => { this.timer = null; void this.flush(); }, this.delayMs);
  }

  async flush(force = false): Promise<void> {
    const preferences = readSyncPreferences(this.storage);
    const serialized = JSON.stringify(preferences);
    if (!force && serialized === this.baseline) return;
    const previous = this.baseline;
    this.baseline = serialized;
    try { await this.push(preferences); }
    catch (error) {
      if (this.baseline === serialized) this.baseline = previous;
      this.onError(error);
    }
  }

  dispose(): void {
    if (this.timer !== null) window.clearTimeout(this.timer);
    this.timer = null;
    this.removeListeners();
  }
}
