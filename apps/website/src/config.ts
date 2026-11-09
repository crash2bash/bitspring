// Store links. While a link is null its buttons say "coming soon" and point at the download
// section; fill them in at release and every button on every language page picks them up.
export const LINKS: { appStore: string | null; windows: string | null } = {
  appStore: null,
  windows: null,
};

// Public origin of the site, e.g. "https://hifi.example". Used at build time for hreflang,
// canonical and og:image URLs, which search engines and link previews want absolute.
export const SITE_URL = "";
