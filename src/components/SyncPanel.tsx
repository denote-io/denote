import { useEffect, useId, useRef, useState, type FormEvent, type ReactNode } from "react";
import { api, errorMessage } from "../lib/api";
import {
  SYNC_MAX_INTERVAL_MINUTES, SYNC_MIN_PASSPHRASE_LENGTH, isSyncStatusView, passphraseStrength,
  type PassphraseStrength, type SyncBackendKind, type SyncBackendRequest, type SyncConflict, type SyncDevice,
  type SyncGitAuthMode, type SyncOptions, type SyncPhase, type SyncSetupRequest, type SyncStatusView, type SyncVault,
  type SyncVaultRequest, type SyncVaultState,
} from "../lib/sync";
import { SensitiveToolPanel } from "./SensitiveToolPanel";
import "./SensitiveTools.css";
import "./SyncPanel.css";

type Perform = (operation: () => Promise<string | void>) => Promise<void>;
type Accept = (value: unknown) => SyncStatusView;

export interface SyncPanelProps {
  pluginId: string;
  onClose: () => void;
  /** Opens plugin settings so the user can review a suggested plugin. Never enables anything. */
  onOpenPluginSettings?: (pluginId: string) => void;
}

const BACKEND_LABELS: Record<SyncBackendKind, string> = {
  folder: "Local or mounted folder",
  webdav: "WebDAV",
  s3: "S3-compatible storage",
  "google-drive": "Google Drive",
  git: "Dedicated Git repository",
};

const GIT_AUTH_LABELS: Record<SyncGitAuthMode, string> = {
  system: "System Git credentials",
  public: "Public repository (no credentials)",
  "ssh-agent": "SSH agent",
  "github-https": "GitHub over HTTPS (stored token)",
};

const STATE_LABELS: Record<SyncStatusView["state"], string> = {
  "not-configured": "Not set up",
  "needs-passphrase": "Locked until you enter the sync passphrase",
  idle: "Ready",
  running: "Syncing…",
  paused: "Paused",
  error: "Needs attention",
};

const PHASE_LABELS: Record<SyncPhase, string> = {
  refresh: "Checking storage",
  scan: "Scanning vaults",
  merge: "Merging changes",
  download: "Downloading",
  apply: "Applying changes",
  upload: "Uploading",
  publish: "Publishing",
};

const VAULT_STATE_LABELS: Record<SyncVaultState, string> = {
  synced: "Synced",
  "not-enrolled": "Not synced",
  "pending-mapping": "Needs a folder on this device",
  skipped: "Skipped on this device",
  "paused-conflict": "Paused: encryption settings conflict",
  "paused-encryption": "Paused while encryption changes",
  "paused-deletions": "Paused: most files are missing",
  unavailable: "Folder unavailable",
};

const STRENGTH_LABELS: Record<PassphraseStrength, string> = {
  "too-short": `Too short: use at least ${SYNC_MIN_PASSPHRASE_LENGTH} characters.`,
  weak: "Weak: add more words or characters.",
  fair: "Fair: a longer passphrase is harder to guess.",
  strong: "Strong.",
};

const LOST_PASSPHRASE_WARNING =
  "Denote cannot recover or reset the sync passphrase. If every device forgets it, the data in your sync storage is unrecoverable.";

