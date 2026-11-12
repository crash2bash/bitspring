// The app's look: the classic anodised panel, or glass (translucent layers over an ambient
// backdrop made from the playing album's cover). Kept per machine, like the language.
import { isIOS, isMac } from "./api";

export const LOOKS = ["classic", "glass"] as const;
export type Look = (typeof LOOKS)[number];

const KEY = "look";
/** Glass on Apple's systems, whose own interface is glass; the anodised panel elsewhere.
    Only until the person picks one in Settings. */
const DEFAULT: Look = isMac || isIOS ? "glass" : "classic";

function loadLook(): Look {
  try {
    const v = JSON.parse(localStorage.getItem(KEY) ?? "null");
    return LOOKS.includes(v) ? v : DEFAULT;
  } catch {
    return DEFAULT;
  }
}

/** The CSS keys everything off `<html data-look="…">`, so the switch is one attribute. */
function apply(v: Look) {
  if (typeof document !== "undefined") document.documentElement.dataset.look = v;
}

const state = $state({ look: typeof localStorage !== "undefined" ? loadLook() : DEFAULT });
apply(state.look);

export const look = {
  get value(): Look {
    return state.look;
  },
  set value(v: Look) {
    state.look = v;
    apply(v);
    try {
      localStorage.setItem(KEY, JSON.stringify(v));
    } catch {}
  },
};
