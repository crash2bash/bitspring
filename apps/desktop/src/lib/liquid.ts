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

type Opts = unknown | { key: unknown; part?: string };

const LEAD_MS = 320;
const TRAIL_MS = 520;

export function liquidPill(node: HTMLElement, opts: Opts) {
  let part = partOf(opts);
  const pill = document.createElement("span");
  pill.className = "liquid-pill";
  pill.setAttribute("aria-hidden", "true");
  node.appendChild(pill);

  let last: { left: number; right: number } | null = null;

  function place(animate: boolean) {
    const sel = node.querySelector<HTMLElement>(".sel, [aria-checked='true'], [aria-current='page']");
    if (!sel || sel.offsetParent === null) {
      pill.style.opacity = "0";
      last = null;
      return;
    }
    const target = (part && sel.querySelector<HTMLElement>(part)) || sel;
    const box = node.getBoundingClientRect();
    const r = target.getBoundingClientRect();
    const left = r.left - box.left - node.clientLeft;
    const right = box.width - node.clientLeft * 2 - (left + r.width);
    const top = r.top - box.top - node.clientTop;

    if (animate && last) {
      // Moving right, the right edge leads; moving left, the left edge does.
      const toRight = left > last.left;
      const lead = `${LEAD_MS}ms var(--spring-bouncy)`;
      const trail = `${TRAIL_MS}ms var(--spring-bouncy) 40ms`;
      pill.style.transition = toRight
        ? `left ${trail}, right ${lead}, top 200ms ease, height 200ms ease, opacity 200ms ease`
        : `left ${lead}, right ${trail}, top 200ms ease, height 200ms ease, opacity 200ms ease`;
    } else {
      pill.style.transition = "none";
    }
    pill.style.left = `${left}px`;
    pill.style.right = `${right}px`;
    pill.style.top = `${top}px`;
    pill.style.height = `${r.height}px`;
    pill.style.opacity = "1";
    last = { left, right };
  }

  // after the choice's class has changed in the DOM
  const later = (animate: boolean) => requestAnimationFrame(() => place(animate));
  later(false);
  // A resize (a label turning bold, the window narrowing) must not cut a move short: it just
  // retargets the pill, animated once it has a place.
  const ro = new ResizeObserver(() => place(last !== null));
  ro.observe(node);
  // The choice changes by a class or ARIA attribute flipping on a child: follow that directly,
  // whatever order the framework applies its updates in.
  const mo = new MutationObserver((records) => {
    if (records.some((r) => r.target !== pill)) later(true);
  });
  mo.observe(node, { subtree: true, attributes: true, attributeFilter: ["class", "aria-checked", "aria-current"] });
  // the web fonts change label widths once they load
  document.fonts?.ready.then(() => place(false));

  return {
    update(next: Opts) {
      part = partOf(next);
    },
    destroy() {
      mo.disconnect();
      ro.disconnect();
      pill.remove();
    },
  };
}

function partOf(opts: Opts): string | undefined {
  return opts && typeof opts === "object" && "part" in opts ? (opts as { part?: string }).part : undefined;
}
