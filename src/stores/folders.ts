import { defineStore } from "pinia";
import { ref, watch, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import { OUTBOX_FOLDER, type Folder } from "@/lib/types";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";

export const useFoldersStore = defineStore("folders", () => {
  const folders = ref<Folder[]>([]);
  const foldersByAccount = ref<Record<string, Folder[]>>({});
  const activeFolderPath = ref<string | null>(null);
  const loading = ref(false);

  const accountsStore = useAccountsStore();

  function findFolderInTree(folders: Folder[], path: string): Folder | undefined {
    for (const folder of folders) {
      if (folder.path === path) return folder;
      const found = findFolderInTree(folder.children, path);
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

      // Restore only a folder saved for the active account.
      if (activeFolderPath.value === null) {
        try {
          const lastView = await api.getLastView();
          if (lastView.account_id === accountId && lastView.folder_path) {
            activeFolderPath.value = lastView.folder_path;
          }
        } catch (error) {
          console.error("Failed to load last view:", error);
        }
      }

      if (folders.value.length > 0) {
        // Outbox is synthetic and therefore absent from the folder list.
        const activePath = activeFolderPath.value;
        const currentValid = activePath !== null &&
          (activePath === OUTBOX_FOLDER || findFolderInTree(folders.value, activePath) !== undefined);
        if (!currentValid) {
          const inbox = folders.value.find(
            (folder) => folder.folder_type === "inbox",
          );
          activeFolderPath.value = inbox?.path ?? folders.value[0].path;
        }
      }

      if (folders.value.length === 0) {
        void api.triggerSync(accountId).catch((error) =>
          console.error("Initial sync failed:", error),
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
        if (account.id === accountsStore.activeAccountId) {
          folders.value = accountFolders;
        }
      } catch (error) {
        console.error("Failed to fetch folders for", account.id, error);
      }
    }
  }

  function setActiveFolder(path: string) {
    activeFolderPath.value = path;
  }

  function getAccountFolders(accountId: string): Folder[] {
    return foldersByAccount.value[accountId] ?? [];
  }

  function flattenFolders(tree: Folder[]): Folder[] {
    const result: Folder[] = [];
    for (const folder of tree) {
      result.push(folder);
      if (folder.children.length > 0) {
        result.push(...flattenFolders(folder.children));
      }
    }
    return result;
  }

  function getFlatFolders(): Folder[] {
    return flattenFolders(folders.value);
  }

  watch(
    () => accountsStore.activeAccountId,
    () => {
      void fetchFolders();
    },
  );

  // Non-mail accounts must not overwrite the last persisted mail view.
  let saveViewTimer: ReturnType<typeof setTimeout> | null = null;
  watch(
    () => [accountsStore.activeAccountId, activeFolderPath.value] as const,
    ([accountId, folderPath]) => {
      if (!accountId || !folderPath) return;
      const account = accountsStore.accounts.find(
        (candidate) => candidate.id === accountId,
      );
      if (!account || account.mail_protocol === "") return;
      if (saveViewTimer) clearTimeout(saveViewTimer);
      saveViewTimer = setTimeout(() => {
        saveViewTimer = null;
        void api.saveLastView(accountId, folderPath).catch((error) =>
          console.error("Failed to save last view:", error),
        );
      }, 500);
    },
  );

  let foldersRefreshTimer: ReturnType<typeof setTimeout> | null = null;
  let stopFoldersListener: null | (() => void) = null;
  let disposed = false;
  void listen<string>("folders-changed", () => {
    if (disposed) return;
    if (foldersRefreshTimer) clearTimeout(foldersRefreshTimer);
    foldersRefreshTimer = setTimeout(() => {
      void fetchAllAccountFolders();
      void fetchFolders();
    }, 200);
  })
    .then((unlisten) => {
      if (disposed) {
        unlisten();
        return;
      }
      stopFoldersListener = unlisten;
    })
    .catch((error) => {
      console.error("Failed to subscribe to folders-changed:", error);
    });

  onScopeDispose(() => {
    disposed = true;
    if (foldersRefreshTimer) clearTimeout(foldersRefreshTimer);
    if (saveViewTimer) clearTimeout(saveViewTimer);
    stopFoldersListener?.();
  });

  return {
    folders,
    foldersByAccount,
    activeFolderPath,
    loading,
    fetchFolders,
    fetchAllAccountFolders,
    setActiveFolder,
    getAccountFolders,
    getFlatFolders,
  };
});
