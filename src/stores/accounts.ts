import { defineStore } from "pinia";
import { computed, ref } from "vue";
import type { Account, AccountConfig } from "@/lib/types";
import * as api from "@/lib/tauri";

export const useAccountsStore = defineStore("accounts", () => {
  const accounts = ref<Account[]>([]);
  const activeAccountId = ref<string | null>(null);
  const loading = ref(false);

  const activeAccount = () =>
    accounts.value.find((a) => a.id === activeAccountId.value) ?? null;

  // Phase 4 (#43): standalone CalDAV / CardDAV / JMAP-cal-only accounts
  // surface with `mail_protocol === ""`. Mail screens iterate this
  // getter so they don't try to list folders for an account that has no
  // mail backend.
  const mailAccounts = computed(() =>
    accounts.value.filter((a) => a.mail_protocol !== ""),
  );

  async function fetchAccounts() {
    loading.value = true;
    try {
      accounts.value = await api.listAccounts();
      if (accounts.value.length > 0 && !activeAccountId.value) {
        activeAccountId.value = accounts.value[0].id;
      }
    } finally {
      loading.value = false;
    }
  }

  async function addAccount(config: AccountConfig): Promise<string> {
    const id = await api.addAccount(config);
    await fetchAccounts();
    activeAccountId.value = id;
    // Auto-trigger sync for the new account (fire and forget)
    api.triggerSync(id).catch((e) => console.error("Initial sync failed:", e));
    return id;
  }

  async function deleteAccount(id: string) {
    await api.deleteAccount(id);
    if (activeAccountId.value === id) {
      activeAccountId.value = null;
    }
    await fetchAccounts();
  }

  function setActiveAccount(id: string) {
    activeAccountId.value = id;
  }

  return {
    accounts,
    activeAccountId,
    loading,
    activeAccount,
    mailAccounts,
    fetchAccounts,
    addAccount,
    deleteAccount,
    setActiveAccount,
  };
});
