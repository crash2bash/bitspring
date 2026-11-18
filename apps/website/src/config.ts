// Where each build of the app is published. While a link is null, every button for it says
// "coming soon"; fill them in at release and all pages in every language pick them up.
//
// Files (.dmg / .exe / .apk / .AppImage / .deb) can live on GitHub Releases: the repository is public, and
// https://github.com/crash2bash/bitspring/releases/latest/download/<asset-name> always points at
// the newest release, as long as the asset keeps the same name.
export const LINKS: {
  /** Mac App Store page, e.g. https://apps.apple.com/app/id0000000000 */
  macAppStore: string | null;
  /** Direct .dmg download. */
  macFile: string | null;
  /** App Store page for iPhone and iPad (the same apps.apple.com URL if it is a universal purchase). */
  iosAppStore: string | null;
  /** Microsoft Store page, e.g. https://apps.microsoft.com/detail/9XXXXXXXXXXX */
  windowsStore: string | null;
  /** Direct installer .exe download. */
  windowsFile: string | null;
  /** Google Play page or a direct .apk download. */
  android: string | null;
  /** AppImage: runs on any distribution; updates itself. */
  linuxFile: string | null;
  /** .deb package for Ubuntu, Debian and their relatives. */
  linuxDeb: string | null;
} = {
  macAppStore: null,
  macFile: "https://github.com/crash2bash/bitspring/releases/latest/download/BitSpring-mac.dmg",
  iosAppStore: null,
  windowsStore: "https://apps.microsoft.com/detail/9NDVJBXLSHVM",
  windowsFile: "https://github.com/crash2bash/bitspring/releases/latest/download/BitSpring-windows-setup.exe",
  android: "https://github.com/crash2bash/bitspring/releases/latest/download/BitSpring-android.apk",
  linuxFile: "https://github.com/crash2bash/bitspring/releases/latest/download/BitSpring-linux.AppImage",
  linuxDeb: "https://github.com/crash2bash/bitspring/releases/latest/download/BitSpring-linux.deb",
};

// Public origin of the site, e.g. "https://bitspring.app". Used at build time for hreflang,
// canonical and og:image URLs, which search engines and link previews want absolute.
export const SITE_URL = "https://bitspring.app";
