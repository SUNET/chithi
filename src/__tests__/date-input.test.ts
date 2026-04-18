/**
 * Component tests for DateInput — the theme-aware replacement for the
 * native <input type="date"> (see #57 PR).
 *
 * Covers: open/close, selecting a day emits the right YYYY-MM-DD,
 * `min` disables earlier days, click-outside dismisses, Escape dismisses.
 */
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import DateInput from "@/components/common/DateInput.vue";

// jsdom's default is enough; we just need DOM + event plumbing.

async function mountWith(modelValue: string, min?: string) {
  // Attach to the real document so document-level listeners (click-outside,
  // keydown) can fire on dispatched events.
  const wrapper = mount(DateInput, {
    attachTo: document.body,
    props: {
      modelValue,
      min,
      "onUpdate:modelValue": (v: string) => wrapper.setProps({ modelValue: v }),
    },
  });
  return wrapper;
}

describe("DateInput", () => {
  beforeEach(() => {
    localStorage.clear();
    setActivePinia(createPinia());
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("starts closed and opens on trigger click", async () => {
    const w = await mountWith("2026-04-15");
    expect(w.find('[role="dialog"]').exists()).toBe(false);
    await w.find("button.date-input-trigger").trigger("click");
    expect(w.find('[role="dialog"]').exists()).toBe(true);
  });

  it("emits the clicked day as YYYY-MM-DD and closes", async () => {
    const w = await mountWith("2026-04-15");
    await w.find("button.date-input-trigger").trigger("click");

    const day20 = w.find('[data-testid="date-picker-day-2026-04-20"]');
    expect(day20.exists()).toBe(true);
    await day20.trigger("click");

    const emits = w.emitted("update:modelValue") ?? [];
    expect(emits[emits.length - 1]).toEqual(["2026-04-20"]);
    expect(w.find('[role="dialog"]').exists()).toBe(false);
  });

  it("disables days before `min` and refuses to emit", async () => {
    const w = await mountWith("2026-04-15", "2026-04-15");
    await w.find("button.date-input-trigger").trigger("click");

    const day10 = w.find('[data-testid="date-picker-day-2026-04-10"]');
    expect(day10.attributes("disabled")).toBeDefined();
    expect(day10.classes()).toContain("disabled");

    await day10.trigger("click");
    expect(w.emitted("update:modelValue") ?? []).toEqual([]);
    expect(w.find('[role="dialog"]').exists()).toBe(true);
  });

  it("closes on Escape", async () => {
    const w = await mountWith("2026-04-15");
    await w.find("button.date-input-trigger").trigger("click");
    expect(w.find('[role="dialog"]').exists()).toBe(true);

    const esc = new KeyboardEvent("keydown", { key: "Escape" });
    document.dispatchEvent(esc);
    await w.vm.$nextTick();
    expect(w.find('[role="dialog"]').exists()).toBe(false);
  });

  it("closes on mousedown outside the component", async () => {
    const outside = document.createElement("div");
    document.body.appendChild(outside);

    const w = await mountWith("2026-04-15");
    await w.find("button.date-input-trigger").trigger("click");
    expect(w.find('[role="dialog"]').exists()).toBe(true);

    const evt = new MouseEvent("mousedown", { bubbles: true });
    outside.dispatchEvent(evt);
    await w.vm.$nextTick();
    expect(w.find('[role="dialog"]').exists()).toBe(false);

    outside.remove();
  });

  it("does NOT close when mousedown lands on the popup itself", async () => {
    const w = await mountWith("2026-04-15");
    await w.find("button.date-input-trigger").trigger("click");
    const popup = w.find('[role="dialog"]').element;

    const evt = new MouseEvent("mousedown", { bubbles: true });
    popup.dispatchEvent(evt);
    await w.vm.$nextTick();
    expect(w.find('[role="dialog"]').exists()).toBe(true);
  });

  it("prev/next month navigation updates the visible grid", async () => {
    const w = await mountWith("2026-04-15");
    await w.find("button.date-input-trigger").trigger("click");
    // April is visible — pick a day unique to April.
    expect(
      w.find('[data-testid="date-picker-day-2026-04-20"]').exists(),
    ).toBe(true);

    await w.find('[data-testid="date-picker-next"]').trigger("click");
    // Should now show May.
    expect(
      w.find('[data-testid="date-picker-day-2026-05-20"]').exists(),
    ).toBe(true);

    await w.find('[data-testid="date-picker-prev"]').trigger("click");
    await w.find('[data-testid="date-picker-prev"]').trigger("click");
    // Should now show March.
    expect(
      w.find('[data-testid="date-picker-day-2026-03-20"]').exists(),
    ).toBe(true);
  });
});
