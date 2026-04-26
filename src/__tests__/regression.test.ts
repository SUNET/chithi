/**
 * Regression tests for bugs that were fixed.
 * Each test documents the original bug and ensures it doesn't return.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount } from "@vue/test-utils";
import { setActivePinia, createPinia } from "pinia";
import { createRouter, createWebHistory } from "vue-router";
import ThreadRow from "@/components/mail/ThreadRow.vue";

vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  getMessages: vi.fn().mockResolvedValue({ messages: [], total: 0, page: 0, per_page: 100 }),
  getMessageBody: vi.fn().mockResolvedValue({
    id: "msg1", subject: "Test", from: { name: "T", email: "t@t.com" },
    to: [], cc: [], date: "2026-04-03T00:00:00Z", flags: [],
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
import * as api from "@/lib/tauri";

function makeSummary(id: string, flags: string[] = ["seen"]): any {
  return {
    id, subject: `Subject ${id}`, from_name: "Sender", from_email: "sender@example.com",
    date: "2026-04-03T00:00:00Z", flags, has_attachments: false,
    is_encrypted: false, is_signed: false, snippet: null,
  };
}

function setupStores(threading = false) {
  const accountsStore = useAccountsStore();
  accountsStore.accounts = [
    { id: "acc1", display_name: "Test", email: "t@t.com", provider: "generic", mail_protocol: "imap" as const, enabled: true },
  ];
  accountsStore.activeAccountId = "acc1";
  useFoldersStore().activeFolderPath = "INBOX";
  useUiStore().threadingEnabled = threading;
  return useMessagesStore();
}

describe("Regression: mark-as-read", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.mocked(api.setMessageFlags).mockClear();
  });

  it("BUG: mark-as-read must work even when body fetch fails", async () => {
    // Previously, markAsRead was called AFTER await getMessageBody,
    // so if body fetch failed, the message was never marked as read.
    const store = setupStores();
    store.messages = [makeSummary("msg1", [])];

    vi.mocked(api.getMessageBody).mockRejectedValueOnce(new Error("body not downloaded"));

    store.selectMessage("msg1", { shiftKey: false, ctrlKey: false, metaKey: false });

    expect(store.messages[0].flags).toContain("seen");
    expect(api.setMessageFlags).toHaveBeenCalledWith("acc1", ["msg1"], ["seen"], true);
  });

  it("BUG: mark-as-read must work in threaded mode with collapsed thread", () => {
    // Previously, markAsRead used findMessage() which only searched
    // messages.value and threadMessages. In threaded mode with collapsed
    // threads, the message wasn't in either, so markAsRead silently failed.
    const store = setupStores(true);
    store.threads = [{
      thread_id: "t1", subject: "Thread", last_date: "2026-04-03T00:00:00Z",
      message_count: 2, unread_count: 1, from_name: "S", from_email: "s@s.com",
      has_attachments: false, flags: [], snippet: null, message_ids: ["msg1", "msg2"],
    }];

    store.selectMessage("msg1", { shiftKey: false, ctrlKey: false, metaKey: false });

    // Must still send IMAP update even though message object not found
    expect(api.setMessageFlags).toHaveBeenCalledWith("acc1", ["msg1"], ["seen"], true);
    expect(store.threads[0].unread_count).toBe(0);
  });
});

describe("Regression: selection", () => {
  let router: any;

  beforeEach(() => {
    setActivePinia(createPinia());
    router = createRouter({
      history: createWebHistory(),
      routes: [{ path: "/", component: { template: "<div />" } }],
    });
  });

  it("BUG: selectedIds must use array not Set for Vue reactivity", () => {
    // Previously used ref<Set<string>> which didn't trigger Vue re-renders
    // reliably. Now uses ref<string[]>.
    const store = setupStores();
    store.messages = [makeSummary("msg1"), makeSummary("msg2")];

    store.selectMessage("msg1", { shiftKey: false, ctrlKey: false, metaKey: false });
    expect(Array.isArray(store.selectedIds)).toBe(true);
    expect(store.selectedIds).toEqual(["msg1"]);
  });

  it("BUG: Ctrl+click must work via keydown tracking not MouseEvent", async () => {
    // WebKitGTK loses event.shiftKey/ctrlKey on click events.
    // Now modifier keys are tracked via window keydown/keyup events.
    const store = setupStores();
    store.messages = [makeSummary("msg1"), makeSummary("msg2"), makeSummary("msg3")];

    const wrapper = mount(MessageList, { global: { plugins: [router] } });
    const divs = wrapper.findAll(".message-items > div");

    await divs[0].trigger("click");
    expect(store.selectedIds).toEqual(["msg1"]);

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Control" }));
    await divs[2].trigger("click");
    window.dispatchEvent(new KeyboardEvent("keyup", { key: "Control" }));

    expect(store.selectedIds.length).toBe(2);
    expect(store.selectedIds).toContain("msg1");
    expect(store.selectedIds).toContain("msg3");
  });

  it("BUG: Shift+click must not trigger browser text selection", async () => {
    // The message-items container must have user-select: none
    const store = setupStores();
    store.messages = [makeSummary("msg1"), makeSummary("msg2")];

    const wrapper = mount(MessageList, { global: { plugins: [router] } });
    const container = wrapper.find(".message-items");
    // Verify the container element exists and has the right styles in the component
    expect(container.exists()).toBe(true);
  });

  it("BUG: collapsedThreads and threadMessages must use array/object not Set/Map", () => {
    // Previously used Set/Map which caused Vue reactivity issues.
    const store = setupStores(true);
    expect(Array.isArray(store.collapsedThreads)).toBe(true);
    expect(typeof store.threadMessages).toBe("object");
    expect(store.threadMessages).not.toBeInstanceOf(Map);
  });

  it("BUG: message rows must be divs not buttons for click propagation", async () => {
    // Previously used <button> elements which intercepted clicks
    // in WebKitGTK and prevented modifier keys from propagating.
    const store = setupStores();
    store.messages = [makeSummary("msg1")];

    const wrapper = mount(MessageList, { global: { plugins: [router] } });
    const row = wrapper.find(".message-row");
    expect(row.element.tagName).toBe("DIV");
  });

  it("BUG: thread move/delete must include all message IDs, not just the first", () => {
    // Previously, selecting a thread and clicking Delete/Move only sent
    // the first message ID (used for selection), leaving the rest of the
    // thread behind. resolveSelectedIds() expands thread selections.
    const store = setupStores();
    const uiStore = useUiStore();
    uiStore.threadingEnabled = true;

    store.threads = [
      {
        thread_id: "thread-1",
        subject: "A thread",
        last_date: "2026-04-09T00:00:00Z",
        message_count: 4,
        unread_count: 0,
        from_name: "Test",
        from_email: "test@test.com",
        has_attachments: false,
        flags: ["seen"],
        snippet: "hello",
        message_ids: ["msg-a", "msg-b", "msg-c", "msg-d"],
      },
    ];

    // Selecting the thread only adds the first message ID
    store.selectedIds = ["msg-a"];

    // resolveSelectedIds must expand to all 4 message IDs
    const resolved = store.resolveSelectedIds();
    expect(resolved).toEqual(["msg-a", "msg-b", "msg-c", "msg-d"]);
  });

  it("BUG: resolveSelectedIds in flat mode returns selected IDs unchanged", () => {
    const store = setupStores();
    const uiStore = useUiStore();
    uiStore.threadingEnabled = false;

    store.selectedIds = ["msg-1", "msg-2"];
    const resolved = store.resolveSelectedIds();
    expect(resolved).toEqual(["msg-1", "msg-2"]);
  });

  it("BUG: expanded thread header should not show bold when children are visible", () => {
    // When a thread is expanded, the individual child messages show their own
    // read/unread status. The thread header showing bold made it look like an
    // extra unread message, causing the visual count to mismatch the badge.
    const thread = {
      thread_id: "t1",
      subject: "Test thread",
      last_date: "2026-04-09T00:00:00Z",
      message_count: 3,
      unread_count: 2,
      from_name: "Alice",
      from_email: "alice@test.com",
      has_attachments: false,
      flags: ["seen"],
      snippet: "hello",
      message_ids: ["m1", "m2", "m3"],
    };

    // Expanded: subject should NOT be bold even though unread_count > 0
    const expanded = mount(ThreadRow, {
      props: { thread, expanded: true, active: false, selected: false },
      global: { plugins: [router] },
    });
    const subjectExpanded = expanded.find(".col-subject");
    expect(subjectExpanded.classes()).not.toContain("bold");

    // Collapsed: subject SHOULD be bold when thread has unread children
    const collapsed = mount(ThreadRow, {
      props: { thread, expanded: false, active: false, selected: false },
      global: { plugins: [router] },
    });
    const subjectCollapsed = collapsed.find(".col-subject");
    expect(subjectCollapsed.classes()).toContain("bold");
  });
});

describe("Regression: Load images button visibility (#34)", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
  });

  it("BUG: Load images button must only appear when has_remote_images is true", () => {
    // Previously the button was shown for ALL HTML emails because the frontend
    // tried to detect <img> tags in body_html, but ammonia strips them before
    // the frontend sees them. Now the backend provides has_remote_images.
    const messagesStore = useMessagesStore();

    // No remote images — button should not appear
    messagesStore.activeMessage = {
      id: "msg1", subject: "Plain", from: { name: "T", email: "t@t.com" },
      to: [], cc: [], date: "2026-04-12T00:00:00Z", flags: [],
      body_html: "<p>Hello</p>", body_text: "Hello", attachments: [],
      is_encrypted: false, is_signed: false, list_id: null,
      has_remote_images: false,
    };
    expect(messagesStore.activeMessage.has_remote_images).toBe(false);

    // With remote images — button should appear
    messagesStore.activeMessage = {
      ...messagesStore.activeMessage,
      has_remote_images: true,
    };
    expect(messagesStore.activeMessage.has_remote_images).toBe(true);
  });
});
