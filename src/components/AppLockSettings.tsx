import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { errorMessage } from "../lib/api";
import type { AppLockConfigurationRequest, AppLockStatus } from "../types";

export function AppLockSettings({
  status,
  active = true,
  disabled,
  onConfigure,
  onLock,
}: {
  status: AppLockStatus;
  active?: boolean;
  disabled: boolean;
  onConfigure: (request: AppLockConfigurationRequest) => Promise<void>;
  onLock: () => Promise<void>;
}) {
  const [enabled, setEnabled] = useState(status.enabled);
  const [passwordEnabled, setPasswordEnabled] = useState(status.passwordEnabled || !status.enabled);
  const [deviceEnabled, setDeviceEnabled] = useState(status.deviceAuthenticationEnabled);
  const [minutes, setMinutes] = useState(status.inactivityMinutes?.toString() ?? "");
  const [changePassword, setChangePassword] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const currentRef = useRef<HTMLInputElement>(null);
  const passwordRef = useRef<HTMLInputElement>(null);
  const confirmationRef = useRef<HTMLInputElement>(null);
  const newPasswordNeeded = enabled && passwordEnabled && (!status.passwordEnabled || changePassword);
  const clearPasswords = () => {
    for (const field of [currentRef.current, passwordRef.current, confirmationRef.current]) {
      if (field) field.value = "";
    }
  };

  useEffect(() => {
    setEnabled(status.enabled);
    setPasswordEnabled(status.passwordEnabled || !status.enabled);
    setDeviceEnabled(status.deviceAuthenticationEnabled);
    setMinutes(status.inactivityMinutes?.toString() ?? "");
    setChangePassword(false);
    clearPasswords();
  }, [active, status.enabled, status.passwordEnabled, status.deviceAuthenticationEnabled, status.inactivityMinutes]);
  useEffect(() => {
    setError(null);
    setSaved(false);
  }, [active]);

  const save = async () => {
    const value = passwordRef.current?.value ?? "";
    const confirmation = confirmationRef.current?.value ?? "";
    const currentPassword = currentRef.current?.value || null;
    clearPasswords();
    setError(null);
    setSaved(false);
    if (enabled && !passwordEnabled && !deviceEnabled) {
      setError("Choose at least one unlock method.");
      return;
    }
    if (newPasswordNeeded) {
      if ([...value].length < 12 || !value.trim() || new TextEncoder().encode(value).length > 1024) {
        setError("Use at least 12 characters, not only whitespace, and no more than 1,024 bytes.");
        return;
      }
      if (value !== confirmation) {
        setError("App passwords do not match.");
        return;
      }
    }
    if (status.passwordEnabled && !currentPassword) {
      setError("Enter the current app password to change app lock.");
      currentRef.current?.focus();
      return;
    }
    setBusy(true);
    try {
      await onConfigure({
        password: !enabled || !passwordEnabled
          ? { kind: "remove" }
          : newPasswordNeeded ? { kind: "set", value } : { kind: "keep" },
        currentPassword,
        deviceAuthenticationEnabled: enabled && deviceEnabled,
        inactivityMinutes: enabled && minutes ? Number(minutes) : null,
      });
      setSaved(true);
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setBusy(false);
    }
  };

  const lockDisabled = disabled || busy;
  return (
    <form className="editor-settings-dialog__body app-lock-settings" aria-label="App lock settings"
      aria-busy={busy} onSubmit={(event) => { event.preventDefault(); void save(); }}>
      <AppLockSection title="App lock"
        description="Lock Denote on startup. This hides the workspace, not files on disk. Vault encryption stays separate.">
        <div className="editor-settings-list">
          <AppLockOption label="Enable app lock" description="Require an unlock method before showing the workspace."
            checked={enabled} disabled={lockDisabled} onChange={(checked) => {
              setEnabled(checked);
              clearPasswords();
              setSaved(false);
            }} />
        </div>
      </AppLockSection>
      {enabled ? (
        <>
          <AppLockSection title="Unlock methods"
            description="Either enabled method can unlock the app. Device authentication is requested before enablement.">
            <div className="editor-settings-list">
              <AppLockOption label="App password" description="Use at least 12 characters. Denote cannot recover a forgotten password."
                checked={passwordEnabled} disabled={lockDisabled} onChange={(checked) => {
                  setPasswordEnabled(checked);
                  clearPasswords();
                  setSaved(false);
                }} />
              <AppLockOption label={status.deviceAuthentication.name}
                description={status.deviceAuthentication.available
                  ? "Unlock with this device's system authentication."
                  : status.deviceAuthentication.unavailableReason ?? "Device authentication is unavailable."}
                checked={deviceEnabled} disabled={lockDisabled || (!status.deviceAuthentication.available && !deviceEnabled)}
                onChange={(checked) => { setDeviceEnabled(checked); setSaved(false); }} />
              {newPasswordNeeded ? (
                <>
                  <AppLockField label="New app password">
                    {(id) => <input id={id} ref={passwordRef} type="password" autoComplete="new-password"
                      maxLength={1024} disabled={lockDisabled} />}
                  </AppLockField>
                  <AppLockField label="Confirm app password">
                    {(id) => <input id={id} ref={confirmationRef} type="password" autoComplete="new-password"
                      maxLength={1024} disabled={lockDisabled} />}
                  </AppLockField>
                </>
              ) : null}
            </div>
            {passwordEnabled && status.passwordEnabled ? (
              <button type="button" className="secondary-button app-lock-settings__inline-action" disabled={lockDisabled}
                onClick={() => { setChangePassword((current) => !current); clearPasswords(); setSaved(false); }}>
                {changePassword ? "Keep current password" : "Change app password"}
              </button>
            ) : null}
          </AppLockSection>
          <AppLockSection>
            <AppLockField label="Lock after inactivity"
              description="Locking stops terminal jobs and native tools. Inactivity locking is disabled until you choose a timeout.">
              {(id, describedBy) => <select id={id} aria-describedby={describedBy} value={minutes} disabled={lockDisabled}
                onChange={(event) => { setMinutes(event.currentTarget.value); setSaved(false); }}>
                <option value="">Disabled</option>
                {[1, 5, 15, 30, 60, 120, 240].map((value) =>
                  <option key={value} value={value}>{value} {value === 1 ? "minute" : "minutes"}</option>,
                )}
              </select>}
            </AppLockField>
          </AppLockSection>
        </>
      ) : null}
      {status.passwordEnabled ? (
        <AppLockSection>
          <AppLockField label="Current app password"
            description="Required to change or disable app lock. Denote cannot recover a forgotten password.">
            {(id, describedBy) => <input id={id} aria-describedby={describedBy} ref={currentRef} type="password"
              autoComplete="current-password" maxLength={1024} disabled={lockDisabled} />}
          </AppLockField>
        </AppLockSection>
      ) : status.enabled ? (
        <p className="app-lock-settings__note">Confirm with {status.deviceAuthentication.name} to change or disable app lock.</p>
      ) : null}
      {error ? <p className="app-lock-settings__message app-lock-settings__message--error" role="alert">{error}</p> : null}
      {saved ? <p className="app-lock-settings__message" role="status">App lock saved.</p> : null}
      <div className="app-lock-settings__actions">
        {status.enabled ? (
          <button type="button" className="secondary-button" disabled={lockDisabled}
            onClick={() => { void onLock().catch((caught: unknown) => setError(errorMessage(caught))); }}>
            Lock app now
          </button>
        ) : null}
        <button type="submit" className="primary-button" disabled={lockDisabled || (!enabled && !status.enabled)}>
          {busy ? "Saving…" : "Save app lock"}
        </button>
      </div>
    </form>
  );
}

