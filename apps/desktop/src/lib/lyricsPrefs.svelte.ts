// Whether the app may ask LRCLIB (lrclib.net) for lyrics a track doesn't carry itself. Off until
// the listener switches it on: the app goes online only for what it's asked to do. Per machine.

const KEY = "lyricsOnline";

function loadOnline(): boolean {
  try {
    return JSON.parse(localStorage.getItem(KEY) ?? "false") === true;
  } catch {
    return false;
  }
}

const state = $state({ online: typeof localStorage !== "undefined" ? loadOnline() : false });

export const lyricsPrefs = {
  get online(): boolean {
    return state.online;
  },
  set online(v: boolean) {
    state.online = v;
    try {
      localStorage.setItem(KEY, JSON.stringify(v));
    } catch {}
  },
};
