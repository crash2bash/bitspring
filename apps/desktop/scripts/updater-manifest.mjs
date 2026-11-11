// Writes latest.json for a release: the file every installed website build (Mac DMG, Windows,
// Android APK) reads to learn about a new version. Upload it to the GitHub release next to the
// installers; https://github.com/crash2bash/hi-fi/releases/latest/download/latest.json then
// always points at the newest one.
//
//   node scripts/updater-manifest.mjs --version 0.3.0 --notes notes.md \
//     --mac-sig <Hifi.app.tar.gz.sig> --win-sig <Hifi-windows-setup.exe.sig> > latest.json
//
// Expects these release assets (names are what the URLs below point at):
//   Hifi-mac.app.tar.gz   the updater archive of the universal Mac app (bundle/macos/Hifi.app.tar.gz)
//   Hifi-windows-setup.exe  the NSIS installer (signed by the same build: .exe.sig)
//   Hifi-android.apk      the release-signed APK (Android checks the signing key itself)
// --base overrides the download address (for testing against a local server).
import { readFileSync } from "node:fs";

const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, a, i, all) => (a.startsWith("--") ? [...acc, [a.slice(2), all[i + 1]]] : acc), []),
);
for (const k of ["version", "mac-sig", "win-sig"]) if (!args[k]) throw new Error(`--${k} is required`);

const base = args.base ?? `https://github.com/crash2bash/hi-fi/releases/download/v${args.version}`;
const sig = (p) => readFileSync(p, "utf8").trim();
// Only the "What's new" part of the release notes: the rest is install instructions.
let notes = args.notes ? readFileSync(args.notes, "utf8") : "";
const m = notes.match(/\*\*What's new\*\*\s*\n([\s\S]*?)(\n\s*\n|$)/);
if (m) notes = m[1].trim();

const mac = { signature: sig(args["mac-sig"]), url: `${base}/Hifi-mac.app.tar.gz` };
const manifest = {
  version: args.version,
  notes,
  pub_date: new Date().toISOString().replace(/\.\d+Z$/, "Z"),
  platforms: {
    // One universal app serves both Mac architectures.
    "darwin-aarch64": mac,
    "darwin-x86_64": mac,
    "windows-x86_64": { signature: sig(args["win-sig"]), url: `${base}/Hifi-windows-setup.exe` },
    // Read by our own Android updater (Tauri's has no Android); no signature field: Android
    // only installs an update signed with the same key as the installed app.
    "android-aarch64": { signature: "", url: `${base}/Hifi-android.apk` },
  },
};
process.stdout.write(JSON.stringify(manifest, null, 2) + "\n");
