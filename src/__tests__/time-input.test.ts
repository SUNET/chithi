/**
 * Component tests for TimeInput — the custom time-field replacement used
 * where WebKitGTK's native <input type="time"> won't honor our time-format
 * setting (see #57).
 *
 * Covers: canonical emission across 12h / 24h / auto, flexible parsing,
 * min enforcement, and the "revert to last good value on invalid blur"
 * contract the component header promises.
 */
import { beforeEach, describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import TimeInput from "@/components/common/TimeInput.vue";
import { useUiStore } from "@/stores/ui";

async function mountWith(modelValue = "09:30", min?: string) {
  const wrapper = mount(TimeInput, {
    props: {
      modelValue,
      min,
      "onUpdate:modelValue": (v: string) => wrapper.setProps({ modelValue: v }),
    },
  });
  return wrapper;
}

function lastEmit(w: ReturnType<typeof mount>, event: string): unknown[] | undefined {
  const list = w.emitted(event);
  return list && list.length > 0 ? list[list.length - 1] : undefined;
}

describe("TimeInput", () => {
  beforeEach(() => {
    localStorage.clear();
    setActivePinia(createPinia());
  });

  it("emits canonical 24h HH:MM when user types 24h format", async () => {
    const ui = useUiStore();
    ui.setTimeFormat("24");
    const w = await mountWith("09:30");
    const input = w.find("input").element as HTMLInputElement;
    input.value = "14:45";
    await w.find("input").trigger("input");
    await w.find("input").trigger("blur");
    expect(lastEmit(w, "update:modelValue")).toEqual(["14:45"]);
  });

  it("accepts 12h AM/PM input and normalizes to 24h", async () => {
    const ui = useUiStore();
    ui.setTimeFormat("12");
    const w = await mountWith("09:30");
    const input = w.find("input").element as HTMLInputElement;
    input.value = "2:30 PM";
    await w.find("input").trigger("input");
    await w.find("input").trigger("blur");
    expect(lastEmit(w, "update:modelValue")).toEqual(["14:30"]);
  });

  it("accepts compact formats like '13' and '2 pm'", async () => {
    const w = await mountWith("00:00");
    const input = w.find("input").element as HTMLInputElement;

    input.value = "13";
    await w.find("input").trigger("input");
    await w.find("input").trigger("blur");
    expect(lastEmit(w, "update:modelValue")).toEqual(["13:00"]);

    input.value = "2 pm";
    await w.find("input").trigger("input");
    await w.find("input").trigger("blur");
    expect(lastEmit(w, "update:modelValue")).toEqual(["14:00"]);
  });

  it("rejects invalid input, marks the field invalid, and restores the last good display value", async () => {
    const ui = useUiStore();
    ui.setTimeFormat("24");
    const w = await mountWith("09:30");
    const inputEl = w.find("input").element as HTMLInputElement;

    inputEl.value = "not a time";
    await w.find("input").trigger("input");
    await w.find("input").trigger("blur");

    // No emission
    expect(w.emitted("update:modelValue") ?? []).toEqual([]);
    // invalid flag reflected in the DOM
    expect(w.find("input").classes()).toContain("invalid");
    // display restored to the last good value, not left as "not a time"
    expect(inputEl.value).toBe("09:30");
  });

  it("rejects values below `min`", async () => {
    const ui = useUiStore();
    ui.setTimeFormat("24");
    const w = await mountWith("10:00", "09:00");
    const inputEl = w.find("input").element as HTMLInputElement;

    inputEl.value = "08:30";
    await w.find("input").trigger("input");
    await w.find("input").trigger("blur");

    expect(w.emitted("update:modelValue") ?? []).toEqual([]);
    expect(w.find("input").classes()).toContain("invalid");
    expect(inputEl.value).toBe("10:00");
  });

  it("treats empty input as no-op (no emission, invalid cleared)", async () => {
    const ui = useUiStore();
    ui.setTimeFormat("24");
    const w = await mountWith("10:00");
    const inputEl = w.find("input").element as HTMLInputElement;

    inputEl.value = "  ";
    await w.find("input").trigger("input");
    await w.find("input").trigger("blur");

    expect(w.emitted("update:modelValue") ?? []).toEqual([]);
    expect(w.find("input").classes()).not.toContain("invalid");
    expect(inputEl.value).toBe("10:00");
  });
});
