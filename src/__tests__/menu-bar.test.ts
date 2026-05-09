import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";
import { __setPlatformForTests } from "@/lib/shortcuts";
import MenuBar from "@/components/common/MenuBar.vue";
import pkg from "../../package.json";

// `__APP_VERSION__` is injected by Vite's `define` from package.json (see
// vite.config.ts). Asserting against package.json keeps the test in sync
// with whatever the build pipeline actually inlines.
const APP_VERSION = pkg.version as string;

const { invokeMock, setDecorationsMock, openUrlMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  setDecorationsMock: vi.fn(),
  openUrlMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ setDecorations: setDecorationsMock }),
}));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: openUrlMock }));
// The calendar store subscribes to the "calendar-changed" event at
// construction time; without this mock vue-router's first navigation
// blows up in jsdom because the underlying transformCallback isn't
// available.
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

vi.mock("@/lib/tauri", () => ({
  listTimezones: vi.fn().mockResolvedValue([]),
  getDefaultTimezone: vi.fn().mockResolvedValue("UTC"),
}));

// Named routes mirror the real router so MenuBar's `route.name`-driven
// View-menu context picks up "mail" / "calendar" / "contacts" the same
// way the production app does.
function makeRouter() {
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/", name: "mail", component: { template: "<div/>" } },
      { path: "/calendar", name: "calendar", component: { template: "<div/>" } },
      { path: "/contacts", name: "contacts", component: { template: "<div/>" } },
      { path: "/preferences", name: "preferences", component: { template: "<div/>" } },
    ],
  });
}

async function mountAt(path: string) {
  const router = makeRouter();
  await router.push(path);
  await router.isReady();
  const wrapper = mount(MenuBar, { global: { plugins: [router] } });
  return { wrapper, router };
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  setDecorationsMock.mockReset();
  openUrlMock.mockReset();
  document.body.innerHTML = "";
  __setPlatformForTests(false);
});

afterEach(() => {
  __setPlatformForTests(null);
});

