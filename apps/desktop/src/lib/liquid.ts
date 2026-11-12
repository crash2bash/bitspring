// The glass look's selection pill: one shape that flows from the old choice to the new one
// instead of a background that jumps. While it travels it stretches like a drop: the edge in
// front moves first and fast, the edge behind follows a little later, and both settle on a
// spring. Used on a row of choices that marks the chosen one with `.sel` or aria-checked.
//
//   <nav use:liquidPill={screen}> … <button class:sel={…}> … </nav>
//   <nav use:liquidPill={{ key: screen, part: "svg" }}>   (the pill sits behind the icon only)
//
// The pill is drawn only by glass.css; in the classic look it is display: none. The parameter
// only names what changes (and, with `part`, which part of the chosen button to sit behind):
// the pill follows the `.sel` / aria change itself.
//
// Smoothness: the pill is laid out once at its destination and the journey is a transform
// animation (Web Animations), which the compositor plays on its own. Animating left/right
// instead re-laid out the row and repainted the blurred backdrop under it on every frame, and
// stuttered whenever the page was busy (a meter update, a screen being built).

type Opts = unknown | { key: unknown; part?: string };
type Box = { l: number; r: number; t: number; b: number };

const FRAME = 1000 / 60;
// the edge in front: quick and a little lively; the edge behind: slower, a beat later
const LEAD = { w: 19, z: 0.62, delay: 0 };
const TRAIL = { w: 13, z: 0.68, delay: 40 };
const MAX_MS = 900;

const reducedMotion = () => matchMedia("(prefers-reduced-motion: reduce)").matches;

/** Damped spring from `from` to `to`, t in ms. */
function spring(from: number, to: number, t: number, s: { w: number; z: number; delay: number }) {
  const ms = t - s.delay;
  if (ms <= 0) return from;
  const sec = ms / 1000;
  const wd = s.w * Math.sqrt(1 - s.z * s.z);
  const decay = Math.exp(-s.z * s.w * sec);
  return to + (from - to) * decay * (Math.cos(wd * sec) + ((s.z * s.w) / wd) * Math.sin(wd * sec));
}

export function liquidPill(node: HTMLElement, opts: Opts) {
  let part = partOf(opts);
  const pill = document.createElement("span");
  pill.className = "liquid-pill";
  pill.setAttribute("aria-hidden", "true");
  node.classList.add("has-liquid-pill");
  node.appendChild(pill);

  let at: Box | null = null; // where the pill is laid out (its destination)
  let anim: Animation | null = null;
  let frames: Box[] = [];

  /** The chosen option's box in the row's own (untransformed) pixels. */
  function measure(): Box | null {
    const sel = node.querySelector<HTMLElement>(".sel, [aria-checked='true'], [aria-current='page']");
    if (!sel || sel.offsetParent === null || node.offsetWidth === 0) return null;
    const target = (part && sel.querySelector<Element>(part)) || sel;
    const box = node.getBoundingClientRect();
    const r = target.getBoundingClientRect();
    // A row inside a panel that is still growing open (glass-in scales it) measures small:
    // divide the scale back out, or the pill lands short of its button.
    const sx = box.width / node.offsetWidth || 1;
    const sy = box.height / node.offsetHeight || 1;
    const l = (r.left - box.left) / sx - node.clientLeft;
    const t = (r.top - box.top) / sy - node.clientTop;
    return { l, r: l + r.width / sx, t, b: t + r.height / sy };
  }

  /** Where the pill shows right now, part way through a move or at rest. */
  function shown(): Box | null {
    if (anim && anim.playState === "running" && frames.length) {
      const i = Math.min(frames.length - 1, Math.max(0, Math.round(Number(anim.currentTime ?? 0) / FRAME)));
      return frames[i];
    }
    return at;
  }

  function layOut(b: Box) {
    pill.style.left = `${b.l}px`;
    pill.style.top = `${b.t}px`;
    pill.style.width = `${b.r - b.l}px`;
    pill.style.height = `${b.b - b.t}px`;
    at = b;
  }

  function place(animate: boolean) {
    const to = measure();
    if (!to) {
      pill.style.opacity = "0";
      anim?.cancel();
      anim = null;
      at = null;
      return;
    }
    pill.style.opacity = "1";
    const from = shown();
    const same = at && Math.abs(at.l - to.l) < 0.5 && Math.abs(at.r - to.r) < 0.5 && Math.abs(at.t - to.t) < 0.5 && Math.abs(at.b - to.b) < 0.5;
    if (same) return; // already there or already on its way there
    anim?.cancel();
    anim = null;
    layOut(to);
    if (!animate || !from || reducedMotion()) return;

    // Sample the two edges' springs into keyframes; each maps the laid-out box onto the
    // in-between box with translate + scale from its top-left corner.
    const right = to.l > from.l;
    const w = to.r - to.l;
    const h = to.b - to.t;
    frames = [];
    for (let t = 0; t <= MAX_MS; t += FRAME) {
      const l = spring(from.l, to.l, t, right ? TRAIL : LEAD);
      const r = spring(from.r, to.r, t, right ? LEAD : TRAIL);
      const top = spring(from.t, to.t, t, LEAD);
      const bot = spring(from.b, to.b, t, LEAD);
      frames.push({ l, r: Math.max(r, l + w * 0.3), t: top, b: Math.max(bot, top + h * 0.5) });
      const settled = [l - to.l, r - to.r, top - to.t, bot - to.b].every((d) => Math.abs(d) < 0.3);
      if (settled && t > TRAIL.delay + 120) break;
    }
    frames.push(to);
    const keyframes = frames.map((f) => ({
      transform: `translate(${f.l - to.l}px, ${f.t - to.t}px) scale(${(f.r - f.l) / w}, ${(f.b - f.t) / h})`,
    }));
    anim = pill.animate(keyframes, { duration: (frames.length - 1) * FRAME, easing: "linear" });
    anim.onfinish = () => (anim = null);
  }

  // after the choice's class has changed in the DOM
  let queued = 0;
  const later = (animate: boolean) => {
    cancelAnimationFrame(queued);
    queued = requestAnimationFrame(() => place(animate));
  };
  later(false);
  // A resize (a label turning bold, the window narrowing) retargets the pill, animated once
  // it has a place.
  const ro = new ResizeObserver(() => place(at !== null));
  ro.observe(node);
  // The choice changes by a class or ARIA attribute flipping on a child: follow that directly,
  // whatever order the framework applies its updates in.
  const mo = new MutationObserver((records) => {
    if (records.some((r) => r.target !== pill && r.target !== node)) later(true);
  });
  mo.observe(node, { subtree: true, attributes: true, attributeFilter: ["class", "aria-checked", "aria-current"] });
  // the web fonts change label widths once they load
  document.fonts?.ready.then(() => place(false));

  return {
    update(next: Opts) {
      part = partOf(next);
    },
    destroy() {
      cancelAnimationFrame(queued);
      mo.disconnect();
      ro.disconnect();
      anim?.cancel();
      pill.remove();
      node.classList.remove("has-liquid-pill");
    },
  };
}

function partOf(opts: Opts): string | undefined {
  return opts && typeof opts === "object" && "part" in opts ? (opts as { part?: string }).part : undefined;
}
