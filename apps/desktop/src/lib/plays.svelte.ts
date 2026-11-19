// How often each track was listened to and when last: counted by the backend (on this device
// only), fetched again whenever it counts another listen.
import { api, type Played } from "./api";

const state = $state<{ map: Record<string, Played> }>({ map: {} });
let seen = -1;

export const plays = {
  /** By track path; a track that isn't here was never listened to. */
  get map(): Record<string, Played> {
    return state.map;
  },
};

/** `rev`: Status.playsRev, which moves when a listen is counted. */
export async function syncPlays(rev: number) {
  if (rev === seen) return;
  seen = rev;
  try {
    state.map = await api.plays();
  } catch {
    seen = -1; // asked again on the next status
  }
}
