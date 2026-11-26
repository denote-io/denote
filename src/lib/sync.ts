// Renderer types for the host-owned whole-app sync commands. They mirror the
// payloads in docs/sync.md exactly; secrets only travel renderer to host.

export type SyncGitAuthMode = "system" | "public" | "ssh-agent" | "github-https";

export type SyncBackendRequest =
  | { kind: "folder"; path: string }
  | { kind: "webdav"; url: string; username: string; password: string }
  | {
    kind: "s3"; endpoint: string; region: string; bucket: string; prefix: string;
    pathStyle: boolean; accessKeyId: string; secretAccessKey: string;
  }
  | { kind: "google-drive"; connectionToken: string; folderName: string }
  | { kind: "git"; remoteUrl: string; branch: string; authMode: SyncGitAuthMode };

export type SyncBackendKind = SyncBackendRequest["kind"];

export interface SyncSetupRequest {
  mode: "create" | "join";
  deviceName: string;
  passphrase: string;
  rememberPassphrase: boolean;
  backend: SyncBackendRequest;
}

export interface SyncOptions {
  /** 0 = manual only, max 1440. */
  intervalMinutes: number;
  paused: boolean;
  syncHistory: boolean;
  syncPreferences: boolean;
  syncPluginData: boolean;
}

export type SyncVaultRequest =
  | { action: "map-new"; syncId: string; path: string }
  | { action: "map-existing"; syncId: string; scopeId: string }
  | { action: "skip"; syncId: string }
  | { action: "enroll"; scopeId: string }
  | { action: "stop"; syncId: string };

export type SyncState = "not-configured" | "needs-passphrase" | "idle" | "running" | "paused" | "error";
export type SyncPhase = "refresh" | "scan" | "merge" | "download" | "apply" | "upload" | "publish";
export type SyncVaultState =
  | "synced" | "not-enrolled" | "pending-mapping" | "skipped" | "paused-conflict"
  | "paused-encryption" | "unavailable";
export type SyncConflictKind = "encryption-manifest" | "plugin-setting" | "plugin-state";

export interface SyncDevice {
  writerId: string;
  name: string;
  lastGeneration: number;
  lastSeenAt: string | null;
  retired: boolean;
  isSelf: boolean;
}

export interface SyncVault {
  syncId: string | null;
  name: string;
  scopeId: string | null;
  path: string | null;
  state: SyncVaultState;
  message: string | null;
}

export interface SyncConflict {
  id: string;
  kind: SyncConflictKind;
  vaultSyncId: string | null;
  pluginId: string | null;
  key: string | null;
  localSummary: string;
  remoteSummary: string;
  remoteDevice: string;
  detectedAt: string;
}

export interface SyncLastRun {
  finishedAt: string;
  ok: boolean;
  message: string;
  uploaded: number;
  downloaded: number;
  applied: number;
  conflicts: number;
  skipped: string[];
}

export interface SyncStatusView {
  state: SyncState;
  backend: null | { kind: SyncBackendKind; label: string };
  rememberPassphrase: boolean;
  options: SyncOptions;
  progress: null | { phase: SyncPhase; done: number; total: number };
  lastRun: null | SyncLastRun;
  device: null | { writerId: string; name: string };
  devices: SyncDevice[];
  vaults: SyncVault[];
  conflicts: SyncConflict[];
  pluginSuggestions: { pluginId: string; fromDevice: string }[];
  warnings: string[];
}

export interface SyncGoogleConnection {
  connectionToken: string;
  accountLabel: string;
}

export interface SyncAppliedEvent {
  vaultRoots: string[];
  preferences: Record<string, string | null>;
  pluginIds: string[];
}

export const SYNC_STATUS_EVENT = "denote://sync-status";
export const SYNC_APPLIED_EVENT = "denote://sync-applied";

export const SYNC_MIN_PASSPHRASE_LENGTH = 12;
export const SYNC_MAX_INTERVAL_MINUTES = 1440;

export type PassphraseStrength = "too-short" | "weak" | "fair" | "strong";

/** A coarse hint only; the host enforces its own minimum. */
export function passphraseStrength(passphrase: string): PassphraseStrength {
  const length = [...passphrase].length;
  if (length < SYNC_MIN_PASSPHRASE_LENGTH) return "too-short";
  const classes = [/[a-z]/, /[A-Z]/, /[0-9]/, /[^A-Za-z0-9]/].filter((pattern) => pattern.test(passphrase)).length;
  const words = passphrase.trim().split(/\s+/).length;
  if (length >= 20 && (classes >= 3 || words >= 4)) return "strong";
  if (length >= 16 || classes >= 3) return "fair";
  return "weak";
}

export function isSyncStatusView(value: unknown): value is SyncStatusView {
  if (!value || typeof value !== "object") return false;
  const view = value as Partial<SyncStatusView>;
  return typeof view.state === "string" && typeof view.options === "object" && view.options !== null &&
    Array.isArray(view.devices) && Array.isArray(view.vaults) && Array.isArray(view.conflicts) &&
    Array.isArray(view.pluginSuggestions) && Array.isArray(view.warnings);
}

export function isSyncAppliedEvent(value: unknown): value is SyncAppliedEvent {
  if (!value || typeof value !== "object") return false;
  const event = value as Partial<SyncAppliedEvent>;
  return Array.isArray(event.vaultRoots) && event.vaultRoots.every((root) => typeof root === "string") &&
    Array.isArray(event.pluginIds) && event.pluginIds.every((id) => typeof id === "string") &&
    typeof event.preferences === "object" && event.preferences !== null && !Array.isArray(event.preferences);
}
