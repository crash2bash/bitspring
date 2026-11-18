// Which build of the app fits the visitor's device.

import { LINKS } from "./config";

export type Platform = "mac" | "ios" | "windows" | "android" | "linux" | "other";

export function detectPlatform(): Platform {
  // ?platform=mac etc. overrides detection: handy for links in emails and for testing.
  const forced = new URLSearchParams(location.search).get("platform");
  if (forced === "mac" || forced === "ios" || forced === "windows" || forced === "android" || forced === "linux") return forced;

  const nav = navigator as Navigator & { userAgentData?: { platform?: string } };
  const ua = nav.userAgent;
  const platform = nav.userAgentData?.platform || nav.platform || "";
  // iPadOS Safari presents itself as a Mac; Macs report no touch points, iPads do.
  if (/iPhone|iPad|iPod/.test(ua) || (/mac/i.test(platform) && nav.maxTouchPoints > 0)) return "ios";
  if (/Android/i.test(ua)) return "android";
  if (/Mac/i.test(platform) || /Mac OS X/.test(ua)) return "mac";
  if (/Win/i.test(platform) || /Windows/.test(ua)) return "windows";
  // Desktop Linux (Android is caught above; Chromebooks say CrOS).
  if (/Linux/i.test(platform) || (/Linux|X11/.test(ua) && !/CrOS/.test(ua))) return "linux";
  return "other";
}

/** Is any build for this platform published yet? */
export function isAvailable(p: Platform): boolean {
  switch (p) {
    case "mac":
      return !!(LINKS.macAppStore || LINKS.macFile);
    case "ios":
      return !!LINKS.iosAppStore;
    case "windows":
      return !!(LINKS.windowsStore || LINKS.windowsFile);
    case "android":
      return !!LINKS.android;
    case "linux":
      return !!(LINKS.linuxFile || LINKS.linuxDeb);
    default:
      return false;
  }
}
