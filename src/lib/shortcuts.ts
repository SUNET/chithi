/**
 * Keyboard shortcut helpers.
 *
 * One platform-modifier abstraction is used everywhere so menu labels and
 * keydown handlers cannot drift. Platform detection runs once per module
 * load; tests can override via `__setPlatformForTests`.
 *
 * Shortcut "key" matching is case-insensitive on letters, exact on
 * everything else (e.g. `Enter`, `,`, `\`). Modifier flags default to false
 * so a definition only lists what it cares about.
 */

export type ShortcutDef = {
  /** Single-character or named key. Letter case is ignored on match. */
  key: string;
  /** Platform-resolved primary modifier (Ctrl on Linux/Win, Cmd on macOS). */
  ctrl?: boolean;
  /** Always Shift. */
  shift?: boolean;
  /** Always Alt/Option. */
  alt?: boolean;
};

let cachedIsMac: boolean | null = null;

function detectIsMac(): boolean {
  if (typeof navigator === "undefined") return false;
  const platform = (navigator as { platform?: string; userAgentData?: { platform?: string } });
  const value =
    platform.userAgentData?.platform ?? platform.platform ?? "";
  return /mac|darwin|iphone|ipad|ipod/i.test(value);
}

export function isMac(): boolean {
  if (cachedIsMac === null) cachedIsMac = detectIsMac();
  return cachedIsMac;
}

/** Test seam for forcing platform in unit tests. */
export function __setPlatformForTests(mac: boolean | null): void {
  cachedIsMac = mac;
}

/** Render a shortcut as the user-visible label, e.g. `Ctrl+S` or `⌘S`. */
export function formatShortcut(def: ShortcutDef): string {
  const parts: string[] = [];
  if (isMac()) {
    if (def.ctrl) parts.push("⌘");
    if (def.shift) parts.push("⇧");
    if (def.alt) parts.push("⌥");
    parts.push(formatKey(def.key));
    return parts.join("");
  }
  if (def.ctrl) parts.push("Ctrl");
  if (def.alt) parts.push("Alt");
  if (def.shift) parts.push("Shift");
  parts.push(formatKey(def.key));
  return parts.join("+");
}

function formatKey(key: string): string {
  if (key.length === 1) return key.toUpperCase();
  // Normalise a few common names so the label matches what users expect.
  if (key === "Enter" || key === "Return") return isMac() ? "↩" : "Enter";
  if (key === "Escape") return "Esc";
  return key;
}

/** Does this keyboard event match the shortcut definition? */
export function matchesShortcut(
  event: KeyboardEvent,
  def: ShortcutDef,
): boolean {
  const primaryDown = isMac() ? event.metaKey : event.ctrlKey;
  if (!!def.ctrl !== primaryDown) return false;
  if (!!def.shift !== event.shiftKey) return false;
  if (!!def.alt !== event.altKey) return false;

  const eventKey = event.key;
  if (def.key.length === 1) {
    return eventKey.toLowerCase() === def.key.toLowerCase();
  }
  return eventKey === def.key;
}

export type ShortcutBinding = ShortcutDef & {
  handler: (event: KeyboardEvent) => void;
};

/**
 * Build a keydown handler from a list of bindings. Returns `undefined` for
 * unmatched events so the browser default keeps working (text editing,
 * focus navigation, etc.). The first matching binding wins; later ones are
 * ignored.
 */
export function dispatch(
  event: KeyboardEvent,
  bindings: readonly ShortcutBinding[],
): boolean {
  for (const binding of bindings) {
    if (matchesShortcut(event, binding)) {
      event.preventDefault();
      binding.handler(event);
      return true;
    }
  }
  return false;
}
