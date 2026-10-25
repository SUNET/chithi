import { expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { nextTick } from "vue";

vi.mock("@/lib/tauri", () => ({
  listFilters: vi.fn(),
}));

import * as api from "@/lib/tauri";
import { useAccountsStore } from "@/stores/accounts";
import { useFiltersStore } from "@/stores/filters";
import type { FilterRule } from "@/lib/types";

it("ignores an outdated account response", async () => {
  setActivePinia(createPinia());
  const resolvers = new Map<string, (filters: FilterRule[]) => void>();
  vi.mocked(api.listFilters).mockImplementation((accountId) =>
    new Promise((resolve) => resolvers.set(accountId ?? "", resolve)),
  );

  const accounts = useAccountsStore();
  accounts.activeAccountId = "account-1";
  const filters = useFiltersStore();
  const firstRequest = filters.fetchFilters();

  accounts.activeAccountId = "account-2";
  await nextTick();
  resolvers.get("account-2")!([{ id: "current" } as FilterRule]);
  await vi.waitFor(() => expect(filters.filters[0]?.id).toBe("current"));

  resolvers.get("account-1")!([{ id: "stale" } as FilterRule]);
  await firstRequest;
  expect(filters.filters[0]?.id).toBe("current");
});
