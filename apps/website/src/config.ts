// Where each build of the app is published. While a link is null, every button for it says
// "coming soon"; fill them in at release and all pages in every language pick them up.
//
// Files (.dmg / .exe / .apk) can live on GitHub Releases: the repository is public, and
// https://github.com/crash2bash/hi-fi/releases/latest/download/<asset-name> always points at
// the newest release, as long as the asset keeps the same name.
export const LINKS: {
  /** Mac App Store page, e.g. https://apps.apple.com/app/id0000000000 */
  macAppStore: string | null;
  /** Direct .dmg download. */
  macFile: string | null;
  /** App Store page for iPhone and iPad (the same apps.apple.com URL if it is a universal purchase). */
  iosAppStore: string | null;
  /** Direct installer .exe download. */
  windowsFile: string | null;
  /** Google Play page or a direct .apk download. */
  android: string | null;
} = {
  macAppStore: null,
  macFile: "https://github.com/crash2bash/hi-fi/releases/latest/download/Hifi-mac.dmg",
  iosAppStore: null,
  windowsFile: null,
  android: "https://github.com/crash2bash/hi-fi/releases/latest/download/Hifi-android.apk",
};

// Public origin of the site, e.g. "https://hifi.example". Used at build time for hreflang,
// canonical and og:image URLs, which search engines and link previews want absolute.
export const SITE_URL = "https://hifi-player-nu.vercel.app";
