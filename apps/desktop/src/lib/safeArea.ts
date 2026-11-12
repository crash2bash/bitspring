// Safe-area insets as CSS variables (--safe-top/right/bottom/left) that stay right when the phone
// turns. The styles read them with env(safe-area-inset-*) as the fallback.
//
// On iPhone both the web view's env(safe-area-inset-*) and UIKit's own window insets keep the
// upright phone's values whatever the orientation (seen on iOS 27): sideways they still say
// "62 on top, 34 at the bottom, nothing at the sides" while the Dynamic Island sits at a side.
// So when the window is wider than tall but the insets are still the upright ones, the island's
// depth goes to both sides (iOS keeps landscape symmetric), the top keeps room for the status bar
// (iOS 27 shows it sideways too) and the bottom only for the home indicator. Elsewhere env() is right and the variables aren't set.

import { inTauri, isIOS } from "./api";

export interface Insets {
  top: number;
  right: number;
  bottom: number;
  left: number;
}

/** Home indicator height in landscape, in points. */
const LANDSCAPE_BOTTOM = 21;
/** The status bar sideways: its clock reaches about 41 pt down (iOS 27, iPhone 18 Pro Max). */
const LANDSCAPE_TOP = 44;

/** The insets that fit the window as it is now. */
export function fitInsets(reported: Insets, width: number, height: number): Insets {
  const sideways = width > height;
  const stuckUpright = reported.left === 0 && reported.right === 0 && reported.top > 0;
  if (!sideways || !stuckUpright) return reported;
  return { top: Math.min(reported.top, LANDSCAPE_TOP), right: reported.top, bottom: Math.min(reported.bottom, LANDSCAPE_BOTTOM), left: reported.top };
}

/** Start keeping --safe-* up to date (iPhone app only); returns a function that stops it. */
export function trackSafeArea(): () => void {
  if (!isIOS || !inTauri) return () => {};
  const probe = document.createElement("div");
  probe.setAttribute("aria-hidden", "true");
  probe.style.cssText =
    "position:fixed;visibility:hidden;pointer-events:none;top:0;left:0;" +
    "padding:env(safe-area-inset-top) env(safe-area-inset-right) env(safe-area-inset-bottom) env(safe-area-inset-left)";
  document.body.append(probe);
  const root = document.documentElement.style;
  const update = () => {
    const s = getComputedStyle(probe);
    const px = (v: string) => parseFloat(v) || 0;
    const fit = fitInsets(
      { top: px(s.paddingTop), right: px(s.paddingRight), bottom: px(s.paddingBottom), left: px(s.paddingLeft) },
      innerWidth,
      innerHeight,
    );
    for (const side of ["top", "right", "bottom", "left"] as const) root.setProperty(`--safe-${side}`, `${fit[side]}px`);
  };
  // The web view gets its insets a moment after the page starts, and after each turn: measure
  // now, then again shortly after.
  const timers: number[] = [];
  const refresh = () => {
    update();
    for (const ms of [150, 600, 1500]) timers.push(window.setTimeout(update, ms));
  };
  refresh();
  addEventListener("resize", refresh);
  return () => {
    removeEventListener("resize", refresh);
    timers.forEach(clearTimeout);
    probe.remove();
  };
}