function formatTime(value: string | null): string {
  if (!value) return "Never";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

function clearSecrets(root: HTMLElement | null) {
  root?.querySelectorAll<HTMLInputElement>("input[data-secret]").forEach((input) => { input.value = ""; });
}

function httpUrl(value: string, label: string): string {
  let url: URL;
  try { url = new URL(value); }
  catch { throw new Error(`Enter a full ${label}, starting with https://.`); }
  if (url.protocol !== "https:" && url.protocol !== "http:") throw new Error(`The ${label} must start with https:// or http://.`);
  return value;
}

/** Validates the passphrase fields; the host enforces its own rules as well. */
function checkPassphrase(passphrase: string, confirmation: string | null) {
  if (confirmation === null) {
    if (!passphrase) throw new Error("Enter the sync passphrase.");
    return;
  }
  if ([...passphrase].length < SYNC_MIN_PASSPHRASE_LENGTH) {
    throw new Error(`Use a sync passphrase of at least ${SYNC_MIN_PASSPHRASE_LENGTH} characters.`);
  }
  if (passphrase !== confirmation) throw new Error("The passphrases do not match.");
}

export function SyncPanel({ pluginId, onClose, onOpenPluginSettings }: SyncPanelProps) {
  const [status, setStatus] = useState<SyncStatusView | null>(null);
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const [runRequested, setRunRequested] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const alive = useRef(true);

  const accept: Accept = (value) => {
    if (!isSyncStatusView(value)) throw new Error("The sync host returned an invalid status. Close Sync and open it again.");
    if (alive.current) setStatus(value);
    return value;
  };
  const perform: Perform = async (operation) => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true); setError(""); setNotice("");
    try {
      const message = await operation();
      if (alive.current && message) setNotice(message);
    } catch (caught) {
      if (alive.current) setError(errorMessage(caught));
    } finally {
      busyRef.current = false;
      if (alive.current) setBusy(false);
    }
  };

  useEffect(() => {
    alive.current = true;
    void api.syncStatus(pluginId).then((value) => {
      if (!isSyncStatusView(value)) throw new Error("The sync status is unavailable. Close Sync and open it again.");
      if (alive.current) setStatus((current) => current ?? value);
    }).catch((caught) => { if (alive.current) setError(errorMessage(caught)); });
    const stop = api.onSyncStatus((next) => { if (alive.current) setStatus(next); });
    return () => {
      alive.current = false;
      void stop.then((unlisten) => unlisten()).catch(() => console.error("Sync status listener cleanup failed."));
    };
  }, [pluginId]);

  const syncNow = () => {
    if (runRequested) return;
    setError(""); setNotice(""); setRunRequested(true);
    void api.syncRun(pluginId).then((value) => {
      const view = accept(value);
      if (alive.current && view.lastRun?.ok !== false) setNotice("Sync finished.");
    }).catch((caught) => { if (alive.current) setError(errorMessage(caught)); })
      .finally(() => { if (alive.current) setRunRequested(false); });
  };

  let body: ReactNode;
  if (!status) {
    body = <p role="status">{error ? "" : "Loading sync status…"}</p>;
  } else if (status.state === "not-configured") {
    body = <SyncSetup pluginId={pluginId} busy={busy} perform={perform} accept={accept} />;
  } else if (status.state === "needs-passphrase") {
    body = <>
      <SyncUnlock pluginId={pluginId} busy={busy} remember={status.rememberPassphrase} perform={perform} accept={accept} />
      <SyncDisconnect pluginId={pluginId} busy={busy} perform={perform} accept={accept} />
    </>;
  } else {
    const running = status.state === "running" || runRequested;
    body = <>
      <SyncOverview status={status} running={running} busy={busy}
        onSyncNow={syncNow}
        onRejoin={() => void perform(async () => {
          accept(await api.syncRejoin(pluginId));
          return "This device rejoined sync. The next sync compares its files with your other devices.";
        })}
        onCancel={() => void perform(async () => { await api.syncCancel(pluginId); return "Sync stops after the current file."; })}
        onPause={() => void perform(async () => {
          const paused = !status.options.paused;
          accept(await api.syncSetOptions(pluginId, { ...status.options, paused }));
          return paused ? "Automatic sync paused." : "Automatic sync resumed.";
        })} />
      {status.warnings.length ? <SyncSection title="Warnings">
        <ul className="sync-panel__list">{status.warnings.map((warning, index) => <li key={`${index}:${warning}`}>{warning}</li>)}</ul>
      </SyncSection> : null}
      <SyncConflicts conflicts={status.conflicts} vaults={status.vaults} disabled={busy || running}
        onResolve={(conflict, choice) => void perform(async () => {
          accept(await api.syncResolve(pluginId, conflict.id, choice));
          return choice === "local" ? "Kept the local version." : "Using the remote version.";
        })} />
      <SyncVaults vaults={status.vaults} disabled={busy || running}
        onRequest={(request, message) => void perform(async () => {
          accept(await api.syncMapVault(pluginId, request));
          return message;
        })}
        onChooseNewFolder={(vault) => void perform(async () => {
          if (!vault.syncId) return;
          const path = await api.syncChooseFolder(pluginId, "vault");
          if (!path) return "No folder chosen. The vault is still waiting for a folder.";
          accept(await api.syncMapVault(pluginId, { action: "map-new", syncId: vault.syncId, path }));
          return `${vault.name} will download into the chosen folder.`;
        })} />
      <SyncDevices devices={status.devices} disabled={busy || running}
        onRemove={(device) => void perform(async () => {
          accept(await api.syncRetireDevice(pluginId, device.writerId));
          return `Removed ${device.name}.`;
        })} />
      {status.pluginSuggestions.length ? <SyncSection title="Plugins on other devices">
        <p className="authoring-muted">Sync never installs, approves, or enables plugins. Review each plugin yourself in plugin settings.</p>
        <ul className="sync-panel__list">{status.pluginSuggestions.map((suggestion) =>
          <li key={`${suggestion.pluginId}:${suggestion.fromDevice}`} className="sync-panel__row">
            <span><strong>{suggestion.pluginId}</strong> is enabled on {suggestion.fromDevice}.</span>
            {onOpenPluginSettings ? <button type="button" className="secondary-button"
              aria-label={`Review ${suggestion.pluginId} in plugin settings`}
              onClick={() => onOpenPluginSettings(suggestion.pluginId)}>Review in plugin settings</button> : null}
          </li>)}</ul>
      </SyncSection> : null}
      <SyncOptionsForm key={JSON.stringify(status.options)} options={status.options} disabled={busy}
        onSave={(options) => void perform(async () => {
          accept(await api.syncSetOptions(pluginId, options));
          return "Sync options saved.";
        })} />
      <SyncChangePassphrase pluginId={pluginId} busy={busy || running} perform={perform} accept={accept} />
      <SyncDisconnect pluginId={pluginId} busy={busy || running} perform={perform} accept={accept} />
    </>;
  }

  return <SensitiveToolPanel title="Sync" feature="sync" onClose={onClose}>
    <div className="sensitive-tool sync-panel">
      <p className="authoring-muted">
        Sync replicates every synced vault, portable preferences, revision history, and plugin data through storage you
        choose. Everything is end-to-end encrypted with your sync passphrase before it leaves this device.
      </p>
      {error ? <p role="alert" className="authoring-error">{error}</p> : null}
      <p role="status">{busy ? "Working…" : notice}</p>
      {body}
    </div>
  </SensitiveToolPanel>;
}

