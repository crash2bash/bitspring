// Scrobbling accounts (Last.fm, ListenBrainz) as Settings shows them. All of it lives in the
// backend; this only mirrors its status and passes on what the user does.
import { api, errorText, type ScrobbleService, type ScrobbleStatus } from "./api";

export const scrobble = $state<{
  services: ScrobbleStatus[];
  /** What went wrong the last time the user tried to connect a service. */
  error: Partial<Record<ScrobbleService, string>>;
  /** The service a request is out for. */
  busy: ScrobbleService | null;
}>({ services: [], error: {}, busy: null });

export async function loadScrobble() {
  try {
    scrobble.services = await api.scrobbleStatus();
  } catch {}
}

async function act(service: ScrobbleService, what: () => Promise<ScrobbleStatus[]>) {
  scrobble.busy = service;
  delete scrobble.error[service];
  try {
    scrobble.services = await what();
  } catch (e) {
    scrobble.error[service] = errorText(e);
  } finally {
    scrobble.busy = null;
  }
}

export const connectToken = (service: ScrobbleService, token: string) => act(service, () => api.scrobbleConnectToken(service, token));
export const signIn = (service: ScrobbleService) => act(service, () => api.scrobbleSignIn(service));
export const cancelSignIn = (service: ScrobbleService) => act(service, () => api.scrobbleCancel());
export const disconnect = (service: ScrobbleService) => act(service, () => api.scrobbleDisconnect(service));