function AppLockSection({ title, description, children }: { title?: string; description?: string; children: ReactNode }) {
  const id = useId();
  return (
    <section className="app-lock-settings__section" aria-labelledby={title ? id : undefined}>
      {title ? (
        <div className="app-lock-settings__heading">
          <h3 id={id}>{title}</h3>
          {description ? <p>{description}</p> : null}
        </div>
      ) : null}
      {children}
    </section>
  );
}

function AppLockOption({ label, description, checked, disabled, onChange }: {
  label: string;
  description: string;
  checked: boolean;
  disabled: boolean;
  onChange: (checked: boolean) => void;
}) {
  const id = useId();
  return (
    <label className="editor-setting">
      <input type="checkbox" aria-labelledby={`${id}-label`} aria-describedby={`${id}-description`}
        checked={checked} disabled={disabled} onChange={(event) => onChange(event.currentTarget.checked)} />
      <span>
        <strong id={`${id}-label`}>{label}</strong>
        <small id={`${id}-description`}>{description}</small>
      </span>
    </label>
  );
}

function AppLockField({ label, description, children }: {
  label: string;
  description?: string;
  children: (id: string, describedBy: string | undefined) => ReactNode;
}) {
  const id = useId();
  const describedBy = description ? `${id}-description` : undefined;
  return (
    <div className="app-lock-settings__field">
      <span>
        <label htmlFor={id}>{label}</label>
        {description ? <small id={describedBy}>{description}</small> : null}
      </span>
      {children(id, describedBy)}
    </div>
  );
}
