import { defineStore } from "pinia";
import { ref, computed, watch, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import type { MessageSummary, MessageBody, ThreadSummary, QuickFilter } from "@/lib/types";
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
//
// Selection uses a plain string array (not Set) because Vue's reactivity system
// reliably tracks array mutations but can miss Set changes.
export const useMessagesStore = defineStore("messages", () => {
  // Flat mode state
  const messages = ref<MessageSummary[]>([]);
  // Threaded mode state
  const threads = ref<ThreadSummary[]>([]);
  const expandedThreads = ref<string[]>([]);
  const threadMessages = ref<Record<string, MessageSummary[]>>({});

  const activeMessage = ref<MessageBody | null>(null);
  const activeMessageId = ref<string | null>(null);
  // Multi-select: array of selected message IDs.
  // Uses array instead of Set for Vue reactivity compatibility.
  const selectedIds = ref<string[]>([]);
  // Tracks the last clicked message ID for Shift+click range selection.
  const lastClickedId = ref<string | null>(null);
  const loading = ref(false);
  const loadingMore = ref(false);
  const loadingBody = ref(false);
  const page = ref(0);
  const total = ref(0);
  const totalThreads = ref(0);
  const perPage = 100;
  const sortColumn = ref<SortColumn>("date");
  const sortAsc = ref(false);
  const quickFilter = ref<QuickFilter>({});
  const quickFilterText = ref("");
  const quickFilterVisible = ref(false);
  const quickFilterFields = ref<string[]>([]); // empty = all fields

  const accountsStore = useAccountsStore();
  const foldersStore = useFoldersStore();
  const uiStore = useUiStore();

  const hasMore = computed(() => {
    if (uiStore.threadingEnabled) {
      return threads.value.length < totalThreads.value;
    }
    return messages.value.length < total.value;
  });

  const hasActiveFilter = computed(() => {
    const f = quickFilter.value;
    return !!(f.unread || f.starred || f.has_attachment || f.contact || quickFilterText.value.trim());
  });

  const activeFilterForApi = computed((): QuickFilter | undefined => {
    if (!hasActiveFilter.value) return undefined;
    const f = { ...quickFilter.value };
    const text = quickFilterText.value.trim();
    if (text) {
      f.text = text;
      if (quickFilterFields.value.length > 0) {
        f.text_fields = quickFilterFields.value;
      }
    }
    return f;
  });

  function toggleQuickFilter(key: "unread" | "starred" | "has_attachment" | "contact") {
    quickFilter.value = { ...quickFilter.value, [key]: !quickFilter.value[key] };
    fetchMessages(true);
  }

  function toggleTextField(field: string) {
    const fields = [...quickFilterFields.value];
    const idx = fields.indexOf(field);
    if (idx !== -1) {
      fields.splice(idx, 1);
    } else {
      fields.push(field);
    }
    quickFilterFields.value = fields;
    if (quickFilterText.value.trim()) {
      fetchMessages(true);
    }
  }

  // Debounced text search — fetch from backend after 300ms of no typing
  let textSearchTimer: ReturnType<typeof setTimeout> | null = null;
  function onFilterTextChange() {
    if (textSearchTimer) clearTimeout(textSearchTimer);
    textSearchTimer = setTimeout(() => {
      fetchMessages(true);
    }, 300);
  }

  function clearQuickFilters() {
    quickFilter.value = {};
    quickFilterText.value = "";
    quickFilterFields.value = [];
    if (textSearchTimer) clearTimeout(textSearchTimer);
    fetchMessages(true);
  }

  // Computed for quick lookup
  const selectedSet = computed(() => new Set(selectedIds.value));

  function isSelected(id: string): boolean {
    return selectedIds.value.includes(id);
  }

  async function fetchMessages(resetPage = true) {
    const accountId = accountsStore.activeAccountId;
    const folderPath = foldersStore.activeFolderPath;
    if (!accountId || !folderPath) {
      messages.value = [];
      threads.value = [];
      total.value = 0;
      totalThreads.value = 0;
      return;
    }
    if (resetPage) {
      page.value = 0;
      // Don't clear messages/threads here — causes UI flicker.
      // They get replaced atomically after the API call returns.
      expandedThreads.value = [];
      threadMessages.value = {};
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
          activeFilterForApi.value,
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
          activeFilterForApi.value,
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
          activeFilterForApi.value,
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
          activeFilterForApi.value,
        );
        messages.value = [...messages.value, ...result.messages];
        total.value = result.total;
      }
    } finally {
      loadingMore.value = false;
    }
  }

  async function toggleThread(threadId: string) {
    const idx = expandedThreads.value.indexOf(threadId);
    if (idx !== -1) {
      expandedThreads.value.splice(idx, 1);
    } else {
      if (!threadMessages.value[threadId]) {
        const accountId = accountsStore.activeAccountId;
        const folderPath = foldersStore.activeFolderPath;
        if (!accountId || !folderPath) return;
        const msgs = await api.getThreadMessages(accountId, folderPath, threadId);
        threadMessages.value = { ...threadMessages.value, [threadId]: msgs };
      }
      expandedThreads.value.push(threadId);
    }
  }

  async function showAsThread(messageId: string) {
    const msg = messages.value.find((m) => m.id === messageId);
    if (!msg) return;
    const accountId = accountsStore.activeAccountId;
    const folderPath = foldersStore.activeFolderPath;
    if (!accountId || !folderPath) return;
    const threadMsgs = await api.getThreadMessages(accountId, folderPath, messageId);
    if (threadMsgs.length > 1) {
      threadMessages.value = { ...threadMessages.value, [messageId]: threadMsgs };
      if (!expandedThreads.value.includes(messageId)) {
        expandedThreads.value.push(messageId);
      }
    }
  }

  async function unthreadMessage(messageId: string) {
    await api.unthreadMessage(messageId);
    await fetchMessages();
  }

  // Find a message in flat list or expanded threads
  function findMessage(messageId: string): MessageSummary | undefined {
    let msg = messages.value.find((m) => m.id === messageId);
    if (!msg) {
      for (const msgs of Object.values(threadMessages.value)) {
        msg = msgs.find((m) => m.id === messageId);
        if (msg) break;
      }
    }
    return msg;
  }

  // Mark a message as read — updates local state immediately,
  // then syncs to IMAP in the background.
  // Handles messages in flat list, expanded threads, AND thread summaries
  // (where the message object may not be loaded yet).
  function markAsRead(messageId: string) {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) return;

    // Try to find and update the message object in flat list or expanded threads
    const msg = findMessage(messageId);
    if (msg) {
      if (msg.flags.includes("seen")) return; // already read
      msg.flags = [...msg.flags, "seen"];
    }

    // Update thread summary's unread count (works even if msg not found)
    const thread = threads.value.find((t) =>
      t.message_ids.includes(messageId),
    );
    if (thread && thread.unread_count > 0) {
      thread.unread_count--;
    }

    // Always send IMAP flag update — even if we couldn't find the local
    // message object (e.g., threading mode with collapsed thread), the
    // server still needs to be told.
    api
      .setMessageFlags(accountId, [messageId], ["seen"], true)
      .catch((e) => console.error("Failed to mark as read:", e));
  }

  async function loadMessage(messageId: string) {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) return;
    activeMessageId.value = messageId;
    loadingBody.value = true;
    try {
      activeMessage.value = await api.getMessageBody(accountId, messageId);
    } catch (e) {
      console.error("Failed to load message body:", e);
    } finally {
      loadingBody.value = false;
    }
  }

  // Get ordered list of all visible message IDs for Shift+click range.
  function getVisibleIds(): string[] {
    if (uiStore.threadingEnabled) {
      const ids: string[] = [];
      for (const thread of threads.value) {
        ids.push(thread.message_ids[0]);
        if (expandedThreads.value.includes(thread.thread_id)) {
          const children = threadMessages.value[thread.thread_id] ?? [];
          for (const msg of children) {
            ids.push(msg.id);
          }
        }
      }
      return ids;
    }
    return messages.value.map((m) => m.id);
  }

  function selectMessage(
    messageId: string,
    modifiers: { shiftKey: boolean; ctrlKey: boolean; metaKey: boolean },
  ) {
    if (modifiers.shiftKey && lastClickedId.value) {
      // Range selection
      const ids = getVisibleIds();
      const startIdx = ids.indexOf(lastClickedId.value);
      const endIdx = ids.indexOf(messageId);
      if (startIdx !== -1 && endIdx !== -1) {
        const from = Math.min(startIdx, endIdx);
        const to = Math.max(startIdx, endIdx);
        selectedIds.value = ids.slice(from, to + 1);
      }
    } else if (modifiers.ctrlKey || modifiers.metaKey) {
      // Toggle selection
      const idx = selectedIds.value.indexOf(messageId);
      if (idx !== -1) {
        selectedIds.value = selectedIds.value.filter((id) => id !== messageId);
      } else {
        selectedIds.value = [...selectedIds.value, messageId];
      }
      lastClickedId.value = messageId;
    } else {
      // Single click: replace selection
      selectedIds.value = [messageId];
      lastClickedId.value = messageId;
    }
    // Mark as read on single click (synchronous local update)
    if (!modifiers.shiftKey) {
      markAsRead(messageId);
    }
    // Load the clicked message body in the reader (async)
    loadMessage(messageId);
  }

  function toggleSelectMessage(messageId: string) {
    const idx = selectedIds.value.indexOf(messageId);
    if (idx !== -1) {
      selectedIds.value = selectedIds.value.filter((id) => id !== messageId);
    } else {
      selectedIds.value = [...selectedIds.value, messageId];
    }
  }

  function clearSelection() {
    selectedIds.value = [];
  }

  /** Expand selected IDs to include all message IDs in their threads.
   *  In threaded mode, selectedIds contains only the first message ID of each thread.
   *  This resolves them to the full set of message IDs for move/delete/copy operations. */
  function resolveSelectedIds(): string[] {
    if (!uiStore.threadingEnabled) return [...selectedIds.value];
    const allIds: string[] = [];
    for (const id of selectedIds.value) {
      const thread = threads.value.find(t => t.message_ids.includes(id));
      if (thread) {
        for (const mid of thread.message_ids) {
          if (!allIds.includes(mid)) allIds.push(mid);
        }
      } else {
        if (!allIds.includes(id)) allIds.push(id);
      }
    }
    return allIds;
  }

  async function deleteSelected() {
    const accountId = accountsStore.activeAccountId;
    if (!accountId || selectedIds.value.length === 0) return;
    const ids = resolveSelectedIds();
    try {
      await api.deleteMessages(accountId, ids);
      selectedIds.value = [];
      activeMessage.value = null;
      activeMessageId.value = null;
    } catch (e) {
      console.error("Delete failed:", e);
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
      selectedIds.value = [];
      lastClickedId.value = null;
      fetchMessages();
    },
  );

  watch(
    () => uiStore.threadingEnabled,
    () => {
      fetchMessages();
    },
  );

  // Subscribe to backend message-change events with debounce
  let messagesRefreshTimer: ReturnType<typeof setTimeout> | null = null;
  let stopMessagesListener: null | (() => void) = null;
  let disposed = false;
  void listen<string>("messages-changed", () => {
    if (messagesRefreshTimer) clearTimeout(messagesRefreshTimer);
    messagesRefreshTimer = setTimeout(() => {
      fetchMessages();
    }, 200);
  })
    .then((unlisten) => {
      if (disposed) {
        unlisten();
        return;
      }
      stopMessagesListener = unlisten;
    })
    .catch((error) => {
      console.error("Failed to subscribe to messages-changed:", error);
    });

  onScopeDispose(() => {
    disposed = true;
    if (messagesRefreshTimer) clearTimeout(messagesRefreshTimer);
    stopMessagesListener?.();
  });

  return {
    messages,
    threads,
    expandedThreads,
    threadMessages,
    activeMessage,
    activeMessageId,
    selectedIds,
    selectedSet,
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
    isSelected,
    fetchMessages,
    loadMessage,
    loadNextPage,
    toggleThread,
    showAsThread,
    unthreadMessage,
    selectMessage,
    toggleSelectMessage,
    clearSelection,
    deleteSelected,
    setSort,
    quickFilter,
    quickFilterText,
    quickFilterVisible,
    quickFilterFields,
    hasActiveFilter,
    toggleQuickFilter,
    toggleTextField,
    onFilterTextChange,
    clearQuickFilters,
    resolveSelectedIds,
  };
});
