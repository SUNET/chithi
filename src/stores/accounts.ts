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

  // On startup `fetchAccounts()` is called concurrently from the router
  // guard, App.vue and MailView. Without de-duplication the guard could
  // observe `accounts.length === 0` while another call is still in flight
  // and wrongly redirect to onboarding. Sharing one in-flight promise
  // makes every caller await the same result.
  let inFlight: Promise<void> | null = null;

  // #191: on cold start (no active account yet), prefer the account the
  // user was last viewing over the alphabetically-first one. Falls back
  // to the first enabled account with a mail backend — calendar-/
  // contacts-only accounts (#43) have no folders to show in Mail — and
  // finally to today's plain "first account" behavior if none qualify.
  // The restore is mail-protocol-gated too: FiltersView's account picker
  // (unlike Mail's) lists every account and shares this same
  // activeAccountId, so a calendar-/contacts-only id could in principle
  // end up persisted — restoring into one would leave Mail empty.
  async function resolveInitialAccountId(): Promise<string> {
    try {
      const lastView = await api.getLastView();
      if (lastView.account_id) {
        const restored = accounts.value.find(
          (a) =>
            a.id === lastView.account_id &&
            a.enabled &&
            a.mail_protocol !== "",
        );
        if (restored) return restored.id;
      }
    } catch (e) {
      console.error("Failed to load last view:", e);
    }
    const firstEnabledMail = accounts.value.find(
      (a) => a.mail_protocol !== "" && a.enabled,
    );
    return (firstEnabledMail ?? accounts.value[0]).id;
  }

  async function fetchAccounts(): Promise<void> {
    if (inFlight) return inFlight;
    loading.value = true;
    inFlight = (async () => {
      try {
        accounts.value = await api.listAccounts();
        if (accounts.value.length > 0 && !activeAccountId.value) {
          activeAccountId.value = await resolveInitialAccountId();
        }
      } finally {
        loading.value = false;
        inFlight = null;
      }
    })();
    return inFlight;
  }

  async function addAccount(config: AccountConfig): Promise<string> {
    const id = await api.addAccount(config);
    localStorage.removeItem("chithi-onboarding-skipped");
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
