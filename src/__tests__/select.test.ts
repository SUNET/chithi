/**
 * Component tests for Select — the theme-aware replacement for the
 * native <select> whose expanded popup is browser-chrome and ignores
 * our CSS tokens on WebKitGTK.
 *
 * Covers: open/close, selecting an option emits the value, disabled
 * options are skipped, Escape dismisses, mousedown outside dismisses,
 * keyboard arrow navigation + Enter to commit.
 */
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import Select from "@/components/common/Select.vue";

const OPTIONS = [
  { value: "a", label: "Alpha" },
  { value: "b", label: "Bravo" },
  { value: "c", label: "Charlie", disabled: true },
  { value: "d", label: "Delta" },
];

async function mountWith(modelValue: string | null = "a") {
  const wrapper = mount(Select, {
    attachTo: document.body,
    props: {
      modelValue,
      options: OPTIONS,
      "onUpdate:modelValue": (v: string) => wrapper.setProps({ modelValue: v }),
    },
  });
  return wrapper;
}

describe("Select", () => {
  beforeEach(() => {
    localStorage.clear();
    setActivePinia(createPinia());
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("renders the selected option's label on the trigger", async () => {
    const w = await mountWith("b");
    expect(w.find(".select-label").text()).toBe("Bravo");
  });

  it("opens the listbox on trigger click, closes on second click", async () => {
    const w = await mountWith("a");
    expect(w.find('[role="listbox"]').exists()).toBe(false);
    await w.find(".select-trigger").trigger("click");
    expect(w.find('[role="listbox"]').exists()).toBe(true);
    await w.find(".select-trigger").trigger("click");
    expect(w.find('[role="listbox"]').exists()).toBe(false);
  });

  it("emits the chosen value and closes on option mousedown", async () => {
    const w = await mountWith("a");
    await w.find(".select-trigger").trigger("click");
    const options = w.findAll(".select-option");
    await options[1].trigger("mousedown");
    expect(w.emitted("update:modelValue")?.[0]).toEqual(["b"]);
    expect(w.find('[role="listbox"]').exists()).toBe(false);
  });

  it("does not emit for disabled options", async () => {
    const w = await mountWith("a");
    await w.find(".select-trigger").trigger("click");
    const options = w.findAll(".select-option");
    await options[2].trigger("mousedown"); // Charlie, disabled
    expect(w.emitted("update:modelValue")).toBeUndefined();
    expect(w.find('[role="listbox"]').exists()).toBe(true);
  });

  it("ArrowDown skips disabled options and Enter commits", async () => {
    const w = await mountWith("a"); // highlight starts at Alpha (0)
    await w.find(".select-trigger").trigger("click");
    // 0 (a) → 1 (b)
    await w.find(".select-trigger").trigger("keydown", { key: "ArrowDown" });
    // 1 (b) → skip 2 (c, disabled) → 3 (d)
    await w.find(".select-trigger").trigger("keydown", { key: "ArrowDown" });
    await w.find(".select-trigger").trigger("keydown", { key: "Enter" });
    expect(w.emitted("update:modelValue")?.[0]).toEqual(["d"]);
  });

  it("closes on Escape", async () => {
    const w = await mountWith("a");
    await w.find(".select-trigger").trigger("click");
    await w.find(".select-trigger").trigger("keydown", { key: "Escape" });
    expect(w.find('[role="listbox"]').exists()).toBe(false);
  });

  it("closes on mousedown outside the component", async () => {
    const outside = document.createElement("div");
    document.body.appendChild(outside);

    const w = await mountWith("a");
    await w.find(".select-trigger").trigger("click");
    expect(w.find('[role="listbox"]').exists()).toBe(true);

    const evt = new MouseEvent("mousedown", { bubbles: true });
    outside.dispatchEvent(evt);
    await w.vm.$nextTick();
    expect(w.find('[role="listbox"]').exists()).toBe(false);

    outside.remove();
  });

  it("falls back to the placeholder when no option matches", async () => {
    const w = mount(Select, {
      props: {
        modelValue: "zz",
        options: OPTIONS,
        placeholder: "Pick one…",
      },
    });
    expect(w.find(".select-label").text()).toBe("Pick one…");
    expect(w.find(".select-label").classes()).toContain("is-placeholder");
  });
});
