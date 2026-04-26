import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";

const { setDecorationsMock } = vi.hoisted(() => ({
  setDecorationsMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ setDecorations: setDecorationsMock }),
}));

vi.mock("@/lib/tauri", () => ({
  listTimezones: vi.fn().mockResolvedValue(["UTC", "Europe/Stockholm", "America/New_York"]),
  getDefaultTimezone: vi.fn().mockResolvedValue("UTC"),
}));

import PreferencesView from "@/views/PreferencesView.vue";
import { useUiStore } from "@/stores/ui";

function makeRouter() {
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/", component: { template: "<div/>" } },
      { path: "/preferences", component: PreferencesView },
    ],
  });
}

beforeEach(() => {
  setActivePinia(createPinia());
  setDecorationsMock.mockReset();
  localStorage.clear();
});

afterEach(() => {
  localStorage.clear();
});

describe("PreferencesView", () => {
  it("defaults to the General section", () => {
    const wrapper = mount(PreferencesView, { global: { plugins: [makeRouter()] } });
    expect(wrapper.find('[data-testid="prefs-section-general"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="prefs-section-date-time"]').exists()).toBe(false);
  });

  it("nav swaps the visible section", async () => {
    const wrapper = mount(PreferencesView, { global: { plugins: [makeRouter()] } });
    await wrapper.find('[data-testid="prefs-nav-date-time"]').trigger("click");
    expect(wrapper.find('[data-testid="prefs-section-date-time"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="prefs-section-general"]').exists()).toBe(false);
  });

  it("does not expose Mail-only settings (threading, message pane)", () => {
    const wrapper = mount(PreferencesView, { global: { plugins: [makeRouter()] } });
    expect(wrapper.find('[data-testid="prefs-nav-mail"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="prefs-threaded"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="prefs-pane-tab"]').exists()).toBe(false);
  });

  it("Theme buttons drive uiStore.setTheme", async () => {
    const wrapper = mount(PreferencesView, { global: { plugins: [makeRouter()] } });
    const ui = useUiStore();
    await wrapper.find('[data-testid="prefs-theme-dark"]').trigger("click");
    expect(ui.theme).toBe("dark");
    expect(localStorage.getItem("chithi-theme")).toBe("dark");

    await wrapper.find('[data-testid="prefs-theme-system"]').trigger("click");
    expect(ui.theme).toBe("system");
  });

  it("Hide Title Bar toggle drives uiStore.setDecorations (inverted sense)", async () => {
    const wrapper = mount(PreferencesView, { global: { plugins: [makeRouter()] } });
    const ui = useUiStore();
    expect(ui.decorationsEnabled).toBe(true);
    await wrapper.find('[data-testid="prefs-hide-title-bar"]').setValue(true);
    expect(ui.decorationsEnabled).toBe(false);
    await wrapper.find('[data-testid="prefs-hide-title-bar"]').setValue(false);
    expect(ui.decorationsEnabled).toBe(true);
  });

  it("Date and Time section drives week start, time format, and timezone", async () => {
    const wrapper = mount(PreferencesView, { global: { plugins: [makeRouter()] } });
    const ui = useUiStore();
    ui.timezoneList = ["UTC", "Europe/Stockholm"];
    await wrapper.find('[data-testid="prefs-nav-date-time"]').trigger("click");

    await wrapper.find('[data-testid="prefs-week-start-1"]').trigger("click");
    expect(ui.weekStartDay).toBe(1);

    await wrapper.find('[data-testid="prefs-time-format-24"]').trigger("click");
    expect(ui.timeFormat).toBe("24");

    const input = wrapper.find('[data-testid="prefs-timezone-search"]');
    await input.trigger("focus");
    await wrapper.vm.$nextTick();
    const opt = wrapper.find('[data-testid="prefs-timezone-option-Europe/Stockholm"]');
    await opt.trigger("mousedown");
    expect(ui.displayTimezone).toBe("Europe/Stockholm");
  });

  it("close button calls router.back", async () => {
    const router = makeRouter();
    const back = vi.spyOn(router, "back");
    const wrapper = mount(PreferencesView, { global: { plugins: [router] } });
    await wrapper.find('[data-testid="prefs-close"]').trigger("click");
    expect(back).toHaveBeenCalled();
  });
});

describe("UI store theme system", () => {
  it("resolvedTheme follows the OS preference when theme === 'system'", () => {
    setActivePinia(createPinia());
    const matchMedia = vi.fn().mockReturnValue({
      matches: true,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("matchMedia", matchMedia);
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: matchMedia,
    });
    const ui = useUiStore();
    ui.setTheme("system");
    expect(ui.resolvedTheme).toBe("dark");
    vi.unstubAllGlobals();
  });

  it("validates a stale stored theme and falls back to 'system'", () => {
    localStorage.setItem("chithi-theme", "neon");
    setActivePinia(createPinia());
    const ui = useUiStore();
    expect(ui.theme).toBe("system");
  });

  it("resolvedTheme reacts when the OS preference flips while theme === 'system'", () => {
    setActivePinia(createPinia());
    let changeHandler: ((e: { matches: boolean }) => void) | null = null;
    const mql = {
      matches: false, // OS starts in light mode
      addEventListener: (_evt: string, h: (e: { matches: boolean }) => void) => {
        changeHandler = h;
      },
      removeEventListener: vi.fn(),
    };
    vi.stubGlobal("matchMedia", () => mql);
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: () => mql,
    });
    const ui = useUiStore();
    ui.setTheme("system");
    ui.initTheme();
    expect(ui.resolvedTheme).toBe("light");

    // Simulate the OS flipping to dark.
    expect(changeHandler).not.toBeNull();
    changeHandler!({ matches: true });
    expect(ui.resolvedTheme).toBe("dark");
    vi.unstubAllGlobals();
  });
});
