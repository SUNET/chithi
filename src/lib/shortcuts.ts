export type ShortcutDef = {
  /** Letter case is ignored when matching single-character keys. */
  key: string;
  /** Ctrl on Linux/Windows and Command on macOS. */
  ctrl?: boolean;
  shift?: boolean;
  alt?: boolean;
};

export type ShortcutBinding = ShortcutDef & {
  handler: (event: KeyboardEvent) => void;
};

function detectIsMac(): boolean {
  if (typeof navigator === "undefined") return false;
  const navigatorInfo = navigator as Navigator & {
    userAgentData?: { platform?: string };
  };
  const platform =
    navigatorInfo.userAgentData?.platform ?? navigatorInfo.platform ?? "";
  return /mac|darwin|iphone|ipad|ipod/i.test(platform);
}

export function formatShortcut(shortcut: ShortcutDef): string {
  const mac = detectIsMac();
  const parts: string[] = [];
  if (mac) {
    if (shortcut.ctrl) parts.push("⌘");
    if (shortcut.shift) parts.push("⇧");
    if (shortcut.alt) parts.push("⌥");
    parts.push(formatKey(shortcut.key, mac));
    return parts.join("");
  }
  if (shortcut.ctrl) parts.push("Ctrl");
  if (shortcut.alt) parts.push("Alt");
  if (shortcut.shift) parts.push("Shift");
  parts.push(formatKey(shortcut.key, mac));
  return parts.join("+");
}

function formatKey(key: string, mac: boolean): string {
  if (key.length === 1) return key.toUpperCase();
  if (key === "Enter" || key === "Return") return mac ? "↩" : "Enter";
  if (key === "Escape") return "Esc";
  return key;
}

function matchesShortcutOnPlatform(
  event: KeyboardEvent,
  shortcut: ShortcutDef,
  mac: boolean,
): boolean {
  const primaryDown = mac ? event.metaKey : event.ctrlKey;
  if (Boolean(shortcut.ctrl) !== primaryDown) return false;
  if (Boolean(shortcut.shift) !== event.shiftKey) return false;
  if (Boolean(shortcut.alt) !== event.altKey) return false;

  if (shortcut.key.length === 1) {
    return event.key.toLowerCase() === shortcut.key.toLowerCase();
  }
  return event.key === shortcut.key;
}

export function matchesShortcut(
  event: KeyboardEvent,
  shortcut: ShortcutDef,
): boolean {
  return matchesShortcutOnPlatform(event, shortcut, detectIsMac());
}

/** Dispatches only the first match; unmatched events retain browser defaults. */
export function dispatch(
  event: KeyboardEvent,
  bindings: readonly ShortcutBinding[],
): boolean {
  const mac = detectIsMac();
  for (const binding of bindings) {
    if (!matchesShortcutOnPlatform(event, binding, mac)) continue;
    event.preventDefault();
    binding.handler(event);
    return true;
  }
  return false;
}