describe("MenuBar", () => {
  it("renders File / View / Help menu labels on the mail route", async () => {
    const { wrapper } = await mountAt("/");
    expect(wrapper.text()).toContain("File");
    expect(wrapper.text()).toContain("View");
    expect(wrapper.text()).toContain("Help");
  });

  it("Help > About opens the About dialog", async () => {
    const { wrapper } = await mountAt("/");
    await wrapper.find('.menu-item:nth-of-type(3)').trigger("click");
    await wrapper.find('[data-testid="menu-help-about"]').trigger("click");
    await wrapper.vm.$nextTick();
    expect(document.querySelector('[data-testid="about-overlay"]')).not.toBeNull();
    expect(document.body.textContent).toContain(APP_VERSION);
  });

  it("File menu shows Preferences / Quit with shortcut labels", async () => {
    const wrapper = mount(MenuBar, { global: { plugins: [makeRouter()] } });
    await wrapper.find('.menu-item:nth-of-type(1)').trigger("click");
    const dropdown = wrapper.find('[data-testid="menu-file-dropdown"]');
    expect(dropdown.exists()).toBe(true);
    expect(dropdown.text()).toContain("Preferences");
    expect(dropdown.text()).toContain("Ctrl+,");
    expect(dropdown.text()).toContain("Quit");
    expect(dropdown.text()).toContain("Ctrl+Q");
    expect(dropdown.text()).not.toContain("Close Window");
  });

  it("File > Quit invokes the quit_app command", async () => {
    const wrapper = mount(MenuBar, { global: { plugins: [makeRouter()] } });
    await wrapper.find('.menu-item:nth-of-type(1)').trigger("click");
    await wrapper.find('[data-testid="menu-file-quit"]').trigger("click");
    expect(invokeMock).toHaveBeenCalledWith("quit_app");
  });

  it("File > Preferences routes to /preferences", async () => {
    const router = makeRouter();
    const push = vi.spyOn(router, "push");
    const wrapper = mount(MenuBar, { global: { plugins: [router] } });
    await wrapper.find('.menu-item:nth-of-type(1)').trigger("click");
    await wrapper.find('[data-testid="menu-file-preferences"]').trigger("click");
    expect(push).toHaveBeenCalledWith("/preferences");
  });

  it("View menu shows the four-way radio with None / Right / Bottom / Tabs", async () => {
    const { wrapper } = await mountAt("/");
    await wrapper.find('[data-testid="menu-view"]').trigger("click");
    const dropdown = wrapper.find('[data-testid="menu-view-dropdown"]');
    expect(dropdown.text()).toContain("Message Pane");
    expect(dropdown.text()).toContain("None");
    expect(dropdown.text()).toContain("Right");
    expect(dropdown.text()).toContain("Bottom");
    expect(dropdown.text()).toContain("Tabs");
    // The standalone "Show Message Pane" toggle is gone.
    expect(dropdown.text()).not.toContain("Show Message Pane");
    // Default messageViewMode is "right"; that row should carry the bullet.
    const right = wrapper.find('[data-testid="menu-view-position-right"]');
    expect(right.text()).toContain("\u25CF");
    const none = wrapper.find('[data-testid="menu-view-position-none"]');
    expect(none.text()).not.toContain("\u25CF");
  });

  it("Selecting None hides the pane via setMessageViewMode", async () => {
    const { useUiStore } = await import("@/stores/ui");
    const { wrapper } = await mountAt("/");
    await wrapper.find('[data-testid="menu-view"]').trigger("click");
    await wrapper.find('[data-testid="menu-view-position-none"]').trigger("click");
    const ui = useUiStore();
    expect(ui.messageViewMode).toBe("none");
  });

  it("Selecting Right after None re-enables the reader pane", async () => {
    const { useUiStore } = await import("@/stores/ui");
    const ui = useUiStore();
    ui.setMessageViewMode("none");
    ui.hideReader();
    ui.setMessageViewMode("right");
    expect(ui.readerVisible).toBe(true);
  });

  it("Ctrl+T toggles threading via the keydown listener", async () => {
    const { wrapper } = await mountAt("/");
    // We don't have direct access to uiStore here without importing; check
    // via the menu rendering after dispatch.
    const event = new KeyboardEvent("keydown", { key: "t", ctrlKey: true, cancelable: true });
    window.dispatchEvent(event);
    await wrapper.vm.$nextTick();
    // Open View again to inspect the new state of "Threaded View" prefix.
    await wrapper.find('[data-testid="menu-view"]').trigger("click");
    const threaded = wrapper.find('[data-testid="menu-view-threaded"]');
    // Default threading is enabled; after Ctrl+T it should be off (no checkmark).
    expect(threaded.text()).not.toContain("\u2713");
  });

  it("Ctrl+Q invokes quit_app via the keydown listener", () => {
    mount(MenuBar, { global: { plugins: [makeRouter()] } });
    window.dispatchEvent(
      new KeyboardEvent("keydown", { key: "q", ctrlKey: true, cancelable: true }),
    );
    expect(invokeMock).toHaveBeenCalledWith("quit_app");
  });

  it("ignores shortcuts dispatched while focus is in an input", () => {
    mount(MenuBar, { global: { plugins: [makeRouter()] } });
    const input = document.createElement("input");
    document.body.appendChild(input);
    input.focus();
    input.dispatchEvent(
      new KeyboardEvent("keydown", { key: "q", ctrlKey: true, cancelable: true, bubbles: true }),
    );
    expect(invokeMock).not.toHaveBeenCalled();
    input.remove();
  });

  // --- Issue #150: View menu is context-aware ----------------------------

  it("hides the View menu on routes that have no view-menu items", async () => {
    const { wrapper } = await mountAt("/preferences");
    expect(wrapper.find('[data-testid="menu-view"]').exists()).toBe(false);
    // File and Help still render — only the View top-level button is gone.
    expect(wrapper.text()).toContain("File");
    expect(wrapper.text()).toContain("Help");
  });

  it("hides the View menu on the calendar route (toolbar already exposes it)", async () => {
    // Calendar's Day/Week/Month live in the view's own toolbar; the
    // menu would just duplicate them, so it's deliberately suppressed.
    const { wrapper } = await mountAt("/calendar");
    expect(wrapper.find('[data-testid="menu-view"]').exists()).toBe(false);
  });

  it("shows Right / Bottom contact-pane options on the contacts route", async () => {
    const { wrapper } = await mountAt("/contacts");
    await wrapper.find('[data-testid="menu-view"]').trigger("click");
    const dropdown = wrapper.find('[data-testid="menu-view-dropdown"]');
    expect(dropdown.exists()).toBe(true);
    expect(dropdown.text()).toContain("Contact Pane");
    expect(dropdown.text()).toContain("Right");
    expect(dropdown.text()).toContain("Bottom");
    // Mail-only items must not leak into the contacts context.
    expect(dropdown.text()).not.toContain("Message Pane");
    expect(dropdown.text()).not.toContain("Threaded View");
    // Default contactViewMode is "right" — that row carries the bullet.
    const right = wrapper.find('[data-testid="menu-view-contact-position-right"]');
    expect(right.text()).toContain("●");
  });

  it("selecting Bottom on the contacts route updates the ui store", async () => {
    const { useUiStore } = await import("@/stores/ui");
    const { wrapper } = await mountAt("/contacts");
    await wrapper.find('[data-testid="menu-view"]').trigger("click");
    await wrapper
      .find('[data-testid="menu-view-contact-position-bottom"]')
      .trigger("click");
    expect(useUiStore().contactViewMode).toBe("bottom");
  });

  it("does not show contact-pane items on the mail route", async () => {
    const { wrapper } = await mountAt("/");
    await wrapper.find('[data-testid="menu-view"]').trigger("click");
    const dropdown = wrapper.find('[data-testid="menu-view-dropdown"]');
    expect(dropdown.text()).not.toContain("Contact Pane");
    expect(
      dropdown.find('[data-testid="menu-view-contact-position-right"]').exists(),
    ).toBe(false);
  });
});
