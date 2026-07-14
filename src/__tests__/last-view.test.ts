/**
 * Tests for restoring/persisting the last-viewed account+folder on
 * startup (#191): "Open on Inbox of the first account (or last-viewed
 * folder) on startup".
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";
import { nextTick } from "vue";
import type { Account, Folder } from "@/lib/types";
import { OUTBOX_FOLDER } from "@/lib/types";

vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  addAccount: vi.fn().mockResolvedValue("new-acc"),
  deleteAccount: vi.fn().mockResolvedValue(undefined),
  listFolders: vi.fn().mockResolvedValue([]),
  triggerSync: vi.fn().mockResolvedValue(undefined),
  getLastView: vi.fn().mockResolvedValue({ account_id: null, folder_path: null }),
  saveLastView: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import * as api from "@/lib/tauri";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";

function makeAccount(overrides: Partial<Account> = {}): Account {
  return {
    id: "acc1",
    display_name: "A",
    email: "a@x.com",
    username: "",
    provider: "generic",
    mail_protocol: "imap",
    enabled: true,
    mail_sync_interval_seconds: null,
    calendar_sync_interval_seconds: null,
    contacts_sync_interval_seconds: null,
    has_calendar_binding: false,
    has_contacts_binding: false,
    meet_protocol: "",
    ...overrides,
  };
}

function makeFolder(overrides: Partial<Folder> = {}): Folder {
  return {
    name: "Folder",
    path: "Folder",
    folder_type: null,
    unread_count: 0,
    total_count: 0,
    children: [],
    ...overrides,
  };
}

describe("accounts store: restore last-viewed account on startup (#191)", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.mocked(api.getLastView).mockReset();
    vi.mocked(api.listAccounts).mockReset();
  });

  it("restores the persisted account when it exists, is enabled, and mail-capable", async () => {
    const accA = makeAccount({ id: "acc1", display_name: "A" });
    const accB = makeAccount({ id: "acc2", display_name: "B" });
    vi.mocked(api.listAccounts).mockResolvedValue([accA, accB]);
    vi.mocked(api.getLastView).mockResolvedValue({
      account_id: "acc2",
      folder_path: "Archive",
    });

    const accounts = useAccountsStore();
    await accounts.fetchAccounts();

    expect(accounts.activeAccountId).toBe("acc2");
  });

  it("falls back to the first enabled mail account when the persisted account no longer exists", async () => {
    const accA = makeAccount({ id: "acc1", display_name: "A" });
    vi.mocked(api.listAccounts).mockResolvedValue([accA]);
    vi.mocked(api.getLastView).mockResolvedValue({
      account_id: "deleted-acc",
      folder_path: "Archive",
    });

    const accounts = useAccountsStore();
    await accounts.fetchAccounts();

    expect(accounts.activeAccountId).toBe("acc1");
  });

  it("falls back to the first enabled mail account when the persisted account is disabled", async () => {
    const disabled = makeAccount({ id: "acc1", display_name: "A", enabled: false });
    const enabled = makeAccount({ id: "acc2", display_name: "B", enabled: true });
    vi.mocked(api.listAccounts).mockResolvedValue([disabled, enabled]);
    vi.mocked(api.getLastView).mockResolvedValue({
      account_id: "acc1",
      folder_path: "Archive",
    });

    const accounts = useAccountsStore();
    await accounts.fetchAccounts();

    expect(accounts.activeAccountId).toBe("acc2");
  });

  it("falls back to the first enabled mail account when the persisted account has no mail protocol", async () => {
    // Calendar-/contacts-only account (#43) — FiltersView's account picker
    // shares activeAccountId and lists these too, so a persisted
    // last-view could in principle point at one.
    const calOnly = makeAccount({ id: "acc1", display_name: "Cal", mail_protocol: "" });
    const mailAcc = makeAccount({ id: "acc2", display_name: "Mail" });
    vi.mocked(api.listAccounts).mockResolvedValue([calOnly, mailAcc]);
    vi.mocked(api.getLastView).mockResolvedValue({
      account_id: "acc1",
      folder_path: "Archive",
    });

    const accounts = useAccountsStore();
    await accounts.fetchAccounts();

    expect(accounts.activeAccountId).toBe("acc2");
  });

  it("falls back to accounts[0] when no account is both enabled and mail-capable", async () => {
    const calOnly = makeAccount({ id: "acc1", display_name: "Cal", mail_protocol: "" });
    vi.mocked(api.listAccounts).mockResolvedValue([calOnly]);
    vi.mocked(api.getLastView).mockResolvedValue({
      account_id: null,
      folder_path: null,
    });

    const accounts = useAccountsStore();
    await accounts.fetchAccounts();

    expect(accounts.activeAccountId).toBe("acc1");
  });

  it("does not consult last view when an active account is already set", async () => {
    const accA = makeAccount({ id: "acc1" });
    vi.mocked(api.listAccounts).mockResolvedValue([accA]);
    vi.mocked(api.getLastView).mockResolvedValue({
      account_id: "acc1",
      folder_path: "INBOX",
    });

    const accounts = useAccountsStore();
    accounts.activeAccountId = "acc1";
    await accounts.fetchAccounts();

    expect(api.getLastView).not.toHaveBeenCalled();
  });
});

describe("folders store: restore last-viewed folder on startup (#191)", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.mocked(api.getLastView).mockReset();
    vi.mocked(api.listFolders).mockReset();
    vi.mocked(api.triggerSync).mockReset().mockResolvedValue(undefined);
  });

  it("restores the persisted folder path when it matches the active account and still exists", async () => {
    const accounts = useAccountsStore();
    accounts.accounts = [makeAccount({ id: "acc1" })];
    accounts.activeAccountId = "acc1";

    vi.mocked(api.getLastView).mockResolvedValue({
      account_id: "acc1",
      folder_path: "Archive",
    });
    vi.mocked(api.listFolders).mockResolvedValue([
      makeFolder({ name: "Inbox", path: "INBOX", folder_type: "inbox" }),
      makeFolder({ name: "Archive", path: "Archive" }),
    ]);

    const folders = useFoldersStore();
    await folders.fetchFolders();

    expect(folders.activeFolderPath).toBe("Archive");
  });

  it("falls back to Inbox when the persisted account doesn't match the resolved active account", async () => {
    const accounts = useAccountsStore();
    accounts.accounts = [makeAccount({ id: "acc1" })];
    accounts.activeAccountId = "acc1";

    // Simulates accounts.ts having fallen back to acc1 because the
    // persisted account ("acc2") no longer existed — the persisted
    // folder path is meaningless here.
    vi.mocked(api.getLastView).mockResolvedValue({
      account_id: "acc2",
      folder_path: "Archive",
    });
    vi.mocked(api.listFolders).mockResolvedValue([
      makeFolder({ name: "Inbox", path: "INBOX", folder_type: "inbox" }),
      makeFolder({ name: "Archive", path: "Archive" }),
    ]);

    const folders = useFoldersStore();
    await folders.fetchFolders();

    expect(folders.activeFolderPath).toBe("INBOX");
  });

  it("falls back to Inbox when the persisted folder path no longer exists", async () => {
    const accounts = useAccountsStore();
    accounts.accounts = [makeAccount({ id: "acc1" })];
    accounts.activeAccountId = "acc1";

    vi.mocked(api.getLastView).mockResolvedValue({
      account_id: "acc1",
      folder_path: "Renamed/Away",
    });
    vi.mocked(api.listFolders).mockResolvedValue([
      makeFolder({ name: "Inbox", path: "INBOX", folder_type: "inbox" }),
    ]);

    const folders = useFoldersStore();
    await folders.fetchFolders();

    expect(folders.activeFolderPath).toBe("INBOX");
  });

  it("treats the synthetic Outbox path as always valid", async () => {
    const accounts = useAccountsStore();
    accounts.accounts = [makeAccount({ id: "acc1" })];
    accounts.activeAccountId = "acc1";

    vi.mocked(api.getLastView).mockResolvedValue({
      account_id: "acc1",
      folder_path: OUTBOX_FOLDER,
    });
    vi.mocked(api.listFolders).mockResolvedValue([
      makeFolder({ name: "Inbox", path: "INBOX", folder_type: "inbox" }),
    ]);

    const folders = useFoldersStore();
    await folders.fetchFolders();

    expect(folders.activeFolderPath).toBe(OUTBOX_FOLDER);
  });
});

describe("folders store: debounced persistence of last view (#191)", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.mocked(api.getLastView).mockReset().mockResolvedValue({
      account_id: null,
      folder_path: null,
    });
    vi.mocked(api.listFolders).mockReset().mockResolvedValue([]);
    vi.mocked(api.saveLastView).mockReset().mockResolvedValue(undefined);
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("persists the active account/folder after the debounce window", async () => {
    const accounts = useAccountsStore();
    accounts.accounts = [makeAccount({ id: "acc1" })];
    accounts.activeAccountId = "acc1";

    const folders = useFoldersStore();
    folders.setActiveFolder("INBOX");
    await nextTick();

    expect(api.saveLastView).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(500);

    expect(api.saveLastView).toHaveBeenCalledWith("acc1", "INBOX");
  });

  it("coalesces rapid navigation into a single save call", async () => {
    const accounts = useAccountsStore();
    accounts.accounts = [makeAccount({ id: "acc1" })];
    accounts.activeAccountId = "acc1";

    const folders = useFoldersStore();
    folders.setActiveFolder("INBOX");
    await nextTick();
    await vi.advanceTimersByTimeAsync(200);
    folders.setActiveFolder("Archive");
    await nextTick();
    await vi.advanceTimersByTimeAsync(500);

    expect(api.saveLastView).toHaveBeenCalledOnce();
    expect(api.saveLastView).toHaveBeenCalledWith("acc1", "Archive");
  });

  it("does not persist when the active account has no mail protocol (calendar-only)", async () => {
    const accounts = useAccountsStore();
    accounts.accounts = [
      makeAccount({ id: "cal1", mail_protocol: "" }),
    ];
    accounts.activeAccountId = "cal1";

    const folders = useFoldersStore();
    // A stale folder path left over from a previously active mail
    // account — must not be persisted alongside the calendar-only id.
    folders.activeFolderPath = "INBOX";
    await nextTick();
    await vi.advanceTimersByTimeAsync(500);

    expect(api.saveLastView).not.toHaveBeenCalled();
  });
});