function SyncSection({ title, children }: { title: string; children: ReactNode }) {
  const id = useId();
  return <section aria-labelledby={id}>
    <h3 id={id}>{title}</h3>
    {children}
  </section>;
}

function PassphraseHint({ strength, id }: { strength: PassphraseStrength | null; id: string }) {
  return <p id={id} className="sync-panel__strength" data-strength={strength ?? undefined} aria-live="polite">
    {strength ? STRENGTH_LABELS[strength] : `Use at least ${SYNC_MIN_PASSPHRASE_LENGTH} characters. Several unrelated words work well.`}
  </p>;
}

function SyncSetup({ pluginId, busy, perform, accept }: { pluginId: string; busy: boolean; perform: Perform; accept: Accept }) {
  const [mode, setMode] = useState<"create" | "join">("create");
  const [backend, setBackend] = useState<SyncBackendKind>("folder");
  const [folderPath, setFolderPath] = useState("");
  const [googleAccount, setGoogleAccount] = useState<string | null>(null);
  const googleToken = useRef<string | null>(null);
  const [strength, setStrength] = useState<PassphraseStrength | null>(null);
  const [validation, setValidation] = useState("");
  const form = useRef<HTMLFormElement>(null);
  const hintId = useId();
  const warningId = useId();

  useEffect(() => {
    const root = form.current;
    return () => { clearSecrets(root); googleToken.current = null; };
  }, []);

  const backendRequest = (values: FormData): SyncBackendRequest => {
    const text = (name: string) => String(values.get(name) ?? "").trim();
    const secret = (name: string) => String(values.get(name) ?? "");
    switch (backend) {
      case "folder": {
        const path = folderPath.trim();
        if (!path) throw new Error("Choose or enter the folder that stores sync data.");
        return { kind: "folder", path };
      }
      case "webdav":
        return { kind: "webdav", url: httpUrl(text("webdavUrl"), "WebDAV URL"), username: text("webdavUsername"), password: secret("webdavPassword") };
      case "s3": {
        const bucket = text("s3Bucket");
        if (!bucket || /[\s/]/.test(bucket)) throw new Error("Enter a bucket name without spaces or slashes.");
        const region = text("s3Region");
        if (!region) throw new Error("Enter the bucket region. Cloudflare R2 uses auto; MinIO and SeaweedFS usually use us-east-1.");
        const accessKeyId = text("s3AccessKeyId");
        const secretAccessKey = secret("s3SecretAccessKey");
        if (!accessKeyId || !secretAccessKey) throw new Error("Enter the access key ID and secret access key.");
        return {
          kind: "s3", endpoint: httpUrl(text("s3Endpoint"), "S3 endpoint URL"), region, bucket,
          prefix: text("s3Prefix").replace(/^\/+|\/+$/g, ""), pathStyle: values.get("s3PathStyle") === "on",
          accessKeyId, secretAccessKey,
        };
      }
      case "google-drive": {
        if (!googleToken.current) throw new Error("Connect Google Drive before continuing.");
        return { kind: "google-drive", connectionToken: googleToken.current, folderName: text("googleFolder") || "Denote Sync" };
      }
      case "git": {
        const remoteUrl = text("gitRemoteUrl");
        if (!remoteUrl) throw new Error("Enter the URL of a dedicated, empty Git repository for sync.");
        const authMode = text("gitAuthMode");
        if (!(authMode in GIT_AUTH_LABELS)) throw new Error("Choose how Denote authenticates to the Git remote.");
        return { kind: "git", remoteUrl, branch: text("gitBranch") || "main", authMode: authMode as SyncGitAuthMode };
      }
    }
  };

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const element = event.currentTarget;
    const values = new FormData(element);
    const passphrase = String(values.get("passphrase") ?? "");
    const confirmation = mode === "create" ? String(values.get("confirmation") ?? "") : null;
    const deviceName = String(values.get("deviceName") ?? "").trim();
    let request: SyncSetupRequest;
    try {
      if (!deviceName) throw new Error("Name this device so you can recognize it on your other devices.");
      const backendValue = backendRequest(values);
      try { checkPassphrase(passphrase, confirmation); }
      catch (caught) {
        const confirmationInput = element.elements.namedItem("confirmation");
        if (confirmationInput instanceof HTMLInputElement) confirmationInput.value = "";
        throw caught;
      }
      request = { mode, deviceName, passphrase, rememberPassphrase: values.get("remember") === "on", backend: backendValue };
    } catch (caught) {
      setValidation(errorMessage(caught));
      return;
    }
    clearSecrets(element);
    googleToken.current = null;
    setGoogleAccount(null);
    setStrength(null);
    setValidation("");
    void perform(async () => {
      accept(await api.syncConfigure(pluginId, request));
      return mode === "create" ? "Sync is set up. The first sync starts shortly." : "Joined sync. Choose a folder for each remote vault below.";
    });
  };

  const connectGoogle = () => {
    const field = form.current?.elements.namedItem("googleClientId");
    const clientId = field instanceof HTMLInputElement && field.value.trim() ? field.value.trim() : null;
    void perform(async () => {
      const connection = await api.syncConnectGoogle(pluginId, clientId);
      if (!connection || typeof connection.connectionToken !== "string" || !connection.connectionToken) {
        throw new Error("Google Drive did not finish connecting. Try again.");
      }
      googleToken.current = connection.connectionToken;
      const label = typeof connection.accountLabel === "string" ? connection.accountLabel : "your Google account";
      setGoogleAccount(label);
      return `Connected Google Drive as ${label}.`;
    });
  };

  return <form ref={form} noValidate onSubmit={submit} aria-label="Set up sync">
    <h3>Set up sync</h3>
    <fieldset className="sync-panel__choice" disabled={busy}>
      <legend>Start</legend>
      <label className="sensitive-tool__check"><input type="radio" name="mode" value="create" checked={mode === "create"}
        onChange={() => { setMode("create"); setValidation(""); }} />Create new sync storage</label>
      <label className="sensitive-tool__check"><input type="radio" name="mode" value="join" checked={mode === "join"}
        onChange={() => { setMode("join"); setValidation(""); }} />Join sync from another device</label>
    </fieldset>
    <label>Device name<input name="deviceName" required maxLength={64} autoComplete="off" disabled={busy}
      aria-describedby={`${hintId}-device`} /></label>
    <p id={`${hintId}-device`} className="authoring-muted">Shown on your other devices, for example “Work laptop”.</p>
    <label>Storage<select name="backend" value={backend} disabled={busy}
      onChange={(event) => { setBackend(event.currentTarget.value as SyncBackendKind); setValidation(""); }}>
      {(Object.keys(BACKEND_LABELS) as SyncBackendKind[]).map((kind) => <option key={kind} value={kind}>{BACKEND_LABELS[kind]}</option>)}
    </select></label>
    {backend === "folder" ? <div className="sync-panel__backend">
      <label>Sync folder<input name="folderPath" value={folderPath} onChange={(event) => setFolderPath(event.currentTarget.value)}
        autoComplete="off" required disabled={busy} /></label>
      <div className="authoring-actions">
        <button type="button" className="secondary-button" disabled={busy} onClick={() => void perform(async () => {
          const path = await api.syncChooseFolder(pluginId, "backend");
          if (path) setFolderPath(path);
        })}>Choose folder…</button>
      </div>
      <p className="authoring-muted">Use a folder outside every vault, such as a mounted network share or a folder another sync tool already replicates.</p>
    </div> : backend === "webdav" ? <div className="sync-panel__backend">
      <label>WebDAV URL<input name="webdavUrl" type="url" inputMode="url" autoComplete="off" required disabled={busy}
        placeholder="https://dav.example.invalid/denote" /></label>
      <label>Username<input name="webdavUsername" autoComplete="off" disabled={busy} /></label>
      <label>Password or app token<input name="webdavPassword" type="password" data-secret autoComplete="off" disabled={busy} /></label>
    </div> : backend === "s3" ? <div className="sync-panel__backend">
      <label>Endpoint URL<input name="s3Endpoint" type="url" inputMode="url" autoComplete="off" required disabled={busy}
        placeholder="https://s3.example.invalid" /></label>
      <label>Region<input name="s3Region" autoComplete="off" required disabled={busy} defaultValue="us-east-1" /></label>
      <label>Bucket<input name="s3Bucket" autoComplete="off" required disabled={busy} /></label>
      <label>Prefix (optional)<input name="s3Prefix" autoComplete="off" disabled={busy} /></label>
      <label className="sensitive-tool__check"><input type="checkbox" name="s3PathStyle" disabled={busy} />
        Use path-style addressing (MinIO, SeaweedFS, and most self-hosted servers)</label>
      <label>Access key ID<input name="s3AccessKeyId" autoComplete="off" required disabled={busy} /></label>
      <label>Secret access key<input name="s3SecretAccessKey" type="password" data-secret autoComplete="off" required disabled={busy} /></label>
    </div> : backend === "google-drive" ? <div className="sync-panel__backend">
      <label>Folder name in Google Drive<input name="googleFolder" autoComplete="off" disabled={busy} defaultValue="Denote Sync" /></label>
      <details>
        <summary>Use your own Google OAuth client</summary>
        <label>OAuth client ID (optional)<input name="googleClientId" autoComplete="off" disabled={busy} /></label>
        <p className="authoring-muted">Leave empty to use Denote's client. A desktop client ID from your own Google Cloud project works too.</p>
      </details>
      <div className="authoring-actions">
        <button type="button" className="secondary-button" disabled={busy} onClick={connectGoogle}>
          {googleAccount ? "Reconnect Google Drive" : "Connect Google Drive"}
        </button>
      </div>
      <p role="status">{googleAccount ? `Connected as ${googleAccount}.` : "Opens your browser to sign in. Denote only asks for access to files it creates."}</p>
    </div> : <div className="sync-panel__backend">
      <label>Remote URL<input name="gitRemoteUrl" autoComplete="off" required disabled={busy}
        placeholder="git@example.invalid:me/denote-sync.git" /></label>
      <label>Branch<input name="gitBranch" autoComplete="off" disabled={busy} defaultValue="main" /></label>
      <label>Authentication<select name="gitAuthMode" defaultValue="system" disabled={busy}>
        {(Object.keys(GIT_AUTH_LABELS) as SyncGitAuthMode[]).map((mode) => <option key={mode} value={mode}>{GIT_AUTH_LABELS[mode]}</option>)}
      </select></label>
      <p className="authoring-muted">Use a dedicated private repository, never one of your vault repositories.</p>
    </div>}
    <label>{mode === "create" ? "New sync passphrase" : "Sync passphrase"}
      <input name="passphrase" type="password" data-secret autoComplete="off" required maxLength={1024} disabled={busy}
        aria-describedby={`${mode === "create" ? `${hintId} ` : ""}${warningId}`}
        onInput={(event) => setStrength(mode === "create" ? passphraseStrength(event.currentTarget.value) : null)} /></label>
    {mode === "create" ? <>
      <PassphraseHint strength={strength} id={hintId} />
      <label>Confirm sync passphrase<input name="confirmation" type="password" data-secret autoComplete="off" required maxLength={1024}
        disabled={busy} /></label>
    </> : null}
    <p id={warningId} className="sync-panel__warning">{mode === "create" ? LOST_PASSPHRASE_WARNING : "Enter the passphrase you chose on your first device."}</p>
    <label className="sensitive-tool__check"><input type="checkbox" name="remember" defaultChecked disabled={busy} />
      Remember the passphrase on this device (stored in the system keychain)</label>
    {validation ? <p role="alert" className="authoring-error">{validation}</p> : null}
    <div className="authoring-actions">
      <button type="submit" className="primary-button" disabled={busy}>{mode === "create" ? "Create sync" : "Join sync"}</button>
    </div>
  </form>;
}

