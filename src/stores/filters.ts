import { defineStore } from "pinia";
import { ref, watch } from "vue";
import type { FilterRule } from "@/lib/types";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";

export const useFiltersStore = defineStore("filters", () => {
  const filters = ref<FilterRule[]>([]);
  const loading = ref(false);

  const accountsStore = useAccountsStore();

  async function fetchFilters() {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) {
      filters.value = [];
      return;
    }
    loading.value = true;
    try {
      filters.value = await api.listFilters(accountId);
    } finally {
      loading.value = false;
    }
  }

  async function saveFilter(rule: FilterRule) {
    await api.saveFilter(rule);
    await fetchFilters();
  }

  async function deleteFilter(id: string) {
    await api.deleteFilter(id);
    await fetchFilters();
  }

  async function applyToFolder(accountId: string, folderPath: string) {
    return api.applyFiltersToFolder(accountId, folderPath);
  }

  // Refresh filters when account changes
  watch(
    () => accountsStore.activeAccountId,
    () => fetchFilters(),
  );

  return {
    filters,
    loading,
    fetchFilters,
    saveFilter,
    deleteFilter,
    applyToFolder,
  };
});
