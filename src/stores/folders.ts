import { defineStore } from "pinia";
import { ref, watch } from "vue";
import type { Folder } from "@/lib/types";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";

export const useFoldersStore = defineStore("folders", () => {
  const folders = ref<Folder[]>([]);
  const activeFolderPath = ref<string | null>(null);
  const loading = ref(false);

  const accountsStore = useAccountsStore();

  async function fetchFolders() {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) {
      folders.value = [];
      return;
    }
    loading.value = true;
    try {
      folders.value = await api.listFolders(accountId);
      if (folders.value.length > 0 && !activeFolderPath.value) {
        const inbox = folders.value.find((f) => f.folder_type === "inbox");
        activeFolderPath.value = inbox?.path ?? folders.value[0].path;
      }
    } finally {
      loading.value = false;
    }
  }

  function setActiveFolder(path: string) {
    activeFolderPath.value = path;
  }

  watch(
    () => accountsStore.activeAccountId,
    () => {
      activeFolderPath.value = null;
      fetchFolders();
    },
  );

  return {
    folders,
    activeFolderPath,
    loading,
    fetchFolders,
    setActiveFolder,
  };
});
