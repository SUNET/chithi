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
  // Threaded mode state.
  // `collapsedThreads` is the inverse of an expansion list: every visible
  // thread is treated as expanded by default, and only ids the user has
  // explicitly collapsed are tracked. The list survives folder/account
  // changes and incremental syncs so toggling stays sticky. Persisted to
  // localStorage so it also survives app restart.
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
      // ignore quota / private-mode failures; in-memory state still works
    }
  }

  function isThreadExpanded(threadId: string): boolean {
    return !collapsedThreads.value.includes(threadId);
  }

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

  // Server-side search state (separate from local quick-filter)
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
    // The server-search results are tied to the *previous* query, so clear
    // them as soon as the user changes the input.
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
    // QuickFilterBar uses `quickFilterFields` with values 'sender', 'recipients',
    // 'subject', 'body'. Empty array means "all fields" (matches the bar's UI).
    const f = quickFilterFields.value;
    if (f.length === 0) {
      return { subject: true, from: true, to: true, body: true };
    }
    return {
      subject: f.includes("subject"),
      from: f.includes("sender"),
      to: f.includes("recipients"),
      body: f.includes("body"),
    };
  }

  // Monotonically increases on each server-search dispatch (and on each
  // clear). Late results from a stale request are dropped if their token
  // no longer matches.
  let serverSearchToken = 0;

  function clearServerSearch() {
    serverSearchToken++;
    serverHits.value = [];
    serverSearchLoading.value = false;
    serverSearchError.value = null;
  }

  /**
   * Open a server-search hit. The hit may not be in the local messages
   * table yet, so we first ask the backend to upsert a stub row (the
   * existing get_message_body flow then fetches the body on demand) and
   * then drive the regular load-and-display path.
   */
  async function openServerHit(hit: SearchHit) {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) return;
    try {
      const messageId = await api.importSearchHit(accountId, hit);
      await loadMessage(messageId);
    } catch (e) {
      console.error("Failed to open server hit:", e);
      serverSearchError.value = e instanceof Error ? e.message : String(e);
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

    const myToken = ++serverSearchToken;
    serverSearchLoading.value = true;
    serverSearchError.value = null;
    try {
      const hits = await api.searchMessagesServer(accountId, query);
      // Drop the result if a newer search/clear has happened in the meantime.
      if (myToken !== serverSearchToken) return;
      serverHits.value = hits;
    } catch (e) {
      if (myToken !== serverSearchToken) return;
      console.error("Server search failed:", e);
      serverSearchError.value = e instanceof Error ? e.message : String(e);
      serverHits.value = [];
    } finally {
      if (myToken === serverSearchToken) {
        serverSearchLoading.value = false;
      }
    }
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
      // collapsedThreads is intentionally preserved across syncs and
      // folder/account switches so the user's toggle state is sticky.
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
        // Threads are expanded by default. Pre-fetch children for every
        // multi-message thread that the user hasn't collapsed so the rows
        // render without a per-thread loading flash.
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

  /**
   * Fetch child message lists for every visible multi-message thread the
   * user has not explicitly collapsed. Existing entries in
   * `threadMessages` are skipped so re-renders don't refetch.
   */
  async function prefetchExpandedChildren(
    accountId: string,
    folderPath: string,
    targets: ThreadSummary[],
  ) {
    const todo = targets.filter(
      (t) =>
        t.message_count > 1 &&
        !collapsedThreads.value.includes(t.thread_id) &&
        !threadMessages.value[t.thread_id],
    );
    if (todo.length === 0) return;
    const fetched = await Promise.all(
      todo.map(async (t): Promise<[string, MessageSummary[]]> => {
        try {
          const msgs = await api.getThreadMessages(accountId, folderPath, t.thread_id);
          return [t.thread_id, msgs];
        } catch (e) {
          console.error(`Failed to prefetch thread ${t.thread_id}:`, e);
          return [t.thread_id, []];
        }
      }),
    );
    const next = { ...threadMessages.value };
    for (const [id, msgs] of fetched) {
      if (msgs.length > 0) next[id] = msgs;
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
    const idx = collapsedThreads.value.indexOf(threadId);
    if (idx !== -1) {
      // Currently collapsed -> expand. Make sure children are loaded.
      collapsedThreads.value.splice(idx, 1);
      if (!threadMessages.value[threadId]) {
        const accountId = accountsStore.activeAccountId;
        const folderPath = foldersStore.activeFolderPath;
        if (accountId && folderPath) {
          try {
            const msgs = await api.getThreadMessages(accountId, folderPath, threadId);
            threadMessages.value = { ...threadMessages.value, [threadId]: msgs };
          } catch (err) {
            // The lazy fetch failed (network/IPC error). Roll the expansion
            // back so the row state matches the "no children loaded" reality
            // and the user can retry.
            console.error("toggleThread: failed to load thread children", err);
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
    const msg = messages.value.find((m) => m.id === messageId);
    if (!msg) return;
    const accountId = accountsStore.activeAccountId;
    const folderPath = foldersStore.activeFolderPath;
    if (!accountId || !folderPath) return;
    const threadMsgs = await api.getThreadMessages(accountId, folderPath, messageId);
    if (threadMsgs.length > 1) {
      threadMessages.value = { ...threadMessages.value, [messageId]: threadMsgs };
      // Threads are expanded by default — make sure this one is too,
      // even if the user had it on their collapsed list.
      const idx = collapsedThreads.value.indexOf(messageId);
      if (idx !== -1) {
        collapsedThreads.value.splice(idx, 1);
        persistCollapsedThreads();
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

  // Get a subject for a message id, searching flat messages, expanded thread
  // messages, and thread summaries (when threading is enabled the flat list
  // may be empty and the subject lives on the thread).
  function subjectForMessage(messageId: string): string | null {
    const msg = findMessage(messageId);
    if (msg?.subject) return msg.subject;
    const thread = threads.value.find((t) => t.message_ids.includes(messageId));
    return thread?.subject ?? null;
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

  function markAsUnread(messageId: string) {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) return;

    const msg = findMessage(messageId);
    if (msg) {
      if (!msg.flags.includes("seen")) return; // already unread
      msg.flags = msg.flags.filter((f) => f !== "seen");
    }

    const thread = threads.value.find((t) =>
      t.message_ids.includes(messageId),
    );
    if (thread) {
      thread.unread_count++;
    }

    api
      .setMessageFlags(accountId, [messageId], ["seen"], false)
      .catch((e) => console.error("Failed to mark as unread:", e));
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

    const msg = findMessage(messageId);
    const thread = threads.value.find((t) =>
      t.message_ids.includes(messageId),
    );

    // Check individual message first, fall back to thread summary
    // (in threaded mode with collapsed threads, msg may be undefined)
    const isCurrentlyStarred = msg
      ? msg.flags.includes("flagged")
      : thread
        ? thread.flags.includes("flagged")
        : false;

    if (msg) {
      if (isCurrentlyStarred) {
        msg.flags = msg.flags.filter((f) => f !== "flagged");
      } else {
        msg.flags = [...msg.flags, "flagged"];
      }
    }

    // Update thread summary flags
    if (thread) {
      if (isCurrentlyStarred) {
        thread.flags = thread.flags.filter((f) => f !== "flagged");
      } else if (!thread.flags.includes("flagged")) {
        thread.flags = [...thread.flags, "flagged"];
      }
    }

    api
      .setMessageFlags(accountId, [messageId], ["flagged"], !isCurrentlyStarred)
      .catch((e) => console.error("Failed to toggle star:", e));
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
        if (isThreadExpanded(thread.thread_id)) {
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

    // Optimistic: remove from local state immediately
    messages.value = messages.value.filter((m) => !ids.includes(m.id));

    // Track which threads are fully deleted vs partially deleted
    const fullyDeletedThreadIds: string[] = [];
    threads.value = threads.value.filter((t) => {
      if (t.message_ids.every((mid) => ids.includes(mid))) {
        fullyDeletedThreadIds.push(t.thread_id);
        return false; // remove fully deleted thread
      }
      return true;
    });

    // Clean up threadMessages for fully deleted threads
    if (fullyDeletedThreadIds.length > 0) {
      const updated = { ...threadMessages.value };
      for (const tid of fullyDeletedThreadIds) {
        delete updated[tid];
      }
      threadMessages.value = updated;
    }

    // For partially deleted threads, remove the deleted message_ids
    for (const t of threads.value) {
      const remaining = t.message_ids.filter((mid) => !ids.includes(mid));
      if (remaining.length < t.message_ids.length) {
        t.message_ids = remaining;
        // Also clean up expanded thread messages
        if (threadMessages.value[t.thread_id]) {
          threadMessages.value = {
            ...threadMessages.value,
            [t.thread_id]: threadMessages.value[t.thread_id].filter(
              (m) => !ids.includes(m.id),
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
    } catch (e) {
      // Backend returns immediately (optimistic), so errors here are
      // from the command dispatch itself, not the server operation.
      // Server failures are reported via the "op-failed" event.
      console.error("Delete dispatch failed:", e);
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
      clearServerSearch();
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
    if (disposed) return;
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

  // Subscribe to op-failed events — reconcile by re-fetching when a
  // background server operation fails after an optimistic local update.
  let stopOpFailedListener: null | (() => void) = null;
  void listen<{ account_id: string; op_type: string; error: string }>(
    "op-failed",
    (event) => {
      if (disposed) return;
      const p = event.payload;
      console.warn(`op-failed: ${p.op_type} on account ${p.account_id}: ${p.error}`);
      // Always re-fetch to reconcile optimistic state — the failed op may
      // affect the currently visible folder even if the active account has
      // changed since the op was dispatched.
      void fetchMessages().catch((err) => {
        console.error("Failed to reconcile messages after op-failed:", err);
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
