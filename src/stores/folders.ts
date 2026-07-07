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

      // #191: on cold start (nothing selected yet), try restoring the
      // folder the user was last viewing — but only if it was saved for
      // *this* account. If accounts.ts had to fall back to a different
      // account (the saved one no longer exists/is disabled), the saved
      // folder path is meaningless here and we fall through to the
      // Inbox-default logic below instead.
      if (activeFolderPath.value === null) {
        try {
          const lastView = await api.getLastView();
          if (lastView.account_id === accountId && lastView.folder_path) {
            activeFolderPath.value = lastView.folder_path;
          }
        } catch (e) {
          console.error("Failed to load last view:", e);
        }
      }

      if (folders.value.length > 0) {
        // If no folder is selected, or the selected folder doesn't exist
        // in this account, default to Inbox. The synthetic Outbox path
        // (never a row in `folders`) is always considered valid.
        const active = activeFolderPath.value;
        const currentValid = active !== null &&
          (active === OUTBOX_FOLDER || findFolderInTree(folders.value, active) !== undefined);
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

  /** Flatten a folder tree into a flat list (for dropdowns, filters, move targets). */
  function flattenFolders(tree: Folder[]): Folder[] {
    const result: Folder[] = [];
    for (const f of tree) {
      result.push(f);
      if (f.children.length > 0) {
        result.push(...flattenFolders(f.children));
      }
    }
    return result;
  }

  /** All folders for the active account as a flat list (for dropdowns/selects). */
  function getFlatFolders(): Folder[] {
    return flattenFolders(folders.value);
  }

  watch(
    () => accountsStore.activeAccountId,
    () => {
      fetchFolders();
    },
  );

  // #191: persist the active account/folder (debounced) so the next
  // startup can restore this view. Skipped while either half is unset
  // (cold start, or an account with no folders yet), and while the
  // active account isn't mail-capable: FiltersView's account picker
  // (unlike Mail's) lists every account and shares this same
  // activeAccountId, so switching to a calendar-/contacts-only account
  // there must not overwrite the last *mail* view with a folder path
  // that belongs to a different, unrelated account.
  let saveViewTimer: ReturnType<typeof setTimeout> | null = null;
  watch(
    () => [accountsStore.activeAccountId, activeFolderPath.value] as const,
    ([accountId, folderPath]) => {
      if (!accountId || !folderPath) return;
      const account = accountsStore.accounts.find((a) => a.id === accountId);
      if (!account || account.mail_protocol === "") return;
      if (saveViewTimer) clearTimeout(saveViewTimer);
      saveViewTimer = setTimeout(() => {
        api.saveLastView(accountId, folderPath).catch((e) =>
          console.error("Failed to save last view:", e),
        );
      }, 500);
    },
  );

  // Subscribe to backend folder-change events with debounce
  let foldersRefreshTimer: ReturnType<typeof setTimeout> | null = null;
  let stopFoldersListener: null | (() => void) = null;
  let disposed = false;
  void listen<string>("folders-changed", () => {
    if (disposed) return;
    if (foldersRefreshTimer) clearTimeout(foldersRefreshTimer);
    foldersRefreshTimer = setTimeout(() => {
      fetchAllAccountFolders();
      fetchFolders();
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
