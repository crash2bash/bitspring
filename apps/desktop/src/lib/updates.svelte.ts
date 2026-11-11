// Updates for the website builds (Mac DMG, Windows, Android APK): checked quietly at start and
// once a day, shown as a notice, installed only when asked. App Store builds report
// `enabled: false` and none of this shows.

import { api, errorText, isAndroid } from "./api";

export interface UpdateInfo {
  version: string;
  notes: string | null;
  date: string | null;
}

export const updates = $state({
  enabled: false,
  current: "",
  update: null as UpdateInfo | null,
  checking: false,
  /** A manual check found nothing newer (for the message in Settings). */
  upToDate: false,
  installing: false,
  /** 0…1, or null while the size isn't known. */
  progress: null as number | null,
  error: null as string | null,
  /** "Later" for this version, until the app starts again. */
  dismissed: null as string | null,
});

export async function checkUpdates(manual = false) {
  if (updates.checking || updates.installing) return;
  updates.checking = true;
  updates.upToDate = false;
  if (manual) updates.error = null;
  try {
    const r = await api.updateCheck();
    updates.enabled = r.enabled;
    updates.current = r.current;
    updates.update = r.update;
    updates.upToDate = manual && !r.update;
    if (manual && r.update) updates.dismissed = null;
  } catch (e) {
    // A failed background check (offline) stays quiet; a manual one says why.
    if (manual) updates.error = errorText(e);
  } finally {
    updates.checking = false;
  }
}

export async function installUpdate() {
  if (updates.installing) return;
  updates.installing = true;
  updates.error = null;
  updates.progress = null;
  const timer = setInterval(async () => {
    const [done, total] = await api.updateProgress();
    updates.progress = total > 0 ? Math.min(1, done / total) : null;
  }, 250);
  try {
    // Mac and Windows restart into the new version and never return here; Android opens
    // the system installer.
    await api.updateInstall();
  } catch (e) {
    updates.error = errorText(e);
  } finally {
    clearInterval(timer);
    updates.installing = false;
    if (isAndroid) updates.progress = null;
  }
}

/** First check a little after start, then daily. */
export function startUpdateChecks(): () => void {
  const first = setTimeout(() => checkUpdates(), 8000);
  const daily = setInterval(() => checkUpdates(), 24 * 3600 * 1000);
  return () => {
    clearTimeout(first);
    clearInterval(daily);
  };
}

/** Release notes as plain lines (they are written in Markdown on GitHub). */
export function plainNotes(notes: string | null): string[] {
  if (!notes) return [];
  return notes
    .split("\n")
    .map((l) => l.replace(/\*\*/g, "").replace(/^\s*[-*]\s+/, "• ").replace(/`/g, "").trim())
    .filter(Boolean)
    .slice(0, 12);
}
