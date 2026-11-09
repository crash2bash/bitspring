// Tauri doesn't have a Node.js server to do proper SSR
// so we use adapter-static with a fallback to index.html to put the site in SPA mode
// See: https://svelte.dev/docs/kit/single-page-apps
// See: https://v2.tauri.app/start/frontend/sveltekit/ for more info
import { api, inTauri } from "$lib/api";
import { setSystemLanguages } from "$lib/i18n.svelte";

export const ssr = false;

// Ask the OS for its language before the first paint, so the UI doesn't flash in English.
export async function load() {
  if (!inTauri) return;
  try {
    setSystemLanguages(await api.systemLocales());
  } catch {
    // an older backend without the command: navigator.languages stays in charge
  }
}
