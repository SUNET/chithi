/**
 * Tests for the time format user setting (#57).
 *
 * Verifies the ui store's timeFormat / hour12 contract:
 * - "auto" resolves to undefined (Intl uses the locale's default)
 * - "12" resolves to true (force AM/PM)
 * - "24" resolves to false (force 24-hour)
 * - invalid values are rejected and the stored value is unchanged
 * - the choice persists to localStorage so reloads restore it
 */
import { beforeEach, describe, expect, it } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { useUiStore } from "@/stores/ui";

describe("ui store: timeFormat", () => {
  beforeEach(() => {
    localStorage.clear();
    setActivePinia(createPinia());
  });

  it("defaults to 'auto' and hour12 is undefined", () => {
    const ui = useUiStore();
    expect(ui.timeFormat).toBe("auto");
    expect(ui.hour12).toBeUndefined();
  });

  it("'12' resolves hour12 to true and persists", () => {
    const ui = useUiStore();
    ui.setTimeFormat("12");
    expect(ui.timeFormat).toBe("12");
    expect(ui.hour12).toBe(true);
    expect(localStorage.getItem("chithi-time-format")).toBe("12");
  });

  it("'24' resolves hour12 to false and persists", () => {
    const ui = useUiStore();
    ui.setTimeFormat("24");
    expect(ui.timeFormat).toBe("24");
    expect(ui.hour12).toBe(false);
    expect(localStorage.getItem("chithi-time-format")).toBe("24");
  });

  it("rejects invalid values", () => {
    const ui = useUiStore();
    ui.setTimeFormat("24");
    // @ts-expect-error — invalid on purpose
    ui.setTimeFormat("bogus");
    expect(ui.timeFormat).toBe("24");
    expect(localStorage.getItem("chithi-time-format")).toBe("24");
  });

  it("restores a previously persisted value on new store instance", () => {
    localStorage.setItem("chithi-time-format", "24");
    setActivePinia(createPinia());
    const ui = useUiStore();
    expect(ui.timeFormat).toBe("24");
    expect(ui.hour12).toBe(false);
  });

  it("ignores a corrupt persisted value and falls back to auto", () => {
    localStorage.setItem("chithi-time-format", "bogus");
    setActivePinia(createPinia());
    const ui = useUiStore();
    expect(ui.timeFormat).toBe("auto");
    expect(ui.hour12).toBeUndefined();
  });
});
