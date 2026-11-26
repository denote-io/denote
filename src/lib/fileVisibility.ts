import { notifySyncPreferenceChanged } from "./syncPreferences";

const STORAGE_KEY = "denote-show-dotfiles";

export function getShowDotfiles(): boolean {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    return stored !== "false";
  } catch {
    return true;
  }
}

export function saveShowDotfiles(showDotfiles: boolean): boolean {
  localStorage.setItem(STORAGE_KEY, String(showDotfiles));
  notifySyncPreferenceChanged(STORAGE_KEY);
  return showDotfiles;
}
