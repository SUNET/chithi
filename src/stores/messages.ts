import { defineStore } from "pinia";
import { ref, computed, watch, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import type {
  MessageSummary,
  MessageBody,
  ThreadSummary,
  QuickFilter,
  SearchHit,
  SearchQuery,
  SearchFields,
} from "@/lib/types";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";
import { useFoldersStore } from "./folders";
import { useUiStore } from "./ui";

export type SortColumn = "subject" | "from" | "date" | "flagged";

export const useMessagesStore = defineStore("messages", () => {
  const messages = ref<MessageSummary[]>([]);
  // Threads default to expanded; only collapsed IDs are persisted.
  const COLLAPSED_KEY = "chithi-collapsed-threads";
  const threads = ref<ThreadSummary[]>([]);
  const collapsedThreads = ref<string[]>(loadCollapsedThreads());
  const threadMessages = ref<Record<string, MessageSummary[]>>({});

  function loadCollapsedThreads(): string[] {
    try {
      const raw = localStorage.getItem(COLLAPSED_KEY);
      const parsed: unknown = raw ? JSON.parse(raw) : [];
      return Array.isArray(parsed) ? parsed.filter((x) => typeof x === "string") : [];
    } catch {
      return [];
    }
  }

  function persistCollapsedThreads() {
    try {
      localStorage.setItem(COLLAPSED_KEY, JSON.stringify(collapsedThreads.value));
    } catch {
      // In-memory collapse state remains usable without storage.
    }
  }

  function isThreadExpanded(threadId: string): boolean {
    return !collapsedThreads.value.includes(threadId);
  }

  const activeMessage = ref<MessageBody | null>(null);
  const activeMessageId = ref<string | null>(null);
  const selectedIds = ref<string[]>([]);
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
  const quickFilterFields = ref<string[]>([]);

  const serverHits = ref<SearchHit[]>([]);
  const serverSearchLoading = ref(false);
  const serverSearchError = ref<string | null>(null);

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
    const filter = quickFilter.value;
    return !!(
      filter.unread ||
      filter.starred ||
      filter.has_attachment ||
      filter.contact ||
      quickFilterText.value.trim()
    );
  });

  const activeFilterForApi = computed((): QuickFilter | undefined => {
    if (!hasActiveFilter.value) return undefined;
    const filter = { ...quickFilter.value };
    const text = quickFilterText.value.trim();
    if (text) {
      filter.text = text;
      if (quickFilterFields.value.length > 0) {
        filter.text_fields = quickFilterFields.value;
      }
    }
    return filter;
  });

  function toggleQuickFilter(key: "unread" | "starred" | "has_attachment" | "contact") {
    quickFilter.value = { ...quickFilter.value, [key]: !quickFilter.value[key] };
    fetchMessages(true);
  }

  function toggleTextField(field: string) {
    const fields = [...quickFilterFields.value];
    const index = fields.indexOf(field);
    if (index !== -1) {
      fields.splice(index, 1);
    } else {
      fields.push(field);
    }
    quickFilterFields.value = fields;
    if (quickFilterText.value.trim()) {
      fetchMessages(true);
    }
  }

  let textSearchTimer: ReturnType<typeof setTimeout> | null = null;
  function onFilterTextChange() {
    if (textSearchTimer) clearTimeout(textSearchTimer);
    clearServerSearch();
    textSearchTimer = setTimeout(() => {
      fetchMessages(true);
    }, 300);
  }

  function clearQuickFilters() {
    quickFilter.value = {};
    quickFilterText.value = "";
    quickFilterFields.value = [];
    if (textSearchTimer) clearTimeout(textSearchTimer);
    clearServerSearch();
    fetchMessages(true);
  }

  function fieldsForServerSearch(): SearchFields {
    const fields = quickFilterFields.value;
    if (fields.length === 0) {
      return { subject: true, from: true, to: true, body: true };
    }
    return {
      subject: fields.includes("subject"),
      from: fields.includes("sender"),
      to: fields.includes("recipients"),
      body: fields.includes("body"),
    };
  }

  // Search tokens discard responses superseded by a newer search or clear.
  let serverSearchToken = 0;

  function clearServerSearch() {
    serverSearchToken++;
    serverHits.value = [];
    serverSearchLoading.value = false;
    serverSearchError.value = null;
  }

  /** Imports a server hit into the local table before loading its body. */
  async function openServerHit(hit: SearchHit) {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) return;
    try {
      const messageId = await api.importSearchHit(accountId, hit);
      await loadMessage(messageId);
    } catch (error) {
      console.error("Failed to open server hit:", error);
      serverSearchError.value =
        error instanceof Error ? error.message : String(error);
    }
  }

  async function runServerSearch() {
    const accountId = accountsStore.activeAccountId;
    const text = quickFilterText.value.trim();
    if (!accountId || !text) return;

    const query: SearchQuery = {
      text,
      fields: fieldsForServerSearch(),
      has_attachment: quickFilter.value.has_attachment ? true : undefined,
    };

    const token = ++serverSearchToken;
    serverSearchLoading.value = true;
    serverSearchError.value = null;
    try {
      const hits = await api.searchMessagesServer(accountId, query);
      if (token !== serverSearchToken) return;
      serverHits.value = hits;
    } catch (error) {
      if (token !== serverSearchToken) return;
      console.error("Server search failed:", error);
      serverSearchError.value =
        error instanceof Error ? error.message : String(error);
      serverHits.value = [];
    } finally {
      if (token === serverSearchToken) {
        serverSearchLoading.value = false;
      }
    }
  }

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
      // Preserve rows and collapse state until replacements arrive.
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
        // Preload expanded children to avoid per-thread loading flashes.
        await prefetchExpandedChildren(accountId, folderPath, result.threads);
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

  async function prefetchExpandedChildren(
    accountId: string,
    folderPath: string,
    targets: ThreadSummary[],
  ) {
    const pending = targets.filter(
      (thread) =>
        thread.message_count > 1 &&
        !collapsedThreads.value.includes(thread.thread_id) &&
        !threadMessages.value[thread.thread_id],
    );
    if (pending.length === 0) return;
    const fetched = await Promise.all(
      pending.map(async (thread): Promise<[string, MessageSummary[]]> => {
        try {
          const messages = await api.getThreadMessages(
            accountId,
            folderPath,
            thread.thread_id,
          );
          return [thread.thread_id, messages];
        } catch (error) {
          console.error(
            `Failed to prefetch thread ${thread.thread_id}:`,
            error,
          );
          return [thread.thread_id, []];
        }
      }),
    );
    const next = { ...threadMessages.value };
    for (const [threadId, messages] of fetched) {
      if (messages.length > 0) next[threadId] = messages;
    }
    threadMessages.value = next;
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
        await prefetchExpandedChildren(accountId, folderPath, result.threads);
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
    const index = collapsedThreads.value.indexOf(threadId);
    if (index !== -1) {
      collapsedThreads.value.splice(index, 1);
      if (!threadMessages.value[threadId]) {
        const accountId = accountsStore.activeAccountId;
        const folderPath = foldersStore.activeFolderPath;
        if (accountId && folderPath) {
          try {
            const messages = await api.getThreadMessages(
              accountId,
              folderPath,
              threadId,
            );
            threadMessages.value = {
              ...threadMessages.value,
              [threadId]: messages,
            };
          } catch (error) {
            // Roll back failed expansion so it remains retryable.
            console.error("toggleThread: failed to load thread children", error);
            collapsedThreads.value = [...collapsedThreads.value, threadId];
          }
        }
      }
    } else {
      collapsedThreads.value = [...collapsedThreads.value, threadId];
    }
    persistCollapsedThreads();
  }

  async function showAsThread(messageId: string) {
    const message = messages.value.find((candidate) => candidate.id === messageId);
    if (!message) return;
    const accountId = accountsStore.activeAccountId;
    const folderPath = foldersStore.activeFolderPath;
    if (!accountId || !folderPath) return;
    const threadMembers = await api.getThreadMessages(
      accountId,
      folderPath,
      messageId,
    );
    if (threadMembers.length > 1) {
      threadMessages.value = {
        ...threadMessages.value,
        [messageId]: threadMembers,
      };
      const index = collapsedThreads.value.indexOf(messageId);
      if (index !== -1) {
        collapsedThreads.value.splice(index, 1);
        persistCollapsedThreads();
      }
    }
  }

  async function unthreadMessage(messageId: string) {
    await api.unthreadMessage(messageId);
    await fetchMessages();
  }

  function findMessage(messageId: string): MessageSummary | undefined {
    let message = messages.value.find((candidate) => candidate.id === messageId);
    if (!message) {
      for (const threadMembers of Object.values(threadMessages.value)) {
        message = threadMembers.find((candidate) => candidate.id === messageId);
        if (message) break;
      }
    }
    return message;
  }

  function subjectForMessage(messageId: string): string | null {
    const message = findMessage(messageId);
    if (message?.subject) return message.subject;
    const thread = threads.value.find((candidate) =>
      candidate.message_ids.includes(messageId),
    );
    return thread?.subject ?? null;
  }

  function markAsRead(messageId: string) {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) return;

    const message = findMessage(messageId);
    if (message) {
      if (message.flags.includes("seen")) return;
      message.flags = [...message.flags, "seen"];
    }

    const thread = threads.value.find((candidate) =>
      candidate.message_ids.includes(messageId),
    );
    if (thread && thread.unread_count > 0) {
      thread.unread_count--;
    }

    // Persist even when a collapsed thread has no loaded message object.
    void api
      .setMessageFlags(accountId, [messageId], ["seen"], true)
      .catch((error) => console.error("Failed to mark as read:", error));
  }

  function markAsUnread(messageId: string) {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) return;

    const message = findMessage(messageId);
    if (message) {
      if (!message.flags.includes("seen")) return;
      message.flags = message.flags.filter((flag) => flag !== "seen");
    }

    const thread = threads.value.find((candidate) =>
      candidate.message_ids.includes(messageId),
    );
    if (thread) {
      thread.unread_count++;
    }

    void api
      .setMessageFlags(accountId, [messageId], ["seen"], false)
      .catch((error) => console.error("Failed to mark as unread:", error));
  }

  function setReadStatus(messageIds: string[], read: boolean) {
    for (const id of messageIds) {
      if (read) {
        markAsRead(id);
      } else {
        markAsUnread(id);
      }
    }
  }

  function toggleStar(messageId: string) {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) return;

    const message = findMessage(messageId);
    const thread = threads.value.find((candidate) =>
      candidate.message_ids.includes(messageId),
    );

    const isCurrentlyStarred = message
      ? message.flags.includes("flagged")
      : thread
        ? thread.flags.includes("flagged")
        : false;

    if (message) {
      if (isCurrentlyStarred) {
        message.flags = message.flags.filter((flag) => flag !== "flagged");
      } else {
        message.flags = [...message.flags, "flagged"];
      }
    }

    if (thread) {
      if (isCurrentlyStarred) {
        thread.flags = thread.flags.filter((flag) => flag !== "flagged");
      } else if (!thread.flags.includes("flagged")) {
        thread.flags = [...thread.flags, "flagged"];
      }
    }

    void api
      .setMessageFlags(accountId, [messageId], ["flagged"], !isCurrentlyStarred)
      .catch((error) => console.error("Failed to toggle star:", error));
  }

  async function loadMessage(messageId: string) {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) return;
    activeMessageId.value = messageId;
    loadingBody.value = true;
    try {
      activeMessage.value = await api.getMessageBody(accountId, messageId);
    } catch (error) {
      console.error("Failed to load message body:", error);
    } finally {
      loadingBody.value = false;
    }
  }

  function getVisibleIds(): string[] {
    if (uiStore.threadingEnabled) {
      const ids: string[] = [];
      for (const thread of threads.value) {
        ids.push(thread.message_ids[0]);
        if (isThreadExpanded(thread.thread_id)) {
          // The thread header already represents the root message.
          const children = threadMessages.value[thread.thread_id] ?? [];
          for (let i = 1; i < children.length; i += 1) {
            ids.push(children[i].id);
          }
        }
      }
      return ids;
    }
    return messages.value.map((message) => message.id);
  }

  function selectMessage(
    messageId: string,
    modifiers: { shiftKey: boolean; ctrlKey: boolean; metaKey: boolean },
  ) {
    if (modifiers.shiftKey && lastClickedId.value) {
      const ids = getVisibleIds();
      const startIndex = ids.indexOf(lastClickedId.value);
      const endIndex = ids.indexOf(messageId);
      if (startIndex !== -1 && endIndex !== -1) {
        const from = Math.min(startIndex, endIndex);
        const to = Math.max(startIndex, endIndex);
        selectedIds.value = ids.slice(from, to + 1);
      }
    } else if (modifiers.ctrlKey || modifiers.metaKey) {
      const index = selectedIds.value.indexOf(messageId);
      if (index !== -1) {
        selectedIds.value = selectedIds.value.filter((id) => id !== messageId);
      } else {
        selectedIds.value = [...selectedIds.value, messageId];
      }
      lastClickedId.value = messageId;
    } else {
      selectedIds.value = [messageId];
      lastClickedId.value = messageId;
    }
    if (!modifiers.shiftKey) {
      markAsRead(messageId);
    }
    void loadMessage(messageId);
  }

  function toggleSelectMessage(messageId: string) {
    const index = selectedIds.value.indexOf(messageId);
    if (index !== -1) {
      selectedIds.value = selectedIds.value.filter((id) => id !== messageId);
    } else {
      selectedIds.value = [...selectedIds.value, messageId];
    }
  }

  function clearSelection() {
    selectedIds.value = [];
  }

  /** Expands selected thread roots for bulk operations. */
  function resolveSelectedIds(): string[] {
    if (!uiStore.threadingEnabled) return [...selectedIds.value];
    const allIds: string[] = [];
    for (const id of selectedIds.value) {
      const thread = threads.value.find((candidate) =>
        candidate.message_ids.includes(id),
      );
      if (thread) {
        for (const messageId of thread.message_ids) {
          if (!allIds.includes(messageId)) allIds.push(messageId);
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

    messages.value = messages.value.filter(
      (message) => !ids.includes(message.id),
    );

    const fullyDeletedThreadIds: string[] = [];
    threads.value = threads.value.filter((thread) => {
      if (thread.message_ids.every((messageId) => ids.includes(messageId))) {
        fullyDeletedThreadIds.push(thread.thread_id);
        return false;
      }
      return true;
    });

    if (fullyDeletedThreadIds.length > 0) {
      const updated = { ...threadMessages.value };
      for (const threadId of fullyDeletedThreadIds) {
        delete updated[threadId];
      }
      threadMessages.value = updated;
    }

    for (const thread of threads.value) {
      const remaining = thread.message_ids.filter(
        (messageId) => !ids.includes(messageId),
      );
      if (remaining.length < thread.message_ids.length) {
        thread.message_ids = remaining;
        if (threadMessages.value[thread.thread_id]) {
          threadMessages.value = {
            ...threadMessages.value,
            [thread.thread_id]: threadMessages.value[thread.thread_id].filter(
              (message) => !ids.includes(message.id),
            ),
          };
        }
      }
    }

    selectedIds.value = [];
    activeMessage.value = null;
    activeMessageId.value = null;

    try {
      await api.deleteMessages(accountId, ids);
    } catch (error) {
      // Later server failures arrive through op-failed and trigger reconciliation.
      console.error("Delete dispatch failed:", error);
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

  // Account and path are both required because folder names can repeat.
  watch(
    () => [accountsStore.activeAccountId, foldersStore.activeFolderPath] as const,
    () => {
      activeMessage.value = null;
      activeMessageId.value = null;
      selectedIds.value = [];
      lastClickedId.value = null;
      clearServerSearch();
      void fetchMessages();
    },
  );

  watch(
    () => uiStore.threadingEnabled,
    () => {
      void fetchMessages();
    },
  );

  let messagesRefreshTimer: ReturnType<typeof setTimeout> | null = null;
  let stopMessagesListener: (() => void) | null = null;
  let disposed = false;
  void listen<string>("messages-changed", () => {
    if (disposed) return;
    if (messagesRefreshTimer) clearTimeout(messagesRefreshTimer);
    messagesRefreshTimer = setTimeout(() => {
      void fetchMessages();
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

  let stopOpFailedListener: (() => void) | null = null;
  void listen<{ account_id: string; op_type: string; error: string }>(
    "op-failed",
    (event) => {
      if (disposed) return;
      const payload = event.payload;
      console.warn(
        `op-failed: ${payload.op_type} on account ${payload.account_id}: ${payload.error}`,
      );
      // Reconcile optimistic state even if the active account has changed.
      void fetchMessages().catch((error) => {
        console.error("Failed to reconcile messages after op-failed:", error);
      });
    },
  )
    .then((unlisten) => {
      if (disposed) {
        unlisten();
        return;
      }
      stopOpFailedListener = unlisten;
    })
    .catch((error) => {
      console.error("Failed to subscribe to op-failed:", error);
    });

  onScopeDispose(() => {
    disposed = true;
    if (messagesRefreshTimer) clearTimeout(messagesRefreshTimer);
    stopMessagesListener?.();
    stopOpFailedListener?.();
  });

  return {
    messages,
    threads,
    collapsedThreads,
    isThreadExpanded,
    threadMessages,
    activeMessage,
    activeMessageId,
    selectedIds,
    loading,
    loadingMore,
    loadingBody,
    total,
    totalThreads,
    sortColumn,
    sortAsc,
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
    toggleStar,
    markAsUnread,
    setReadStatus,
    subjectForMessage,
    serverHits,
    serverSearchLoading,
    serverSearchError,
    runServerSearch,
    clearServerSearch,
    openServerHit,
  };
});
