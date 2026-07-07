import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@/lib/tauri", () => ({
  listFilters: vi.fn().mockResolvedValue([]),
  saveFilter: vi.fn().mockResolvedValue(undefined),
  deleteFilter: vi.fn().mockResolvedValue(undefined),
  applyFiltersToFolder: vi.fn().mockResolvedValue(0),
  listFolders: vi.fn().mockResolvedValue([
    { name: "Inbox", path: "INBOX", folder_type: "inbox", unread_count: 0, total_count: 0, children: [] },
  ]),
  triggerSync: vi.fn().mockResolvedValue(undefined),
  getLastView: vi.fn().mockResolvedValue({ account_id: null, folder_path: null }),
  saveLastView: vi.fn().mockResolvedValue(undefined),
}));

import * as api from "@/lib/tauri";
import FiltersView from "@/views/FiltersView.vue";
import { useAccountsStore } from "@/stores/accounts";

function setupActiveAccount() {
  const accountsStore = useAccountsStore();
  accountsStore.accounts = [
    {
      id: "acc1",
      display_name: "Test",
      email: "test@example.com",
      provider: "generic",
      mail_protocol: "imap" as const,
      enabled: true,
      mail_sync_interval_seconds: null,
      calendar_sync_interval_seconds: null,
      contacts_sync_interval_seconds: null,
      username: "",
      has_calendar_binding: false,
      has_contacts_binding: false,
      meet_protocol: "",
    },
  ];
  accountsStore.activeAccountId = "acc1";
}

describe("FiltersView", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    vi.mocked(api.saveFilter).mockClear();
    setupActiveAccount();
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("offers and saves the combined To/CC condition field", async () => {
    const wrapper = mount(FiltersView, { attachTo: document.body });

    await wrapper.find('[data-testid="filter-new-btn"]').trigger("click");
    await wrapper.find(".cond-field .select-trigger").trigger("click");

    const labels = wrapper.findAll(".cond-field .select-option").map((option) => option.text());
    expect(labels).toContain("To/CC");

    const toCcOption = wrapper.findAll(".cond-field .select-option").find((option) => option.text() === "To/CC");
    expect(toCcOption).toBeDefined();
    await toCcOption!.trigger("mousedown");

    await wrapper.find(".cond-value").setValue("team@example.com");
    await wrapper.find('[data-testid="filter-save-btn"]').trigger("click");
    await Promise.resolve();

    expect(api.saveFilter).toHaveBeenCalledTimes(1);
    expect(vi.mocked(api.saveFilter).mock.calls[0][0].conditions[0]).toEqual({
      field: "to_cc",
      op: "contains",
      value: "team@example.com",
    });
  });
});
