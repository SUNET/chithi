import { defineStore } from "pinia";
import { ref, watch } from "vue";
import type { MessageSummary, MessageBody } from "@/lib/types";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";
import { useFoldersStore } from "./folders";

export type SortColumn = "subject" | "from" | "date" | "flagged";

export const useMessagesStore = defineStore("messages", () => {
  const messages = ref<MessageSummary[]>([]);
  const activeMessage = ref<MessageBody | null>(null);
  const activeMessageId = ref<string | null>(null);
  const loading = ref(false);
  const loadingBody = ref(false);
  const page = ref(0);
  const total = ref(0);
  const perPage = 50;
  const sortColumn = ref<SortColumn>("date");
  const sortAsc = ref(false);

  const accountsStore = useAccountsStore();
  const foldersStore = useFoldersStore();

  async function fetchMessages(resetPage = true) {
    const accountId = accountsStore.activeAccountId;
    const folderPath = foldersStore.activeFolderPath;
    if (!accountId || !folderPath) {
      messages.value = [];
      return;
    }
    if (resetPage) page.value = 0;
    loading.value = true;
    try {
      const result = await api.getMessages(
        accountId,
        folderPath,
        page.value,
        perPage,
        sortColumn.value,
        sortAsc.value,
      );
      messages.value = result.messages;
      total.value = result.total;
    } finally {
      loading.value = false;
    }
  }

  async function loadMessage(messageId: string) {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) return;
    activeMessageId.value = messageId;
    loadingBody.value = true;
    try {
      activeMessage.value = await api.getMessageBody(accountId, messageId);

      // Mark as read if unread
      const msg = messages.value.find((m) => m.id === messageId);
      if (msg && !msg.flags.includes("seen")) {
        // Update locally first for instant UI feedback
        msg.flags = [...msg.flags, "seen"];
        // Then sync to IMAP in background
        api
          .setMessageFlags(accountId, [messageId], ["seen"], true)
          .catch((e) => console.error("Failed to mark as read:", e));
      }
    } finally {
      loadingBody.value = false;
    }
  }

  async function loadNextPage() {
    page.value++;
    await fetchMessages(false);
  }

  function setSort(column: SortColumn) {
    if (sortColumn.value === column) {
      sortAsc.value = !sortAsc.value;
    } else {
      sortColumn.value = column;
      // Default sort direction per column
      sortAsc.value = column === "subject" || column === "from";
    }
    fetchMessages();
  }

  watch(
    () => foldersStore.activeFolderPath,
    () => {
      activeMessage.value = null;
      activeMessageId.value = null;
      fetchMessages();
    },
  );

  return {
    messages,
    activeMessage,
    activeMessageId,
    loading,
    loadingBody,
    page,
    total,
    perPage,
    sortColumn,
    sortAsc,
    fetchMessages,
    loadMessage,
    loadNextPage,
    setSort,
  };
});
