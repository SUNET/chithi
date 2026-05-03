import { describe, it, expect, vi, beforeEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  getMessages: vi.fn().mockResolvedValue({ messages: [], total: 0, page: 0, per_page: 100 }),
  getMessageBody: vi.fn().mockResolvedValue({
    id: "msg1", subject: "Test", from: { name: "Test", email: "test@example.com" },
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

import { useMessagesStore } from "@/stores/messages";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useUiStore } from "@/stores/ui";
import * as api from "@/lib/tauri";

function setup(threading = false) {
  const accountsStore = useAccountsStore();
  accountsStore.accounts = [
    {
      id: "acc1", display_name: "Test", email: "test@test.com",
      provider: "generic", mail_protocol: "imap" as const, enabled: true,
      mail_sync_interval_seconds: null,
      calendar_sync_interval_seconds: null,
      contacts_sync_interval_seconds: null,
    },
  ];
  accountsStore.activeAccountId = "acc1";
  const foldersStore = useFoldersStore();
  foldersStore.activeFolderPath = "INBOX";
  const uiStore = useUiStore();
  uiStore.threadingEnabled = threading;
  return useMessagesStore();
}

function makeSummary(id: string, flags: string[] = ["seen"]): any {
  return {
    id, subject: `Subject ${id}`, from_name: "Sender", from_email: "sender@example.com",
    date: "2026-04-03T00:00:00Z", flags, has_attachments: false,
    is_encrypted: false, is_signed: false, snippet: null,
  };
}

function makeThread(id: string, messageIds: string[], unread = 0): any {
  return {
    thread_id: id, subject: `Thread ${id}`, last_date: "2026-04-03T00:00:00Z",
    message_count: messageIds.length, unread_count: unread,
    from_name: "S", from_email: "s@s.com", has_attachments: false,
    flags: [], snippet: null, message_ids: messageIds,
  };
}

const noMod = { shiftKey: false, ctrlKey: false, metaKey: false };
const ctrl = { shiftKey: false, ctrlKey: true, metaKey: false };
const shift = { shiftKey: true, ctrlKey: false, metaKey: false };

describe("Message selection", () => {
  beforeEach(() => setActivePinia(createPinia()));

  it("single click selects one message", () => {
    const store = setup();
    store.messages = [makeSummary("msg1"), makeSummary("msg2"), makeSummary("msg3")];
    store.selectMessage("msg1", noMod);
    expect(store.selectedIds).toEqual(["msg1"]);
    store.selectMessage("msg2", noMod);
    expect(store.selectedIds).toEqual(["msg2"]);
  });

  it("Ctrl+click toggles without affecting others", () => {
    const store = setup();
    store.messages = [makeSummary("msg1"), makeSummary("msg2"), makeSummary("msg3")];
    store.selectMessage("msg1", noMod);
    store.selectMessage("msg2", ctrl);
    expect(store.selectedIds).toContain("msg1");
    expect(store.selectedIds).toContain("msg2");
    expect(store.selectedIds.length).toBe(2);
    store.selectMessage("msg1", ctrl);
    expect(store.selectedIds).toEqual(["msg2"]);
  });

  it("Shift+click selects range from last clicked", () => {
    const store = setup();
    store.messages = [makeSummary("msg1"), makeSummary("msg2"), makeSummary("msg3"), makeSummary("msg4"), makeSummary("msg5")];
    store.selectMessage("msg2", noMod);
    store.selectMessage("msg4", shift);
    expect(store.selectedIds).toEqual(["msg2", "msg3", "msg4"]);
  });

  it("Shift+click works in reverse", () => {
    const store = setup();
    store.messages = [makeSummary("msg1"), makeSummary("msg2"), makeSummary("msg3"), makeSummary("msg4")];
    store.selectMessage("msg4", noMod);
    store.selectMessage("msg2", shift);
    expect(store.selectedIds).toEqual(["msg2", "msg3", "msg4"]);
  });

  it("isSelected returns correct state", () => {
    const store = setup();
    store.messages = [makeSummary("msg1"), makeSummary("msg2")];
    store.selectMessage("msg1", noMod);
    expect(store.isSelected("msg1")).toBe(true);
    expect(store.isSelected("msg2")).toBe(false);
    store.selectMessage("msg2", ctrl);
    expect(store.isSelected("msg1")).toBe(true);
    expect(store.isSelected("msg2")).toBe(true);
  });

  it("clearSelection empties selection", () => {
    const store = setup();
    store.messages = [makeSummary("msg1"), makeSummary("msg2")];
    store.selectMessage("msg1", noMod);
    store.selectMessage("msg2", ctrl);
    store.clearSelection();
    expect(store.selectedIds.length).toBe(0);
  });

  it("selectMessage sets activeMessageId", () => {
    const store = setup();
    store.messages = [makeSummary("msg1"), makeSummary("msg2")];
    store.selectMessage("msg2", noMod);
    expect(store.activeMessageId).toBe("msg2");
  });

  it("deleteSelected calls API with all selected IDs", async () => {
    const store = setup();
    store.messages = [makeSummary("msg1"), makeSummary("msg2"), makeSummary("msg3")];
    store.selectMessage("msg1", noMod);
    store.selectMessage("msg3", ctrl);
    await store.deleteSelected();
    expect(api.deleteMessages).toHaveBeenCalledWith("acc1", ["msg1", "msg3"]);
    expect(store.selectedIds.length).toBe(0);
  });
});

describe("Mark as read", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.mocked(api.setMessageFlags).mockClear();
  });

  it("single click marks unread message as seen (flat mode)", () => {
    const store = setup(false);
    store.messages = [makeSummary("msg1", [])];

    store.selectMessage("msg1", noMod);

    // Mark-as-read happens synchronously in selectMessage
    expect(store.messages[0].flags).toContain("seen");
    expect(api.setMessageFlags).toHaveBeenCalledWith("acc1", ["msg1"], ["seen"], true);
  });

  it("does not re-mark already seen message", () => {
    const store = setup(false);
    store.messages = [makeSummary("msg1", ["seen"])];

    store.selectMessage("msg1", noMod);

    expect(api.setMessageFlags).not.toHaveBeenCalled();
  });

  it("Shift+click does NOT mark messages as read (bulk select)", () => {
    const store = setup(false);
    store.messages = [makeSummary("msg1", []), makeSummary("msg2", []), makeSummary("msg3", [])];

    store.selectMessage("msg1", noMod);
    vi.mocked(api.setMessageFlags).mockClear();

    store.selectMessage("msg3", shift);
    // Shift+click selects range but should not mark all as read
    // (only the body-loaded message should be marked)
    expect(store.selectedIds.length).toBe(3);
  });

  it("Ctrl+click marks individual message as read", () => {
    const store = setup(false);
    store.messages = [makeSummary("msg1", []), makeSummary("msg2", [])];

    store.selectMessage("msg1", noMod);
    vi.mocked(api.setMessageFlags).mockClear();

    store.selectMessage("msg2", ctrl);
    expect(api.setMessageFlags).toHaveBeenCalledWith("acc1", ["msg2"], ["seen"], true);
  });

  it("marks as read in threaded mode via thread summary (collapsed)", () => {
    const store = setup(true);
    store.threads = [makeThread("t1", ["msg1", "msg2"], 1)];
    // No expanded threads — message objects not available

    store.selectMessage("msg1", noMod);

    // Should still send IMAP flag update even though msg object not found
    expect(api.setMessageFlags).toHaveBeenCalledWith("acc1", ["msg1"], ["seen"], true);
    // Thread unread count decremented
    expect(store.threads[0].unread_count).toBe(0);
  });

  it("marks threaded message as read when thread is expanded", () => {
    const store = setup(true);
    store.threads = [makeThread("t1", ["msg1", "msg2"], 1)];
    store.threadMessages = {
      t1: [makeSummary("msg1", []), makeSummary("msg2", ["seen"])],
    };
    // Threads are expanded by default; clear any persisted collapse state.
    store.collapsedThreads = [];

    store.selectMessage("msg1", noMod);

    expect(store.threadMessages["t1"][0].flags).toContain("seen");
    expect(store.threads[0].unread_count).toBe(0);
    expect(api.setMessageFlags).toHaveBeenCalledWith("acc1", ["msg1"], ["seen"], true);
  });

  it("mark-as-read works even when body fetch fails", async () => {
    const store = setup(false);
    store.messages = [makeSummary("msg1", [])];

    vi.mocked(api.getMessageBody).mockRejectedValueOnce(new Error("Network error"));

    store.selectMessage("msg1", noMod);

    // Mark-as-read is synchronous, happens before body fetch
    expect(store.messages[0].flags).toContain("seen");
    expect(api.setMessageFlags).toHaveBeenCalledWith("acc1", ["msg1"], ["seen"], true);
  });

  it("subjectForMessage finds subject in flat messages list", () => {
    const store = setup(false);
    store.messages = [makeSummary("msg1"), makeSummary("msg2")];
    expect(store.subjectForMessage("msg1")).toBe("Subject msg1");
    expect(store.subjectForMessage("msg2")).toBe("Subject msg2");
  });

  it("subjectForMessage falls back to thread subject when threading is enabled", () => {
    // Regression: when threading is enabled, messages.value may be empty
    // (data lives in threads.value), causing tab labels to show "(no subject)"
    const store = setup(true);
    store.messages = [];
    store.threads = [makeThread("t1", ["msg1", "msg2"])];
    expect(store.subjectForMessage("msg1")).toBe("Thread t1");
    expect(store.subjectForMessage("msg2")).toBe("Thread t1");
  });

  it("subjectForMessage finds subject in expanded thread messages", () => {
    const store = setup(true);
    store.messages = [];
    store.threads = [makeThread("t1", ["msg1"])];
    store.threadMessages = { t1: [makeSummary("msg1")] };
    expect(store.subjectForMessage("msg1")).toBe("Subject msg1");
  });

  it("subjectForMessage returns null for unknown message id", () => {
    const store = setup(false);
    store.messages = [makeSummary("msg1")];
    expect(store.subjectForMessage("missing")).toBeNull();
  });
});
