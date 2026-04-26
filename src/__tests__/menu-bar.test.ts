import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";
import { __setPlatformForTests } from "@/lib/shortcuts";
import MenuBar from "@/components/common/MenuBar.vue";

const { invokeMock, closeMock, setDecorationsMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  closeMock: vi.fn(),
  setDecorationsMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ close: closeMock, setDecorations: setDecorationsMock }),
}));
vi.mock("@/lib/tauri", () => ({
  listTimezones: vi.fn().mockResolvedValue([]),
  getDefaultTimezone: vi.fn().mockResolvedValue("UTC"),
}));

function makeRouter() {
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/", component: { template: "<div/>" } },
      { path: "/settings", component: { template: "<div/>" } },
      { path: "/filters", component: { template: "<div/>" } },
    ],
  });
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  closeMock.mockReset();
  setDecorationsMock.mockReset();
  __setPlatformForTests(false);
});

afterEach(() => {
  __setPlatformForTests(null);
});

describe("MenuBar", () => {
  it("renders File and View menu labels", () => {
    const wrapper = mount(MenuBar, { global: { plugins: [makeRouter()] } });
    expect(wrapper.text()).toContain("File");
    expect(wrapper.text()).toContain("View");
  });

  it("File menu shows Preferences / Close Window / Quit with shortcut labels", async () => {
    const wrapper = mount(MenuBar, { global: { plugins: [makeRouter()] } });
    await wrapper.find('.menu-item:nth-of-type(1)').trigger("click");
    const dropdown = wrapper.find('[data-testid="menu-file-dropdown"]');
    expect(dropdown.exists()).toBe(true);
    expect(dropdown.text()).toContain("Preferences");
    expect(dropdown.text()).toContain("Ctrl+,");
    expect(dropdown.text()).toContain("Close Window");
    expect(dropdown.text()).toContain("Ctrl+W");
    expect(dropdown.text()).toContain("Quit");
    expect(dropdown.text()).toContain("Ctrl+Q");
  });

  it("File > Quit invokes the quit_app command", async () => {
    const wrapper = mount(MenuBar, { global: { plugins: [makeRouter()] } });
    await wrapper.find('.menu-item:nth-of-type(1)').trigger("click");
    await wrapper.find('[data-testid="menu-file-quit"]').trigger("click");
    expect(invokeMock).toHaveBeenCalledWith("quit_app");
  });

  it("File > Close Window calls getCurrentWindow().close()", async () => {
    const wrapper = mount(MenuBar, { global: { plugins: [makeRouter()] } });
    await wrapper.find('.menu-item:nth-of-type(1)').trigger("click");
    await wrapper.find('[data-testid="menu-file-close-window"]').trigger("click");
    expect(closeMock).toHaveBeenCalled();
  });

  it("File > Preferences routes to /settings", async () => {
    const router = makeRouter();
    const push = vi.spyOn(router, "push");
    const wrapper = mount(MenuBar, { global: { plugins: [router] } });
    await wrapper.find('.menu-item:nth-of-type(1)').trigger("click");
    await wrapper.find('[data-testid="menu-file-preferences"]').trigger("click");
    expect(push).toHaveBeenCalledWith("/settings");
  });

  it("View menu shows the radio group with the active position prefixed", async () => {
    const wrapper = mount(MenuBar, { global: { plugins: [makeRouter()] } });
    await wrapper.find('.menu-item:nth-of-type(2)').trigger("click");
    const dropdown = wrapper.find('[data-testid="menu-view-dropdown"]');
    expect(dropdown.text()).toContain("Message Pane Position");
    expect(dropdown.text()).toContain("Right");
    expect(dropdown.text()).toContain("Bottom");
    expect(dropdown.text()).toContain("Tabs");
    // Default messageViewMode is "right"; that row should carry the bullet.
    const right = wrapper.find('[data-testid="menu-view-position-right"]');
    expect(right.text()).toContain("\u25CF");
    const bottom = wrapper.find('[data-testid="menu-view-position-bottom"]');
    expect(bottom.text()).not.toContain("\u25CF");
  });

  it("Ctrl+T toggles threading via the keydown listener", async () => {
    const wrapper = mount(MenuBar, { global: { plugins: [makeRouter()] } });
    // We don't have direct access to uiStore here without importing; check
    // via the menu rendering after dispatch.
    const event = new KeyboardEvent("keydown", { key: "t", ctrlKey: true, cancelable: true });
    window.dispatchEvent(event);
    await wrapper.vm.$nextTick();
    // Open View again to inspect the new state of "Threaded View" prefix.
    await wrapper.find('.menu-item:nth-of-type(2)').trigger("click");
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
});
