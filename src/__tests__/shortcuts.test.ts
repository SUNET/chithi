import { afterEach, describe, expect, it, vi } from "vitest";
import {
  __setPlatformForTests,
  dispatch,
  formatShortcut,
  matchesShortcut,
} from "@/lib/shortcuts";

afterEach(() => __setPlatformForTests(null));

function ev(
  key: string,
  mods: { ctrl?: boolean; meta?: boolean; shift?: boolean; alt?: boolean } = {},
): KeyboardEvent {
  return new KeyboardEvent("keydown", {
    key,
    ctrlKey: !!mods.ctrl,
    metaKey: !!mods.meta,
    shiftKey: !!mods.shift,
    altKey: !!mods.alt,
    cancelable: true,
  });
}

describe("formatShortcut", () => {
  it("renders Ctrl+X on non-mac", () => {
    __setPlatformForTests(false);
    expect(formatShortcut({ key: "s", ctrl: true })).toBe("Ctrl+S");
  });

  it("renders ⌘S on mac", () => {
    __setPlatformForTests(true);
    expect(formatShortcut({ key: "s", ctrl: true })).toBe("⌘S");
  });

  it("includes Shift segment", () => {
    __setPlatformForTests(false);
    expect(formatShortcut({ key: "z", ctrl: true, shift: true })).toBe(
      "Ctrl+Shift+Z",
    );
  });

  it("normalises Enter on macOS to ↩", () => {
    __setPlatformForTests(true);
    expect(formatShortcut({ key: "Enter", ctrl: true })).toBe("⌘↩");
  });
});

describe("matchesShortcut", () => {
  it("matches case-insensitively on letter keys", () => {
    __setPlatformForTests(false);
    expect(matchesShortcut(ev("S", { ctrl: true }), { key: "s", ctrl: true }))
      .toBe(true);
    expect(matchesShortcut(ev("s", { ctrl: true }), { key: "s", ctrl: true }))
      .toBe(true);
  });

  it("requires the platform-correct primary modifier", () => {
    __setPlatformForTests(true);
    // Ctrl alone on macOS does not satisfy the abstract `ctrl: true`.
    expect(matchesShortcut(ev("s", { ctrl: true }), { key: "s", ctrl: true }))
      .toBe(false);
    // ⌘ does.
    expect(matchesShortcut(ev("s", { meta: true }), { key: "s", ctrl: true }))
      .toBe(true);
  });

  it("requires modifier flags to match exactly (no extras allowed)", () => {
    __setPlatformForTests(false);
    expect(
      matchesShortcut(ev("s", { ctrl: true, shift: true }), {
        key: "s",
        ctrl: true,
      }),
    ).toBe(false);
  });

  it("matches non-letter keys exactly", () => {
    __setPlatformForTests(false);
    expect(matchesShortcut(ev(",", { ctrl: true }), { key: ",", ctrl: true }))
      .toBe(true);
    expect(matchesShortcut(ev("Enter", { ctrl: true }), {
      key: "Enter",
      ctrl: true,
    })).toBe(true);
  });
});

describe("dispatch", () => {
  it("calls the matching handler and prevents default", () => {
    __setPlatformForTests(false);
    const handler = vi.fn();
    const event = ev("w", { ctrl: true });
    const preventDefault = vi.spyOn(event, "preventDefault");
    const matched = dispatch(event, [{ key: "w", ctrl: true, handler }]);
    expect(matched).toBe(true);
    expect(handler).toHaveBeenCalledOnce();
    expect(preventDefault).toHaveBeenCalled();
  });

  it("returns false and does not preventDefault for unmatched events", () => {
    __setPlatformForTests(false);
    const handler = vi.fn();
    const event = ev("a");
    const preventDefault = vi.spyOn(event, "preventDefault");
    const matched = dispatch(event, [{ key: "w", ctrl: true, handler }]);
    expect(matched).toBe(false);
    expect(handler).not.toHaveBeenCalled();
    expect(preventDefault).not.toHaveBeenCalled();
  });

  it("first matching binding wins", () => {
    __setPlatformForTests(false);
    const first = vi.fn();
    const second = vi.fn();
    dispatch(ev("s", { ctrl: true }), [
      { key: "s", ctrl: true, handler: first },
      { key: "s", ctrl: true, handler: second },
    ]);
    expect(first).toHaveBeenCalledOnce();
    expect(second).not.toHaveBeenCalled();
  });
});
