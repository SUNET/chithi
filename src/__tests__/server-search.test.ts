import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

const { listenMock } = vi.hoisted(() => ({
  listenMock: vi.fn(() => Promise.resolve(() => {})),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: listenMock,
}));

vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  listFolders: vi.fn().mockResolvedValue([]),
  triggerSync: vi.fn().mockResolvedValue(undefined),
  getMessages: vi
    .fn()
    .mockResolvedValue({ messages: [], total: 0, page: 0, per_page: 100 }),
  getThreadedMessages: vi.fn().mockResolvedValue({
    threads: [],
    total_threads: 0,
    total_messages: 0,
    page: 0,
    per_page: 100,
  }),
  getThreadMessages: vi.fn().mockResolvedValue([]),
  getMessageBody: vi.fn().mockResolvedValue(null),
  setMessageFlags: vi.fn().mockResolvedValue(undefined),
  deleteMessages: vi.fn().mockResolvedValue(undefined),
  backfillThreads: vi.fn().mockResolvedValue(0),
  prefetchBodies: vi.fn().mockResolvedValue(0),
  searchMessagesServer: vi.fn(),
}));

import * as api from "@/lib/tauri";
import { useAccountsStore } from "@/stores/accounts";
import { useMessagesStore } from "@/stores/messages";
import QuickFilterBar from "@/components/mail/QuickFilterBar.vue";

const mockedSearch = vi.mocked(api.searchMessagesServer);

function withActiveAccount() {
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
}

describe("server-side search", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    listenMock.mockClear();
    mockedSearch.mockReset();
  });

  it("hides the trigger when filter text is empty", () => {
    const wrapper = mount(QuickFilterBar);
    expect(wrapper.find('[data-testid="search-server-trigger"]').exists()).toBe(
      false,
    );
  });

  it("shows the trigger when filter text is non-empty", async () => {
    const messagesStore = useMessagesStore();
    messagesStore.quickFilterText = "report";
    const wrapper = mount(QuickFilterBar);
    expect(wrapper.find('[data-testid="search-server-trigger"]').exists()).toBe(
      true,
    );
    expect(wrapper.text()).toContain("Search server for");
    expect(wrapper.text()).toContain("report");
  });

  it("dispatches a SearchQuery built from filter text + field toggles", async () => {
    withActiveAccount();
    mockedSearch.mockResolvedValueOnce([]);

    const messagesStore = useMessagesStore();
    messagesStore.quickFilterText = "invoice";
    messagesStore.quickFilterFields = ["subject"];

    const wrapper = mount(QuickFilterBar);
    await wrapper.find('[data-testid="search-server-trigger"]').trigger("click");
    await Promise.resolve();

    expect(mockedSearch).toHaveBeenCalledTimes(1);
    const [accountId, query] = mockedSearch.mock.calls[0];
    expect(accountId).toBe("acc1");
    expect(query.text).toBe("invoice");
    expect(query.fields).toEqual({
      subject: true,
      from: false,
      to: false,
      body: false,
    });
  });

  it("uses all fields when no field toggle has been set", async () => {
    withActiveAccount();
    mockedSearch.mockResolvedValueOnce([]);

    const messagesStore = useMessagesStore();
    messagesStore.quickFilterText = "invoice";
    messagesStore.quickFilterFields = [];

    await messagesStore.runServerSearch();
    expect(mockedSearch.mock.calls[0][1].fields).toEqual({
      subject: true,
      from: true,
      to: true,
      body: true,
    });
  });

  it("populates serverHits on a successful search", async () => {
    withActiveAccount();
    mockedSearch.mockResolvedValueOnce([
      {
        account_id: "acc1",
        folder_path: "INBOX",
        uid: 42,
        message_id: "<x@y>",
        backend_id: "INBOX:42",
        subject: "Hi there",
        from_name: "Alice",
        from_email: "alice@example.com",
        date: 1_700_000_000,
        snippet: "preview text",
      },
    ]);

    const messagesStore = useMessagesStore();
    messagesStore.quickFilterText = "hi";
    await messagesStore.runServerSearch();

    expect(messagesStore.serverHits).toHaveLength(1);
    expect(messagesStore.serverSearchLoading).toBe(false);
    expect(messagesStore.serverSearchError).toBeNull();
  });

  it("captures errors and clears hits", async () => {
    withActiveAccount();
    mockedSearch.mockRejectedValueOnce(new Error("timeout"));

    const messagesStore = useMessagesStore();
    messagesStore.quickFilterText = "hi";
    await messagesStore.runServerSearch();

    expect(messagesStore.serverHits).toHaveLength(0);
    expect(messagesStore.serverSearchError).toContain("timeout");
  });

  it("clears prior hits as soon as the user types again", async () => {
    withActiveAccount();
    const messagesStore = useMessagesStore();
    messagesStore.serverHits = [
      {
        account_id: "acc1",
        folder_path: "INBOX",
        uid: null,
        message_id: null,
        backend_id: "x",
        subject: "old",
        from_name: null,
        from_email: null,
        date: 0,
        snippet: null,
      },
    ];
    messagesStore.serverSearchError = "stale";

    messagesStore.onFilterTextChange();
    expect(messagesStore.serverHits).toHaveLength(0);
    expect(messagesStore.serverSearchError).toBeNull();
  });
});
