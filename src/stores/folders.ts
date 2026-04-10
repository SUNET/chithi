import { defineStore } from "pinia";
import { ref, watch } from "vue";
import type { Folder } from "@/lib/types";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";

export const useFoldersStore = defineStore("folders", () => {
  const folders = ref<Folder[]>([]);
  const foldersByAccount = ref<Record<string, Folder[]>>({});
  const activeFolderPath = ref<string | null>(null);
  const loading = ref(false);

  const accountsStore = useAccountsStore();

  function findFolderInTree(folders: Folder[], path: string): Folder | undefined {
    for (const f of folders) {
      if (f.path === path) return f;
      const found = findFolderInTree(f.children, path);
      if (found) return found;
    }
    return undefined;
  }

  async function fetchFolders() {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) {
      folders.value = [];
      return;
    }
    loading.value = true;
    try {
      folders.value = await api.listFolders(accountId);
      foldersByAccount.value = { ...foldersByAccount.value, [accountId]: folders.value };
      if (folders.value.length > 0) {
        // If no folder is selected, or the selected folder doesn't exist
        // in this account, default to Inbox.
        const currentValid = activeFolderPath.value &&
          findFolderInTree(folders.value, activeFolderPath.value);
        if (!currentValid) {
          const inbox = folders.value.find((f) => f.folder_type === "inbox");
          activeFolderPath.value = inbox?.path ?? folders.value[0].path;
        }
      }

      if (folders.value.length === 0) {
        api.triggerSync(accountId).catch((e) =>
          console.error("Initial sync failed:", e),
        );
      }
    } finally {
      loading.value = false;
    }
  }

  async function fetchAllAccountFolders() {
    for (const account of accountsStore.accounts) {
      try {
        const accountFolders = await api.listFolders(account.id);
        foldersByAccount.value = {
          ...foldersByAccount.value,
          [account.id]: accountFolders,
        };
        // Keep the active account's folders in sync
        if (account.id === accountsStore.activeAccountId) {
          folders.value = accountFolders;
        }
      } catch (e) {
        console.error("Failed to fetch folders for", account.id, e);
      }
    }
  }

  function setActiveFolder(path: string) {
    activeFolderPath.value = path;
  }

  function getAccountFolders(accountId: string): Folder[] {
    return foldersByAccount.value[accountId] ?? [];
  }

  watch(
    () => accountsStore.activeAccountId,
    () => {
      fetchFolders();
    },
  );

  return {
    folders,
    foldersByAccount,
    activeFolderPath,
    loading,
    fetchFolders,
    fetchAllAccountFolders,
    setActiveFolder,
    getAccountFolders,
  };
});
