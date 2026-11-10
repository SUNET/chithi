import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { nextTick } from "vue";

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

vi.mock("@/lib/tauri", () => ({
  listFolders: vi.fn().mockResolvedValue([]),
  getLastView: vi.fn().mockResolvedValue({ account_id: null, folder_path: null }),
  saveLastView: vi.fn().mockResolvedValue(undefined),
}));

import FolderTree from "@/components/mail/FolderTree.vue";
import { isDragging } from "@/lib/drag-state";
import { useAccountsStore } from "@/stores/accounts";

let nextFrameId = 1;
let frames: Map<number, FrameRequestCallback>;

function runNextFrame() {
  const frame = frames.entries().next().value as
    | [number, FrameRequestCallback]
    | undefined;
  if (!frame) return;
  frames.delete(frame[0]);
  frame[1](0);
}

function runUntilIdle() {
  let count = 0;
  while (frames.size > 0 && count < 100) {
    runNextFrame();
    count++;
  }
  expect(count).toBeLessThan(100);
}

function mountScrollableFolderTree() {
  const wrapper = mount(FolderTree);
  const element = wrapper.get<HTMLElement>(".folder-tree").element;
  Object.defineProperties(element, {
    clientHeight: { configurable: true, value: 200 },
    scrollHeight: { configurable: true, value: 600 },
  });
  vi.spyOn(element, "getBoundingClientRect").mockReturnValue({
    x: 0,
    y: 100,
    top: 100,
    right: 240,
    bottom: 300,
    left: 0,
    width: 240,
    height: 200,
    toJSON: () => ({}),
  });
  return { wrapper, element };
}

beforeEach(() => {
  setActivePinia(createPinia());
  useAccountsStore().accounts = [];
  isDragging.value = false;
  nextFrameId = 1;
  frames = new Map();
  vi.stubGlobal("requestAnimationFrame", vi.fn((callback: FrameRequestCallback) => {
    const id = nextFrameId++;
    frames.set(id, callback);
    return id;
  }));
  vi.stubGlobal("cancelAnimationFrame", vi.fn((id: number) => {
    frames.delete(id);
  }));
});

afterEach(() => {
  isDragging.value = false;
  vi.unstubAllGlobals();
});

describe("FolderTree drag auto-scroll", () => {
  it("scrolls to the bottom while a drag remains near the lower edge", async () => {
    const { wrapper, element } = mountScrollableFolderTree();
    element.scrollTop = 100;
    isDragging.value = true;

    await wrapper.get(".folder-tree").trigger("mousemove", { clientY: 298 });
    runUntilIdle();

    expect(element.scrollTop).toBe(400);
  });

  it("scrolls to the top while a drag remains near the upper edge", async () => {
    const { wrapper, element } = mountScrollableFolderTree();
    element.scrollTop = 100;
    isDragging.value = true;

    await wrapper.get(".folder-tree").trigger("mousemove", { clientY: 102 });
    runUntilIdle();

    expect(element.scrollTop).toBe(0);
  });

  it("cancels a pending scroll frame when dragging ends", async () => {
    const { wrapper, element } = mountScrollableFolderTree();
    element.scrollTop = 100;
    isDragging.value = true;
    await wrapper.get(".folder-tree").trigger("mousemove", { clientY: 298 });
    expect(frames.size).toBe(1);

    isDragging.value = false;
    await nextTick();

    expect(frames.size).toBe(0);
    expect(element.scrollTop).toBe(100);
  });
});