function SyncUnlock({ pluginId, busy, remember, perform, accept }: {
  pluginId: string; busy: boolean; remember: boolean; perform: Perform; accept: Accept;
}) {
  const [validation, setValidation] = useState("");
  const form = useRef<HTMLFormElement>(null);
  useEffect(() => {
    const root = form.current;
    return () => clearSecrets(root);
  }, []);
  return <form ref={form} noValidate aria-label="Unlock sync" onSubmit={(event) => {
    event.preventDefault();
    const values = new FormData(event.currentTarget);
    const passphrase = String(values.get("passphrase") ?? "");
    const rememberPassphrase = values.get("remember") === "on";
    clearSecrets(event.currentTarget);
    if (!passphrase) { setValidation("Enter the sync passphrase."); return; }
    setValidation("");
    void perform(async () => {
      accept(await api.syncUnlock(pluginId, passphrase, rememberPassphrase));
      return "Sync unlocked.";
    });
  }}>
    <h3>Unlock sync</h3>
    <p>Enter the sync passphrase to resume syncing on this device.</p>
    <label>Sync passphrase<input name="passphrase" type="password" data-secret autoComplete="off" required maxLength={1024}
      disabled={busy} autoFocus /></label>
    <label className="sensitive-tool__check"><input type="checkbox" name="remember" defaultChecked={remember} disabled={busy} />
      Remember the passphrase on this device</label>
    {validation ? <p role="alert" className="authoring-error">{validation}</p> : null}
    <div className="authoring-actions">
      <button type="submit" className="primary-button" disabled={busy}>Unlock sync</button>
    </div>
  </form>;
}

