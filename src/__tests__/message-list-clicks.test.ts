import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount } from "@vue/test-utils";
import { setActivePinia, createPinia } from "pinia";
import { createRouter, createWebHistory } from "vue-router";

vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  getMessages: vi.fn().mockResolvedValue({ messages: [], total: 0, page: 0, per_page: 100 }),
  getMessageBody: vi.fn().mockResolvedValue({
    id: "msg1", subject: "Test", from: { name: "T", email: "t@t.com" },
    to: [], cc: [], date: "2026-04-03T00:00:00Z", flags: ["seen"],
    body_html: null, body_text: "Hello", attachments: [],
    is_encrypted: false, is_signed: false, list_id: null,
  }),
  setMessageFlags: vi.fn().mockResolvedValue(undefined),
  deleteMessages: vi.fn().mockResolvedValue(undefined),
  listFolders: vi.fn().mockResolvedValue([]),
  getThreadedMessages: vi.fn().mockResolvedValue({
    threads: [], total_threads: 0, total_messages: 0, page: 0, per_page: 100,
  }),
  getThreadMessages: vi.fn().mockResolvedValue([]),
  triggerSync: vi.fn().mockResolvedValue(undefined),
  backfillThreads: vi.fn().mockResolvedValue(0),
  prefetchBodies: vi.fn().mockResolvedValue(0),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import MessageList from "@/components/mail/MessageList.vue";
import { useMessagesStore } from "@/stores/messages";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useUiStore } from "@/stores/ui";

function makeSummary(id: string): any {
  return {
    id, subject: `Subject ${id}`, from_name: "Sender", from_email: "sender@example.com",
    date: "2026-04-03T00:00:00Z", flags: ["seen"], has_attachments: false,
    is_encrypted: false, is_signed: false, snippet: null,
  };
}

function setupStores() {
  const accountsStore = useAccountsStore();
  accountsStore.accounts = [
    {
      id: "acc1", display_name: "Test", email: "t@t.com",
      provider: "generic", mail_protocol: "imap" as const, enabled: true,
      mail_sync_interval_seconds: null,
      calendar_sync_interval_seconds: null,
      contacts_sync_interval_seconds: null,
    },
  ];
  accountsStore.activeAccountId = "acc1";
  useFoldersStore().activeFolderPath = "INBOX";
  useUiStore().threadingEnabled = false;
  const store = useMessagesStore();
  store.messages = [makeSummary("msg1"), makeSummary("msg2"), makeSummary("msg3"), makeSummary("msg4")];
  store.total = 4;
  return store;
}

describe("MessageList click handling", () => {
  let router: any;

  beforeEach(() => {
    setActivePinia(createPinia());
    router = createRouter({
      history: createWebHistory(),
      routes: [{ path: "/", component: { template: "<div />" } }],
    });
  });

  it("renders message rows", () => {
    setupStores();
    const wrapper = mount(MessageList, { global: { plugins: [router] } });
    expect(wrapper.findAll(".message-row").length).toBe(4);
  });

  it("click selects one message", async () => {
    const store = setupStores();
    const wrapper = mount(MessageList, { global: { plugins: [router] } });

    const divs = wrapper.findAll(".message-items > div");
    await divs[1].trigger("click");

    expect(store.selectedIds).toEqual(["msg2"]);
    expect(store.activeMessageId).toBe("msg2");
  });

  it("Shift held + click selects range via keydown tracking", async () => {
    // The component tracks Shift via keydown/keyup events on window,
    // not via MouseEvent.shiftKey, because WebKitGTK can lose modifier state.
    const store = setupStores();
    const wrapper = mount(MessageList, { global: { plugins: [router] } });

    const divs = wrapper.findAll(".message-items > div");

    // Click msg1 normally
    await divs[0].trigger("click");
    expect(store.selectedIds).toEqual(["msg1"]);

    // Simulate Shift keydown, then click msg4
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Shift" }));
    await divs[3].trigger("click");
    window.dispatchEvent(new KeyboardEvent("keyup", { key: "Shift" }));

    expect(store.selectedIds.length).toBe(4);
    expect(store.selectedIds).toContain("msg1");
    expect(store.selectedIds).toContain("msg2");
    expect(store.selectedIds).toContain("msg3");
    expect(store.selectedIds).toContain("msg4");
  });

  it("Ctrl held + click toggles via keydown tracking", async () => {
    const store = setupStores();
    const wrapper = mount(MessageList, { global: { plugins: [router] } });

    const divs = wrapper.findAll(".message-items > div");

    await divs[0].trigger("click");
    expect(store.selectedIds).toEqual(["msg1"]);

    // Simulate Ctrl keydown, click msg3, then keyup
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Control" }));
    await divs[2].trigger("click");
    window.dispatchEvent(new KeyboardEvent("keyup", { key: "Control" }));

    expect(store.selectedIds.length).toBe(2);
    expect(store.selectedIds).toContain("msg1");
    expect(store.selectedIds).toContain("msg3");
  });

  it("selected messages have .selected class", async () => {
    setupStores();
    const wrapper = mount(MessageList, { global: { plugins: [router] } });

    const divs = wrapper.findAll(".message-items > div");
    await divs[0].trigger("click");

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Control" }));
    await divs[2].trigger("click");
    window.dispatchEvent(new KeyboardEvent("keyup", { key: "Control" }));

    await wrapper.vm.$nextTick();

    const rows = wrapper.findAll(".message-row");
    expect(rows[0].classes()).toContain("selected");
    expect(rows[1].classes()).not.toContain("selected");
    expect(rows[2].classes()).toContain("selected");
  });

  it("message rows are divs not buttons", () => {
    setupStores();
    const wrapper = mount(MessageList, { global: { plugins: [router] } });
    const row = wrapper.find(".message-row");
    expect(row.element.tagName).toBe("DIV");
  });
});
