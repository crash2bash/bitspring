import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { defineConfig, type Plugin } from "vite";

import { SITE_URL } from "./src/config";
import { DEFAULT_LOCALE, LOCALES } from "./src/i18n/locales";
import { renderPage } from "./src/i18n/render";

/** Page templates, by their path below a language root. */
const PAGES: Record<string, string> = {
  "": "index.html",
  "download/": "download/index.html",
};

const LOCALE_PATH = "[a-z]{2}(?:-[a-z]+)?";

/** One static page per language and template: "/", "/download/", "/de/", "/de/download/", … */
function i18nPages(): Plugin {
  return {
    name: "i18n-pages",
    enforce: "post",
    configureServer(server) {
      const root = server.config.root;
      server.watcher.add(resolve(root, "src/i18n/locales"));
      server.watcher.on("change", (file) => {
        if (file.endsWith(".json") && file.includes("/i18n/locales/")) server.ws.send({ type: "full-reload" });
      });
      server.middlewares.use(async (req, res, next) => {
        const url = (req.url ?? "/").split("?")[0];
        // "/de" or "/download" without the trailing slash: redirect like the host will.
        if (!url.endsWith("/") && !url.includes(".") && /^\/[\w-]+(\/[\w-]+)?$/.test(url)) {
          const [, a, b] = url.split("/");
          const isPage = (s: string | undefined) => s !== undefined && `${s}/` in PAGES;
          const isLocale = (s: string | undefined) => LOCALES.some((l) => l.path === s);
          if ((b === undefined && (isPage(a) || isLocale(a))) || (isLocale(a) && isPage(b))) {
            res.writeHead(301, { Location: `${url}/${req.url?.includes("?") ? "?" + req.url.split("?")[1] : ""}` }).end();
            return;
          }
        }
        const m = url.match(new RegExp(`^/(?:(${LOCALE_PATH})/)?((?:[\\w-]+/)?)(?:index\\.html)?$`));
        if (!m) return next();
        const [, localePath, page] = m;
        const locale = localePath ? LOCALES.find((l) => l.path === localePath) : DEFAULT_LOCALE;
        if (!locale || !(page in PAGES)) return next();
        try {
          const template = readFileSync(resolve(root, PAGES[page]), "utf8");
          const html = await server.transformIndexHtml(url, renderPage(template, locale, { site: SITE_URL, page }));
          res.setHeader("Content-Type", "text/html; charset=utf-8");
          res.end(html);
        } catch (e) {
          next(e);
        }
      });
    },
    // Vite has emitted each template with hashed asset URLs by now; fan them out per language.
    generateBundle(_, bundle) {
      for (const [page, file] of Object.entries(PAGES)) {
        const asset = bundle[file];
        if (!asset || asset.type !== "asset") continue;
        const template = String(asset.source);
        for (const l of LOCALES) {
          const html = renderPage(template, l, { site: SITE_URL, page });
          if (l === DEFAULT_LOCALE) asset.source = html;
          else this.emitFile({ type: "asset", fileName: `${l.path}/${file}`, source: html });
        }
      }
    },
  };
}

export default defineConfig({
  plugins: [i18nPages()],
  server: { host: "127.0.0.1", port: 5180, strictPort: true },
  build: {
    rolldownOptions: {
      input: Object.fromEntries(Object.values(PAGES).map((f) => [f.replace(/\/?index\.html$/, "") || "home", resolve(import.meta.dirname, f)])),
    },
    // three.js is most of the home page bundle (~185 kB gzipped) and the hero needs it immediately.
    chunkSizeWarningLimit: 800,
  },
});
