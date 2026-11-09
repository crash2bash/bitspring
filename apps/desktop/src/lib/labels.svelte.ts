// What to call queue entries that aren't library files with tags in their names — streaming
// tracks above all (their "path" is an id like qobuz:track:123). Filled wherever such tracks are
// put in the queue, read by the queue list.
import { trackName } from "./api";

export interface Label {
  title: string;
  artist: string | null;
}

export const labels: Record<string, Label> = $state({});

export function remember(list: { uri?: string | null; path?: string; title: string; artist?: string | null }[]) {
  for (const t of list) {
    const key = t.uri ?? t.path;
    if (key) labels[key] = { title: t.title, artist: t.artist ?? null };
  }
}

export function labelOf(path: string): string {
  return labels[path]?.title ?? trackName(path);
}
