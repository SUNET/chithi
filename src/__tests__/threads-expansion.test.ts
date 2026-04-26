import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

const { getThreadMessagesMock, getThreadedMessagesMock } = vi.hoisted(() => ({
  getThreadMessagesMock: vi.fn(),
  getThreadedMessagesMock: vi.fn(),
}));

vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  listFolders: vi.fn().mockResolvedValue([]),
  triggerSync: vi.fn().mockResolvedValue(undefined),
  getMessages: vi
    .fn()
    .mockResolvedValue({ messages: [], total: 0, page: 0, per_page: 100 }),
  getThreadedMessages: getThreadedMessagesMock,
  getThreadMessages: getThreadMessagesMock,
  getMessageBody: vi.fn().mockResolvedValue(null),
  setMessageFlags: vi.fn().mockResolvedValue(undefined),
  deleteMessages: vi.fn().mockResolvedValue(undefined),
  prefetchBodies: vi.fn().mockResolvedValue(0),
}));

import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useUiStore } from "@/stores/ui";
import { useMessagesStore } from "@/stores/messages";

function withActiveContext() {
  const accountsStore = useAccountsStore();
  accountsStore.accounts = [
    {
      id: "acc1",
      display_name: "Test",
      email: "test@example.com",
      provider: "generic",
      mail_protocol: "imap" as const,
      enabled: true,
    },
  ];
  accountsStore.activeAccountId = "acc1";
  const foldersStore = useFoldersStore();
  foldersStore.activeFolderPath = "INBOX";
}

function makeThread(id: string, messageCount = 2) {
  return {
    thread_id: id,
    subject: `Subject ${id}`,
    last_date: "2026-04-26T00:00:00Z",
    message_count: messageCount,
    unread_count: 0,
    from_name: "Alice",
    from_email: "alice@example.com",
    has_attachments: false,
    flags: [],
    snippet: null,
    message_ids: Array.from({ length: messageCount }, (_, i) => `${id}_${i}`),
  };
}

beforeEach(() => {
  setActivePinia(createPinia());
  localStorage.clear();
  getThreadMessagesMock.mockReset();
  getThreadedMessagesMock.mockReset();
});

afterEach(() => {
  localStorage.clear();
});

describe("Thread expansion", () => {
  it("treats threads as expanded by default", () => {
    const store = useMessagesStore();
    expect(store.collapsedThreads).toEqual([]);
    expect(store.isThreadExpanded("any-id")).toBe(true);
  });

  it("loads stored collapsed list from localStorage on init", () => {
    localStorage.setItem("chithi-collapsed-threads", JSON.stringify(["t1", "t2"]));
    const store = useMessagesStore();
    expect(store.collapsedThreads).toEqual(["t1", "t2"]);
    expect(store.isThreadExpanded("t1")).toBe(false);
    expect(store.isThreadExpanded("t3")).toBe(true);
  });

  it("toggleThread collapses an expanded thread and persists", async () => {
    const store = useMessagesStore();
    await store.toggleThread("t1");
    expect(store.collapsedThreads).toContain("t1");
    expect(JSON.parse(localStorage.getItem("chithi-collapsed-threads") ?? "[]"))
      .toContain("t1");
  });

  it("toggleThread re-expands a collapsed thread and fetches children", async () => {
    withActiveContext();
    getThreadMessagesMock.mockResolvedValue([
      { id: "t1_0" }, { id: "t1_1" },
    ]);
    const store = useMessagesStore();
    store.collapsedThreads = ["t1"];

    await store.toggleThread("t1");
    expect(store.collapsedThreads).not.toContain("t1");
    expect(getThreadMessagesMock).toHaveBeenCalledWith("acc1", "INBOX", "t1");
    expect(store.threadMessages.t1).toHaveLength(2);
  });

  it("fetchMessages preserves collapsedThreads across syncs", async () => {
    withActiveContext();
    useUiStore().setThreading(true);
    getThreadedMessagesMock.mockResolvedValue({
      threads: [makeThread("t1"), makeThread("t2")],
      total_threads: 2,
      total_messages: 4,
      page: 0,
      per_page: 100,
    });
    getThreadMessagesMock.mockResolvedValue([]);

    const store = useMessagesStore();
    store.collapsedThreads = ["t1"];

    await store.fetchMessages();
    expect(store.collapsedThreads).toEqual(["t1"]);
  });

  it("fetchMessages prefetches children only for expanded threads", async () => {
    withActiveContext();
    useUiStore().setThreading(true);
    getThreadedMessagesMock.mockResolvedValue({
      threads: [makeThread("t1"), makeThread("t2"), makeThread("t3", 1)],
      total_threads: 3,
      total_messages: 5,
      page: 0,
      per_page: 100,
    });
    getThreadMessagesMock.mockImplementation((_acc, _folder, id) => {
      return Promise.resolve([{ id: `${id}_0` }, { id: `${id}_1` }]);
    });

    const store = useMessagesStore();
    store.collapsedThreads = ["t2"];

    await store.fetchMessages();

    // t1 expanded + multi-msg → fetched
    expect(getThreadMessagesMock).toHaveBeenCalledWith("acc1", "INBOX", "t1");
    // t2 collapsed → skipped
    expect(getThreadMessagesMock).not.toHaveBeenCalledWith("acc1", "INBOX", "t2");
    // t3 single message → no fetch needed
    expect(getThreadMessagesMock).not.toHaveBeenCalledWith("acc1", "INBOX", "t3");
  });
});
