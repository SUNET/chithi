import { defineStore } from "pinia";
import { ref, watch } from "vue";
import type { FilterRule } from "@/lib/types";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";

export const useFiltersStore = defineStore("filters", () => {
  const filters = ref<FilterRule[]>([]);
  const accountsStore = useAccountsStore();
  let requestGeneration = 0;

  async function fetchFilters() {
    const request = ++requestGeneration;
    const accountId = accountsStore.activeAccountId;
    if (!accountId) {
      filters.value = [];
      return;
    }
    const nextFilters = await api.listFilters(accountId);
    if (
      request !== requestGeneration ||
      accountsStore.activeAccountId !== accountId
    ) return;
    filters.value = nextFilters;
  }

  async function saveFilter(rule: FilterRule) {
    await api.saveFilter(rule);
    await fetchFilters();
  }

  async function deleteFilter(filterId: string) {
    await api.deleteFilter(filterId);
    await fetchFilters();
  }

  function applyToFolder(accountId: string, folderPath: string) {
    return api.applyFiltersToFolder(accountId, folderPath);
  }

  watch(
    () => accountsStore.activeAccountId,
    () =>
      void fetchFilters().catch((error) =>
        console.error("Failed to refresh filters:", error),
      ),
  );

  return {
    filters,
    fetchFilters,
    saveFilter,
    deleteFilter,
    applyToFolder,
  };
});
