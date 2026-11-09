import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { defineConfig, type Plugin } from "vite";

import { SITE_URL } from "./src/config";
import { DEFAULT_LOCALE, LOCALES } from "./src/i18n/locales";
import { renderPage } from "./src/i18n/render";

/** One static page per language: "/" for English, "/de/", "/ja/", … for the rest. */
function i18nPages(): Plugin {
  return {
    name: "i18n-pages",
    enforce: "post",
    configureServer(server) {
      const template = () => readFileSync(resolve(server.config.root, "index.html"), "utf8");
      server.watcher.add(resolve(server.config.root, "src/i18n/locales"));
      server.watcher.on("change", (file) => {
        if (file.endsWith(".json") && file.includes("/i18n/locales/")) server.ws.send({ type: "full-reload" });
      });
      server.middlewares.use(async (req, res, next) => {
        const url = (req.url ?? "/").split("?")[0];
        const bare = url.match(/^\/([a-z]{2}(?:-[a-z]+)?)$/);
        if (bare && LOCALES.some((l) => l.path === bare[1])) {
          res.writeHead(301, { Location: `${url}/` }).end();
          return;
        }
        const m = url.match(/^\/(?:([a-z]{2}(?:-[a-z]+)?)\/)?(?:index\.html)?$/);
        const locale = m && (m[1] ? LOCALES.find((l) => l.path === m[1]) : DEFAULT_LOCALE);
        if (!locale) return next();
        try {
          const html = await server.transformIndexHtml(url, renderPage(template(), locale, { site: SITE_URL }));
          res.setHeader("Content-Type", "text/html; charset=utf-8");
          res.end(html);
        } catch (e) {
          next(e);
        }
      });
    },
    // Vite has emitted index.html with hashed asset URLs by now; fan it out per language.
    generateBundle(_, bundle) {
      const page = bundle["index.html"];
      if (!page || page.type !== "asset") return;
      const template = String(page.source);
      for (const l of LOCALES) {
        const html = renderPage(template, l, { site: SITE_URL });
        if (l === DEFAULT_LOCALE) page.source = html;
        else this.emitFile({ type: "asset", fileName: `${l.path}/index.html`, source: html });
      }
    },
  };
}

export default defineConfig({
  plugins: [i18nPages()],
  server: { host: "127.0.0.1", port: 5180, strictPort: true },
  // three.js is most of the bundle (~185 kB gzipped) and the hero needs it immediately.
  build: { chunkSizeWarningLimit: 800 },
});