function SyncOverview({ status, running, busy, onSyncNow, onRejoin, onCancel, onPause }: {
  status: SyncStatusView; running: boolean; busy: boolean;
  onSyncNow: () => void; onRejoin: () => void; onCancel: () => void; onPause: () => void;
}) {
  const progress = status.progress;
  const lastRun = status.lastRun;
  const removed = status.device?.removed === true;
  return <SyncSection title="Status">
    <dl className="sync-panel__facts">
      <div><dt>State</dt><dd>{running ? STATE_LABELS.running : STATE_LABELS[status.state]}</dd></div>
      {status.backend ? <div><dt>Storage</dt><dd>{BACKEND_LABELS[status.backend.kind] ?? status.backend.kind}: {status.backend.label}</dd></div> : null}
      {status.device ? <div><dt>This device</dt><dd>{status.device.name}</dd></div> : null}
      <div><dt>Schedule</dt><dd>{status.options.paused ? "Paused" :
        status.options.intervalMinutes === 0 ? "Only when you choose Sync now" : `Every ${status.options.intervalMinutes} minutes`}</dd></div>
    </dl>
    {progress ? <div className="sync-panel__progress">
      <label>{PHASE_LABELS[progress.phase] ?? progress.phase}{progress.total ? ` (${progress.done} of ${progress.total})` : ""}
        <progress max={progress.total || undefined} value={progress.total ? progress.done : undefined} /></label>
    </div> : null}
    {lastRun ? <div className="sync-panel__last-run">
      <p><strong>Last sync:</strong> {formatTime(lastRun.finishedAt)}, {lastRun.ok ? "succeeded" : "failed"}.</p>
      {lastRun.message ? <p className={lastRun.ok ? undefined : "authoring-error"}>{lastRun.message}</p> : null}
      <p className="authoring-muted">
        {`Uploaded ${lastRun.uploaded}, downloaded ${lastRun.downloaded}, applied ${lastRun.applied}, conflicts ${lastRun.conflicts}.`}
      </p>
      {lastRun.skipped.length ? <p className="authoring-muted">Skipped: {lastRun.skipped.join(", ")}</p> : null}
    </div> : <p className="authoring-muted">No sync has finished on this device yet.</p>}
    {removed ? <div className="sync-panel__removed">
      <p className="authoring-error">This device was removed from sync. Rejoin to continue.</p>
      <p className="authoring-muted">
        Rejoining gives this device a new identity. Its files are compared with your other devices, and versions that
        differ are kept as conflict copies, so nothing is lost.
      </p>
    </div> : null}
    <div className="authoring-actions">
      {running ? <button type="button" className="secondary-button" disabled={busy} onClick={onCancel}>Cancel sync</button> :
        removed ? <button type="button" className="primary-button" disabled={busy} onClick={onRejoin}>Rejoin sync</button> :
          <button type="button" className="primary-button" disabled={busy} onClick={onSyncNow}>Sync now</button>}
      <button type="button" className="secondary-button" disabled={busy} aria-pressed={status.options.paused} onClick={onPause}>
        {status.options.paused ? "Resume automatic sync" : "Pause automatic sync"}
      </button>
    </div>
  </SyncSection>;
}

