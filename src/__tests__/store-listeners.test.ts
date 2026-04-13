import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";

const { listenMock } = vi.hoisted(() => ({
  listenMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: listenMock,
}));

vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  listFolders: vi.fn().mockResolvedValue([]),
  triggerSync: vi.fn().mockResolvedValue(undefined),
  getMessages: vi.fn().mockResolvedValue({ messages: [], total: 0, page: 0, per_page: 100 }),
  getThreadedMessages: vi.fn().mockResolvedValue({
    threads: [], total_threads: 0, total_messages: 0, page: 0, per_page: 100,
  }),
  getThreadMessages: vi.fn().mockResolvedValue([]),
  getMessageBody: vi.fn().mockResolvedValue(null),
  setMessageFlags: vi.fn().mockResolvedValue(undefined),
  deleteMessages: vi.fn().mockResolvedValue(undefined),
  backfillThreads: vi.fn().mockResolvedValue(0),
  prefetchBodies: vi.fn().mockResolvedValue(0),
}));

import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useMessagesStore } from "@/stores/messages";

describe("Store listener cleanup", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    listenMock.mockReset();
  });

  it("cleans up the folders listener even if listen resolves after dispose", async () => {
    const accountsStore = useAccountsStore();
    accountsStore.accounts = [];

    let resolveListen: ((unlisten: () => void) => void) | undefined;
    const unlisten = vi.fn();
    listenMock.mockImplementationOnce(
      () => new Promise<() => void>((resolve) => {
        resolveListen = resolve;
      }),
    );

    const foldersStore = useFoldersStore();
    foldersStore.$dispose();

    resolveListen!(unlisten);
    await Promise.resolve();

    expect(unlisten).toHaveBeenCalledOnce();
  });

  it("cleans up only the messages listener when the message store is disposed", async () => {
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

    const foldersUnlisten = vi.fn();
    const messagesUnlisten = vi.fn();
    const opFailedUnlisten = vi.fn();
    listenMock
      .mockImplementationOnce(() => Promise.resolve(foldersUnlisten))
      .mockImplementationOnce(() => Promise.resolve(messagesUnlisten))
      .mockImplementationOnce(() => Promise.resolve(opFailedUnlisten));

    const foldersStore = useFoldersStore();
    foldersStore.activeFolderPath = "INBOX";
    const messagesStore = useMessagesStore();

    await Promise.resolve();
    messagesStore.$dispose();

    expect(messagesUnlisten).toHaveBeenCalledOnce();
    expect(opFailedUnlisten).toHaveBeenCalledOnce();
    expect(foldersUnlisten).not.toHaveBeenCalled();
  });
});