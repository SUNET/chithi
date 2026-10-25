import { defineStore } from "pinia";
import { computed, ref } from "vue";
import type { Account, AccountConfig } from "@/lib/types";
import * as api from "@/lib/tauri";

function hasMailBinding(account: Account): boolean {
  return account.mail_protocol !== "";
}

export const useAccountsStore = defineStore("accounts", () => {
  const accounts = ref<Account[]>([]);
  const activeAccountId = ref<string | null>(null);
  const loading = ref(false);

  const activeAccount = () =>
    accounts.value.find((account) => account.id === activeAccountId.value) ??
    null;

  const mailAccounts = computed(() =>
    accounts.value.filter(hasMailBinding),
  );

  // Startup callers share one load so guards cannot observe a partial state.
  let inFlight: Promise<void> | null = null;

  // Restoring a non-mail account would leave the Mail view unusable.
  async function resolveInitialAccountId(): Promise<string> {
    try {
      const lastView = await api.getLastView();
      if (lastView.account_id) {
        const restored = accounts.value.find(
          (account) =>
            account.id === lastView.account_id &&
            account.enabled &&
            hasMailBinding(account),
        );
        if (restored) return restored.id;
      }
    } catch (error) {
      console.error("Failed to load last view:", error);
    }
    const firstEnabledMail = accounts.value.find(
      (account) => hasMailBinding(account) && account.enabled,
    );
    return (firstEnabledMail ?? accounts.value[0]).id;
  }

  async function loadAccounts(): Promise<void> {
    accounts.value = await api.listAccounts();
    if (accounts.value.length > 0 && !activeAccountId.value) {
      activeAccountId.value = await resolveInitialAccountId();
    }
  }

  function fetchAccounts(): Promise<void> {
    if (inFlight) return inFlight;
    loading.value = true;
    inFlight = loadAccounts().finally(() => {
      loading.value = false;
      inFlight = null;
    });
    return inFlight;
  }

  async function addAccount(config: AccountConfig): Promise<string> {
    const id = await api.addAccount(config);
    localStorage.removeItem("chithi-onboarding-skipped");
    await fetchAccounts();
    activeAccountId.value = id;
    void api
      .triggerSync(id)
      .catch((error) => console.error("Initial sync failed:", error));
    return id;
  }

  async function removeAccount(
    id: string,
    remove: () => Promise<void>,
  ): Promise<void> {
    await remove();
    if (activeAccountId.value === id) {
      activeAccountId.value = null;
    }
    await fetchAccounts();
  }

  function deleteAccount(id: string): Promise<void> {
    return removeAccount(id, () => api.deleteAccount(id));
  }

  function abandonZoomAccount(
    id: string,
    confirmation: string,
  ): Promise<void> {
    return removeAccount(id, () => api.abandonZoomAccount(id, confirmation));
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
    abandonZoomAccount,
    setActiveAccount,
  };
});
