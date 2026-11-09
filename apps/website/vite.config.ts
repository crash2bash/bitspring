import { defineConfig } from "vite";

export default defineConfig({
  server: { host: "127.0.0.1", port: 5180, strictPort: true },
  // three.js is most of the bundle (~185 kB gzipped) and the hero needs it immediately.
  build: { chunkSizeWarningLimit: 800 },
});