function conflictSubject(conflict: SyncConflict, vaults: SyncVault[]): string {
  if (conflict.kind === "encryption-manifest") {
    const vault = vaults.find((candidate) => candidate.syncId === conflict.vaultSyncId);
    return `Encryption settings of ${vault?.name ?? "a vault"}`;
  }
  const what = [conflict.kind === "plugin-setting" ? "Setting" : "Saved data", conflict.key].filter(Boolean).join(" ");
  return `${what} of ${conflict.pluginId ?? "a plugin"}`;
}

function SyncConflicts({ conflicts, vaults, disabled, onResolve }: {
  conflicts: SyncConflict[]; vaults: SyncVault[]; disabled: boolean;
  onResolve: (conflict: SyncConflict, choice: "local" | "remote") => void;
}) {
  if (!conflicts.length) return null;
  return <SyncSection title={`Conflicts (${conflicts.length})`}>
    <p className="authoring-muted">These changes were made on two devices at once. Nothing is lost until you choose. Conflicting note edits are kept as separate “sync-conflict” files in the vault instead.</p>
    <ul className="sync-panel__list">{conflicts.map((conflict) => {
      const subject = conflictSubject(conflict, vaults);
      return <li key={conflict.id} className="sync-panel__item">
        <strong>{subject}</strong>
        <dl className="sync-panel__facts">
          <div><dt>This device</dt><dd>{conflict.localSummary}</dd></div>
          <div><dt>{conflict.remoteDevice}</dt><dd>{conflict.remoteSummary}</dd></div>
          <div><dt>Detected</dt><dd>{formatTime(conflict.detectedAt)}</dd></div>
        </dl>
        <div className="authoring-actions">
          <button type="button" className="secondary-button" disabled={disabled} aria-label={`Keep local version of ${subject}`}
            onClick={() => onResolve(conflict, "local")}>Keep local</button>
          <button type="button" className="secondary-button" disabled={disabled} aria-label={`Use remote version of ${subject}`}
            onClick={() => onResolve(conflict, "remote")}>Use remote</button>
        </div>
      </li>;
    })}</ul>
  </SyncSection>;
}

