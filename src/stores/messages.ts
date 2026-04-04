import { defineStore } from "pinia";
import { ref, computed, watch } from "vue";
import type { MessageSummary, MessageBody, ThreadSummary } from "@/lib/types";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";
import { useFoldersStore } from "./folders";
import { useUiStore } from "./ui";

export type SortColumn = "subject" | "from" | "date" | "flagged";

// Message store supports two view modes controlled by `uiStore.threadingEnabled`:
//
// **Flat mode**: `messages` array holds individual MessageSummary items.
//
// **Threaded mode**: `threads` array holds ThreadSummary items (one per thread,
// grouped by thread_id within the current folder). Expanding a thread populates
// `threadMessages` map with the individual messages in that thread.
// Thread expansion only shows messages from the current folder to avoid
// Gmail duplicate-label issues.
//
// Both modes support infinite scroll (loadNextPage), sorting, and mark-as-read.
// Switching between modes triggers a full re-fetch.
export const useMessagesStore = defineStore("messages", () => {
  // Flat mode state
  const messages = ref<MessageSummary[]>([]);
  // Threaded mode state
  const threads = ref<ThreadSummary[]>([]);
  const expandedThreads = ref<Set<string>>(new Set());
  const threadMessages = ref<Map<string, MessageSummary[]>>(new Map());

  const activeMessage = ref<MessageBody | null>(null);
  const activeMessageId = ref<string | null>(null);
  const loading = ref(false);
  const loadingMore = ref(false);
  const loadingBody = ref(false);
  const page = ref(0);
  const total = ref(0);
  const totalThreads = ref(0);
  const perPage = 100;
  const sortColumn = ref<SortColumn>("date");
  const sortAsc = ref(false);

  const accountsStore = useAccountsStore();
  const foldersStore = useFoldersStore();
  const uiStore = useUiStore();

  const hasMore = computed(() => {
    if (uiStore.threadingEnabled) {
      return threads.value.length < totalThreads.value;
    }
    return messages.value.length < total.value;
  });

  async function fetchMessages(resetPage = true) {
    const accountId = accountsStore.activeAccountId;
    const folderPath = foldersStore.activeFolderPath;
    if (!accountId || !folderPath) {
      messages.value = [];
      threads.value = [];
      return;
    }
    if (resetPage) {
      page.value = 0;
      messages.value = [];
      threads.value = [];
      expandedThreads.value = new Set();
      threadMessages.value = new Map();
    }
    loading.value = true;
    try {
      if (uiStore.threadingEnabled) {
        const result = await api.getThreadedMessages(
          accountId,
          folderPath,
          page.value,
          perPage,
          sortColumn.value,
          sortAsc.value,
        );
        if (resetPage) {
          threads.value = result.threads;
        } else {
          threads.value = [...threads.value, ...result.threads];
        }
        totalThreads.value = result.total_threads;
        total.value = result.total_messages;
      } else {
        const result = await api.getMessages(
          accountId,
          folderPath,
          page.value,
          perPage,
          sortColumn.value,
          sortAsc.value,
        );
        if (resetPage) {
          messages.value = result.messages;
        } else {
          messages.value = [...messages.value, ...result.messages];
        }
        total.value = result.total;
      }
    } finally {
      loading.value = false;
    }
  }

  async function loadNextPage() {
    if (loadingMore.value || !hasMore.value) return;
    loadingMore.value = true;
    page.value++;
    try {
      const accountId = accountsStore.activeAccountId;
      const folderPath = foldersStore.activeFolderPath;
      if (!accountId || !folderPath) return;

      if (uiStore.threadingEnabled) {
        const result = await api.getThreadedMessages(
          accountId,
          folderPath,
          page.value,
          perPage,
          sortColumn.value,
          sortAsc.value,
        );
        threads.value = [...threads.value, ...result.threads];
        totalThreads.value = result.total_threads;
        total.value = result.total_messages;
      } else {
        const result = await api.getMessages(
          accountId,
          folderPath,
          page.value,
          perPage,
          sortColumn.value,
          sortAsc.value,
        );
        messages.value = [...messages.value, ...result.messages];
        total.value = result.total;
      }
    } finally {
      loadingMore.value = false;
    }
  }

  async function toggleThread(threadId: string) {
    if (expandedThreads.value.has(threadId)) {
      expandedThreads.value.delete(threadId);
      expandedThreads.value = new Set(expandedThreads.value);
    } else {
      // Fetch thread messages if not cached
      if (!threadMessages.value.has(threadId)) {
        const accountId = accountsStore.activeAccountId;
        const folderPath = foldersStore.activeFolderPath;
        if (!accountId || !folderPath) return;
        const msgs = await api.getThreadMessages(accountId, folderPath, threadId);
        threadMessages.value.set(threadId, msgs);
        threadMessages.value = new Map(threadMessages.value);
      }
      expandedThreads.value.add(threadId);
      expandedThreads.value = new Set(expandedThreads.value);
    }
  }

  async function showAsThread(messageId: string) {
    // Find thread_id for this message from the flat list
    const msg = messages.value.find((m) => m.id === messageId);
    if (!msg) return;
    // Fetch thread messages for display
    const accountId = accountsStore.activeAccountId;
    const folderPath = foldersStore.activeFolderPath;
    if (!accountId || !folderPath) return;
    const threadMsgs = await api.getThreadMessages(accountId, folderPath, messageId);
    if (threadMsgs.length > 1) {
      // Switch to threaded view temporarily for this thread
      threadMessages.value.set(messageId, threadMsgs);
      threadMessages.value = new Map(threadMessages.value);
      expandedThreads.value.add(messageId);
      expandedThreads.value = new Set(expandedThreads.value);
    }
  }

  async function unthreadMessage(messageId: string) {
    await api.unthreadMessage(messageId);
    await fetchMessages();
  }

  async function loadMessage(messageId: string) {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) return;
    activeMessageId.value = messageId;
    loadingBody.value = true;
    try {
      activeMessage.value = await api.getMessageBody(accountId, messageId);

      // Find the message in flat list or expanded thread messages and mark as read.
      // In threaded mode, messages live inside threadMessages map, not the flat array.
      // Also decrement the parent thread's unread_count for instant UI update.
      let msg = messages.value.find((m) => m.id === messageId);
      if (!msg) {
        for (const msgs of threadMessages.value.values()) {
          msg = msgs.find((m) => m.id === messageId);
          if (msg) break;
        }
      }
      if (msg && !msg.flags.includes("seen")) {
        msg.flags = [...msg.flags, "seen"];
        // Also update the thread summary's unread count
        const thread = threads.value.find((t) =>
          t.message_ids.includes(messageId),
        );
        if (thread && thread.unread_count > 0) {
          thread.unread_count--;
        }
        api
          .setMessageFlags(accountId, [messageId], ["seen"], true)
          .catch((e) => console.error("Failed to mark as read:", e));
      }
    } finally {
      loadingBody.value = false;
    }
  }

  function setSort(column: SortColumn) {
    if (sortColumn.value === column) {
      sortAsc.value = !sortAsc.value;
    } else {
      sortColumn.value = column;
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

  // Re-fetch when threading mode changes
  watch(
    () => uiStore.threadingEnabled,
    () => {
      fetchMessages();
    },
  );

  return {
    messages,
    threads,
    expandedThreads,
    threadMessages,
    activeMessage,
    activeMessageId,
    loading,
    loadingMore,
    loadingBody,
    page,
    total,
    totalThreads,
    perPage,
    sortColumn,
    sortAsc,
    hasMore,
    fetchMessages,
    loadMessage,
    loadNextPage,
    toggleThread,
    showAsThread,
    unthreadMessage,
    setSort,
  };
});
