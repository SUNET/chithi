import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";
import { __setPlatformForTests } from "@/lib/shortcuts";
import MenuBar from "@/components/common/MenuBar.vue";

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

(globalThis as { __APP_VERSION__?: string }).__APP_VERSION__ = "0.0.0-test";
vi.mock("@/lib/tauri", () => ({
  listTimezones: vi.fn().mockResolvedValue([]),
  getDefaultTimezone: vi.fn().mockResolvedValue("UTC"),
}));

function makeRouter() {
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/", component: { template: "<div/>" } },
      { path: "/preferences", component: { template: "<div/>" } },
    ],
  });
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
  it("renders File / View / Help menu labels", () => {
    const wrapper = mount(MenuBar, { global: { plugins: [makeRouter()] } });
    expect(wrapper.text()).toContain("File");
    expect(wrapper.text()).toContain("View");
    expect(wrapper.text()).toContain("Help");
  });

  it("Help > About opens the About dialog", async () => {
    const wrapper = mount(MenuBar, { global: { plugins: [makeRouter()] } });
    await wrapper.find('.menu-item:nth-of-type(3)').trigger("click");
    await wrapper.find('[data-testid="menu-help-about"]').trigger("click");
    await wrapper.vm.$nextTick();
    expect(document.querySelector('[data-testid="about-overlay"]')).not.toBeNull();
    expect(document.body.textContent).toContain("0.0.0-test");
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
    const wrapper = mount(MenuBar, { global: { plugins: [makeRouter()] } });
    await wrapper.find('.menu-item:nth-of-type(2)').trigger("click");
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
    const wrapper = mount(MenuBar, { global: { plugins: [makeRouter()] } });
    await wrapper.find('.menu-item:nth-of-type(2)').trigger("click");
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