function SyncVaults({ vaults, disabled, onRequest, onChooseNewFolder }: {
  vaults: SyncVault[]; disabled: boolean;
  onRequest: (request: SyncVaultRequest, message: string) => void;
  onChooseNewFolder: (vault: SyncVault) => void;
}) {
  const [stopping, setStopping] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const [targets, setTargets] = useState<Record<string, string>>({});
  const localCandidates = vaults.filter((vault) => vault.scopeId && !vault.syncId && vault.state === "not-enrolled");
  return <SyncSection title="Vaults">
    {!vaults.length ? <p className="authoring-muted">No vaults yet. Open a vault in Denote, then choose Sync this vault.</p> : null}
    <ul className="sync-panel__list">{vaults.map((vault) => {
      const key = vault.syncId ?? vault.scopeId ?? vault.name;
      const remoteOnly = (vault.state === "pending-mapping" || vault.state === "skipped") && vault.syncId;
      const stoppable = vault.syncId && vault.scopeId &&
        ["synced", "paused-conflict", "paused-encryption", "paused-deletions", "unavailable"].includes(vault.state);
      const target = targets[key] ?? "";
      return <li key={key} className="sync-panel__item">
        <strong>{vault.name}</strong>
        <span>{VAULT_STATE_LABELS[vault.state] ?? vault.state}</span>
        {vault.path ? <span className="authoring-muted sync-panel__path">{vault.path}</span> : null}
        {vault.message ? <span className="authoring-muted">{vault.message}</span> : null}
        <div className="authoring-actions">
          {vault.state === "not-enrolled" && vault.scopeId ? <button type="button" className="secondary-button" disabled={disabled}
            aria-label={`Sync ${vault.name}`}
            onClick={() => onRequest({ action: "enroll", scopeId: vault.scopeId as string }, `${vault.name} will sync from now on.`)}>
            Sync this vault</button> : null}
          {remoteOnly ? <>
            <button type="button" className="secondary-button" disabled={disabled} aria-label={`Choose a new folder for ${vault.name}`}
              onClick={() => onChooseNewFolder(vault)}>Choose new folder…</button>
            {localCandidates.length ? <span className="sync-panel__inline" role="group" aria-label={`Merge ${vault.name} into an existing vault`}>
              <label>Existing vault<select value={target} disabled={disabled}
                onChange={(event) => { const value = event.currentTarget.value; setTargets((current) => ({ ...current, [key]: value })); }}>
                <option value="">Choose a vault…</option>
                {localCandidates.map((candidate) => <option key={candidate.scopeId} value={candidate.scopeId as string}>{candidate.name}</option>)}
              </select></label>
              <button type="button" className="secondary-button" disabled={disabled || !target}
                aria-label={`Merge ${vault.name} into the chosen vault`}
                onClick={() => onRequest({ action: "map-existing", syncId: vault.syncId as string, scopeId: target },
                  `${vault.name} will merge into the chosen vault. Differing files are kept as conflict copies.`)}>Merge</button>
            </span> : null}
            {vault.state === "pending-mapping" ? <button type="button" className="secondary-button" disabled={disabled}
              aria-label={`Skip ${vault.name} on this device`}
              onClick={() => onRequest({ action: "skip", syncId: vault.syncId as string }, `${vault.name} is skipped on this device.`)}>
              Skip on this device</button> : null}
          </> : null}
          {vault.state === "paused-deletions" && vault.syncId ? deleting === key ?
            <span role="group" aria-label={`Apply deletions in ${vault.name}?`} className="sync-panel__confirm">
              <span>The missing files are deleted on every device that syncs {vault.name}. Each device keeps them in its vault trash.</span>
              <button type="button" className="secondary-button" onClick={() => setDeleting(null)}>Keep paused</button>
              <button type="button" className="danger-button" disabled={disabled} onClick={() => {
                setDeleting(null);
                onRequest({ action: "confirm-deletions", syncId: vault.syncId as string },
                  `The missing files of ${vault.name} are deleted on the next sync.`);
              }}>Apply deletions</button>
            </span> : <button type="button" className="secondary-button" disabled={disabled}
              aria-label={`Apply deletions in ${vault.name}`} onClick={() => setDeleting(key)}>Apply deletions…</button> : null}
          {stoppable ? stopping === key ? <span role="group" aria-label={`Stop syncing ${vault.name}?`} className="sync-panel__confirm">
            <span>Local files stay. This device stops publishing changes to {vault.name}; other devices keep their copies.</span>
            <button type="button" className="secondary-button" onClick={() => setStopping(null)}>Keep syncing</button>
            <button type="button" className="danger-button" disabled={disabled} onClick={() => {
              setStopping(null);
              onRequest({ action: "stop", syncId: vault.syncId as string }, `${vault.name} no longer syncs on this device.`);
            }}>Stop syncing</button>
          </span> : <button type="button" className="secondary-button" disabled={disabled} aria-label={`Stop syncing ${vault.name}`}
            onClick={() => setStopping(key)}>Stop syncing</button> : null}
        </div>
      </li>;
    })}</ul>
  </SyncSection>;
}

function SyncDevices({ devices, disabled, onRemove }: {
  devices: SyncDevice[]; disabled: boolean; onRemove: (device: SyncDevice) => void;
}) {
  const [removing, setRemoving] = useState<string | null>(null);
  if (!devices.length) return null;
  return <SyncSection title="Devices">
    <ul className="sync-panel__list">{devices.map((device) => <li key={device.writerId} className="sync-panel__item">
      <strong>{device.name}{device.isSelf ? " (this device)" : ""}</strong>
      <span className="authoring-muted">{device.retired ? "Removed" : `Last synced: ${formatTime(device.lastSeenAt)}`}</span>
      {!device.isSelf && !device.retired ? removing === device.writerId ?
        <span role="group" aria-label={`Remove ${device.name}?`} className="sync-panel__confirm">
          <span>Changes from {device.name} are ignored from now on. It stops syncing until someone chooses Rejoin sync on it, and nothing it has is lost.</span>
          <button type="button" className="secondary-button" onClick={() => setRemoving(null)}>Keep device</button>
          <button type="button" className="danger-button" disabled={disabled} onClick={() => { setRemoving(null); onRemove(device); }}>Remove device</button>
        </span> :
        <div className="authoring-actions">
          <button type="button" className="secondary-button" disabled={disabled} aria-label={`Remove ${device.name}`}
            onClick={() => setRemoving(device.writerId)}>Remove</button>
        </div> : null}
    </li>)}</ul>
  </SyncSection>;
}

function SyncOptionsForm({ options, disabled, onSave }: {
  options: SyncOptions; disabled: boolean; onSave: (options: SyncOptions) => void;
}) {
  const [draft, setDraft] = useState(options);
  const [interval, setIntervalText] = useState(String(options.intervalMinutes));
  const [validation, setValidation] = useState("");
  const hintId = useId();
  return <details className="sync-panel__details">
    <summary>Options</summary>
    <form noValidate aria-label="Sync options" onSubmit={(event) => {
      event.preventDefault();
      const minutes = Number(interval);
      if (!/^\d+$/.test(interval.trim()) || minutes > SYNC_MAX_INTERVAL_MINUTES) {
        setValidation(`Enter a whole number of minutes from 0 to ${SYNC_MAX_INTERVAL_MINUTES}.`);
        return;
      }
      setValidation("");
      onSave({ ...draft, paused: options.paused, intervalMinutes: minutes });
    }}>
      <label>Sync every (minutes)<input type="number" min={0} max={SYNC_MAX_INTERVAL_MINUTES} step={1} value={interval}
        disabled={disabled} aria-describedby={hintId} onChange={(event) => setIntervalText(event.currentTarget.value)} /></label>
      <p id={hintId} className="authoring-muted">0 syncs only when you choose Sync now.</p>
      <label className="sensitive-tool__check"><input type="checkbox" checked={draft.syncHistory} disabled={disabled}
        onChange={(event) => { const checked = event.currentTarget.checked; setDraft((current) => ({ ...current, syncHistory: checked })); }} />
        Sync revision history</label>
      <label className="sensitive-tool__check"><input type="checkbox" checked={draft.syncPreferences} disabled={disabled}
        onChange={(event) => { const checked = event.currentTarget.checked; setDraft((current) => ({ ...current, syncPreferences: checked })); }} />
        Sync preferences (theme, editor display, view mode, dotfiles, activity rail, dictation shortcut)</label>
      <label className="sensitive-tool__check"><input type="checkbox" checked={draft.syncPluginData} disabled={disabled}
        onChange={(event) => { const checked = event.currentTarget.checked; setDraft((current) => ({ ...current, syncPluginData: checked })); }} />
        Sync plugin settings and plugin data</label>
      {validation ? <p role="alert" className="authoring-error">{validation}</p> : null}
      <div className="authoring-actions">
        <button type="submit" className="secondary-button" disabled={disabled}>Save options</button>
      </div>
    </form>
  </details>;
}

