import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { __setPlatformForTests } from "@/lib/shortcuts";

const { openUrlMock } = vi.hoisted(() => ({ openUrlMock: vi.fn() }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: openUrlMock }));

import ComposeMenuBar from "@/components/compose/ComposeMenuBar.vue";
import pkg from "../../package.json";

// `__APP_VERSION__` is injected by Vite's `define` from package.json
// (see vite.config.ts); the test asserts against the same source.
const APP_VERSION = pkg.version as string;

beforeEach(() => {
  __setPlatformForTests(false);
  openUrlMock.mockReset();
  // Clean up any teleported nodes from previous tests so dialog assertions
  // don't pick up stale DOM.
  document.body.innerHTML = "";
});

afterEach(() => {
  __setPlatformForTests(null);
});

function makeWrapper(props: { showCc?: boolean; showBcc?: boolean } = {}) {
  return mount(ComposeMenuBar, {
    props: { showCc: false, showBcc: false, ...props },
  });
}

describe("ComposeMenuBar", () => {
  it("renders all five top-level menu labels", () => {
    const wrapper = makeWrapper();
    const text = wrapper.text();
    expect(text).toContain("File");
    expect(text).toContain("Edit");
    expect(text).toContain("View");
    expect(text).toContain("Options");
    expect(text).toContain("Help");
    // Tools is intentionally absent (ADR 0044)
    expect(text).not.toContain("Tools");
  });

  it("File menu shows Save Draft / Send / Close Window with shortcuts", async () => {
    const wrapper = makeWrapper();
    await wrapper.findAll(".menu-item")[0].trigger("click");
    const dropdown = wrapper.find('[data-testid="compose-menu-file-dropdown"]');
    expect(dropdown.text()).toContain("Save Draft");
    expect(dropdown.text()).toContain("Ctrl+S");
    expect(dropdown.text()).toContain("Send");
    expect(dropdown.text()).toContain("Ctrl+Enter");
    expect(dropdown.text()).toContain("Close Window");
    expect(dropdown.text()).toContain("Esc");
    expect(dropdown.text()).not.toContain("Ctrl+W");
  });

  it("emits saveDraft when Save Draft is clicked", async () => {
    const wrapper = makeWrapper();
    await wrapper.findAll(".menu-item")[0].trigger("click");
    await wrapper.find('[data-testid="compose-menu-save-draft"]').trigger("click");
    expect(wrapper.emitted("saveDraft")).toBeTruthy();
  });

  it("emits send when Send is clicked", async () => {
    const wrapper = makeWrapper();
    await wrapper.findAll(".menu-item")[0].trigger("click");
    await wrapper.find('[data-testid="compose-menu-send"]').trigger("click");
    expect(wrapper.emitted("send")).toBeTruthy();
  });

  it("emits closeWindow when Close Window is clicked", async () => {
    const wrapper = makeWrapper();
    await wrapper.findAll(".menu-item")[0].trigger("click");
    await wrapper.find('[data-testid="compose-menu-close-window"]').trigger("click");
    expect(wrapper.emitted("closeWindow")).toBeTruthy();
  });

  it("Edit menu items dispatch document.execCommand", async () => {
    // happy-dom doesn't implement execCommand; install a spy in its place.
    const exec = vi.fn().mockReturnValue(true);
    Object.defineProperty(document, "execCommand", {
      value: exec,
      configurable: true,
      writable: true,
    });
    const wrapper = makeWrapper();
    await wrapper.findAll(".menu-item")[1].trigger("click");
    await wrapper.find('[data-testid="compose-menu-undo"]').trigger("click");
    expect(exec).toHaveBeenLastCalledWith("undo");
    // The dropdown closes after a click, so re-open it before the next.
    await wrapper.findAll(".menu-item")[1].trigger("click");
    await wrapper.find('[data-testid="compose-menu-redo"]').trigger("click");
    expect(exec).toHaveBeenLastCalledWith("redo");
  });

  it("View menu shows checkmark for the active Cc/Bcc state", async () => {
    const wrapper = makeWrapper({ showCc: true, showBcc: false });
    await wrapper.findAll(".menu-item")[2].trigger("click");
    const cc = wrapper.find('[data-testid="compose-menu-show-cc"]');
    const bcc = wrapper.find('[data-testid="compose-menu-show-bcc"]');
    expect(cc.text()).toContain("\u2713");
    expect(bcc.text()).not.toContain("\u2713");
  });

  it("Options > Attach File emits attach", async () => {
    const wrapper = makeWrapper();
    await wrapper.findAll(".menu-item")[3].trigger("click");
    await wrapper.find('[data-testid="compose-menu-attach"]').trigger("click");
    expect(wrapper.emitted("attach")).toBeTruthy();
  });

  it("Help > About Chithi opens the About dialog with version + links", async () => {
    const wrapper = makeWrapper();
    await wrapper.findAll(".menu-item")[4].trigger("click");
    await wrapper.find('[data-testid="compose-menu-about"]').trigger("click");
    await wrapper.vm.$nextTick();

    const overlay = document.querySelector('[data-testid="about-overlay"]');
    expect(overlay).not.toBeNull();
    expect(document.body.textContent).toContain(APP_VERSION);

    const sourceLink = document.querySelector(
      '[data-testid="about-source-link"]',
    ) as HTMLElement | null;
    expect(sourceLink).not.toBeNull();
    sourceLink?.click();
    expect(openUrlMock).toHaveBeenCalledWith(
      "https://github.com/SUNET/chithi",
    );

    const licenseLink = document.querySelector(
      '[data-testid="about-license-link"]',
    ) as HTMLElement | null;
    licenseLink?.click();
    expect(openUrlMock).toHaveBeenLastCalledWith(
      "https://github.com/SUNET/chithi/blob/main/LICENSE",
    );

    // Closing via the X button removes the overlay.
    const closeBtn = document.querySelector('[data-testid="about-close"]') as HTMLElement | null;
    closeBtn?.click();
    await wrapper.vm.$nextTick();
    expect(document.querySelector('[data-testid="about-overlay"]')).toBeNull();
  });

  it("Ctrl+S keydown emits saveDraft (and works while focus is in a textarea)", () => {
    const wrapper = makeWrapper();
    const textarea = document.createElement("textarea");
    document.body.appendChild(textarea);
    textarea.focus();
    const event = new KeyboardEvent("keydown", {
      key: "s",
      ctrlKey: true,
      cancelable: true,
      bubbles: true,
    });
    textarea.dispatchEvent(event);
    expect(wrapper.emitted("saveDraft")).toBeTruthy();
    textarea.remove();
  });

  it("Ctrl+Enter keydown emits send", () => {
    const wrapper = makeWrapper();
    window.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", ctrlKey: true, cancelable: true }),
    );
    expect(wrapper.emitted("send")).toBeTruthy();
  });

  it("Escape keydown emits closeWindow", () => {
    const wrapper = makeWrapper();
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", cancelable: true }));
    expect(wrapper.emitted("closeWindow")).toBeTruthy();
  });

  it("Escape closes the About dialog when it is open", async () => {
    const wrapper = makeWrapper();
    await wrapper.findAll(".menu-item")[4].trigger("click");
    await wrapper.find('[data-testid="compose-menu-about"]').trigger("click");
    await wrapper.vm.$nextTick();
    expect(document.querySelector('[data-testid="about-overlay"]')).not.toBeNull();

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", cancelable: true }));
    await wrapper.vm.$nextTick();
    expect(document.querySelector('[data-testid="about-overlay"]')).toBeNull();
  });

  it("Ctrl+Shift+A keydown emits attach", () => {
    const wrapper = makeWrapper();
    window.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "A",
        ctrlKey: true,
        shiftKey: true,
        cancelable: true,
      }),
    );
    expect(wrapper.emitted("attach")).toBeTruthy();
  });

  it("Ctrl+Z is NOT intercepted (left to browser/WebKitGTK workaround)", () => {
    const wrapper = makeWrapper();
    window.dispatchEvent(
      new KeyboardEvent("keydown", { key: "z", ctrlKey: true, cancelable: true }),
    );
    // No emit should fire — the menu does not bind Edit shortcuts.
    expect(wrapper.emitted("saveDraft")).toBeFalsy();
    expect(wrapper.emitted("send")).toBeFalsy();
    expect(wrapper.emitted("closeWindow")).toBeFalsy();
    expect(wrapper.emitted("attach")).toBeFalsy();
  });
});