function SyncChangePassphrase({ pluginId, busy, perform, accept }: { pluginId: string; busy: boolean; perform: Perform; accept: Accept }) {
  const [strength, setStrength] = useState<PassphraseStrength | null>(null);
  const [validation, setValidation] = useState("");
  const form = useRef<HTMLFormElement>(null);
  const hintId = useId();
  const warningId = useId();
  useEffect(() => {
    const root = form.current;
    return () => clearSecrets(root);
  }, []);
  return <details className="sync-panel__details">
    <summary>Change passphrase</summary>
    <form ref={form} noValidate aria-label="Change sync passphrase" onSubmit={(event) => {
      event.preventDefault();
      const values = new FormData(event.currentTarget);
      const current = String(values.get("current") ?? "");
      const next = String(values.get("next") ?? "");
      const confirmation = String(values.get("confirmation") ?? "");
      clearSecrets(event.currentTarget);
      setStrength(null);
      try {
        if (!current) throw new Error("Enter the current sync passphrase.");
        checkPassphrase(next, confirmation);
      } catch (caught) { setValidation(errorMessage(caught)); return; }
      setValidation("");
      void perform(async () => {
        accept(await api.syncChangePassphrase(pluginId, current, next));
        return "Sync passphrase changed. Other devices ask for the new passphrase.";
      });
    }}>
      <label>Current passphrase<input name="current" type="password" data-secret autoComplete="off" maxLength={1024} disabled={busy} /></label>
      <label>New passphrase<input name="next" type="password" data-secret autoComplete="off" maxLength={1024} disabled={busy}
        aria-describedby={`${hintId} ${warningId}`} onInput={(event) => setStrength(passphraseStrength(event.currentTarget.value))} /></label>
      <PassphraseHint strength={strength} id={hintId} />
      <label>Confirm new passphrase<input name="confirmation" type="password" data-secret autoComplete="off" maxLength={1024} disabled={busy} /></label>
      <p id={warningId} className="sync-panel__warning">{LOST_PASSPHRASE_WARNING}</p>
      {validation ? <p role="alert" className="authoring-error">{validation}</p> : null}
      <div className="authoring-actions">
        <button type="submit" className="secondary-button" disabled={busy}>Change passphrase</button>
      </div>
    </form>
  </details>;
}

function SyncDisconnect({ pluginId, busy, perform, accept }: { pluginId: string; busy: boolean; perform: Perform; accept: Accept }) {
  const [confirming, setConfirming] = useState(false);
  const explanation = "Disconnecting removes this device's sync settings, sync index, saved storage credentials, and remembered passphrase. Local vault files and the data in your sync storage are kept, so you can join again later with the passphrase.";
  return <SyncSection title="Disconnect">
    {confirming ? <div role="group" aria-label="Disconnect this device from sync?" className="sync-panel__confirm">
      <p>{explanation}</p>
      <div className="authoring-actions">
        <button type="button" className="secondary-button" onClick={() => setConfirming(false)}>Stay connected</button>
        <button type="button" className="danger-button" disabled={busy} onClick={() => {
          setConfirming(false);
          void perform(async () => {
            accept(await api.syncDisconnect(pluginId));
            return "This device is disconnected from sync. Local files and remote data were kept.";
          });
        }}>Disconnect</button>
      </div>
    </div> : <>
      <p className="authoring-muted">Stop syncing on this device without deleting anything.</p>
      <div className="authoring-actions">
        <button type="button" className="secondary-button" disabled={busy} onClick={() => setConfirming(true)}>Disconnect this device…</button>
      </div>
    </>}
  </SyncSection>;
}

/** Hosts the Sync panel where the vault sidebar is unavailable: no vault open, or a locked encrypted vault. */
export function SyncDialog({ open, ...props }: SyncPanelProps & { open: boolean }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const element = dialog.current;
    if (!element) return;
    if (open && !element.open) element.showModal();
    else if (!open && element.open) element.close();
  }, [open]);
  return <dialog ref={dialog} className="app-dialog sync-dialog" aria-label="Sync"
    onCancel={(event) => { event.preventDefault(); props.onClose(); }}>
    {open ? <SyncPanel {...props} /> : null}
  </dialog>;
}
