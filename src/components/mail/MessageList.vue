<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from "vue";
import { dragMessageIds, dragSourceAccountId, isDragging } from "@/lib/drag-state";
import { showToast, dismissToast } from "@/lib/toast";
import { useMessagesStore } from "@/stores/messages";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useUiStore } from "@/stores/ui";
import type { SortColumn } from "@/stores/messages";
import type { MessageSummary } from "@/lib/types";
import { openComposeWindow } from "@/lib/compose-window";
import * as api from "@/lib/tauri";
import MessageListItem from "./MessageListItem.vue";
import ThreadRow from "./ThreadRow.vue";
import QuickFilterBar from "./QuickFilterBar.vue";

const messagesStore = useMessagesStore();
const accountsStore = useAccountsStore();
const foldersStore = useFoldersStore();
const uiStore = useUiStore();
const scrollContainer = ref<HTMLElement | null>(null);

const emit = defineEmits<{
  openMessage: [messageId: string];
}>();

withDefaults(
  defineProps<{ mode?: "desktop" | "mobile" }>(),
  { mode: "desktop" },
);

async function onMobileSwipeArchive(messageId: string) {
  const accountId = accountsStore.activeAccountId;
  if (!accountId) return;
  // folder_type is provider-normalized across names and locales.
  const archive = foldersStore
    .getFlatFolders()
    .find((f) => f.folder_type === "archive");
  if (!archive) {
    // Without an archive folder, fall back to marking the message read.
    try {
      await api.setMessageFlags(accountId, [messageId], ["seen"], true);
      void messagesStore.fetchMessages();
    } catch (error) {
      console.error("swipe archive fallback failed", error);
    }
    return;
  }
  try {
    await api.moveMessages(accountId, [messageId], archive.path);
    void messagesStore.fetchMessages();
  } catch (error) {
    console.error("swipe archive failed", error);
  }
}

async function onMobileSwipeDelete(messageId: string) {
  const accountId = accountsStore.activeAccountId;
  if (!accountId) return;
  // Route through deleteSelected so thread cleanup + optimistic removal runs.
  messagesStore.selectedIds.splice(0, messagesStore.selectedIds.length, messageId);
  await messagesStore.deleteSelected();
}

// WebKitGTK can lose click-event modifier state, so track it independently.
const shiftHeld = ref(false);
const ctrlHeld = ref(false);

function onKeyDown(event: KeyboardEvent) {
  if (event.key === "Shift") shiftHeld.value = true;
  if (event.key === "Control" || event.key === "Meta") ctrlHeld.value = true;
  if (event.key === "Delete" && messagesStore.selectedIds.length > 0) {
    event.preventDefault();
    messagesStore.deleteSelected();
  }
  if (event.key === "/" && !(event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement)) {
    event.preventDefault();
    toggleQuickFilter();
  }
  if (event.key === "Escape" && messagesStore.quickFilterVisible) {
    messagesStore.quickFilterVisible = false;
    messagesStore.clearQuickFilters();
  }
}

function onKeyUp(event: KeyboardEvent) {
  if (event.key === "Shift") shiftHeld.value = false;
  if (event.key === "Control" || event.key === "Meta") ctrlHeld.value = false;
}

onMounted(() => {
  window.addEventListener("keydown", onKeyDown);
  window.addEventListener("keyup", onKeyUp);
});

onUnmounted(() => {
  window.removeEventListener("keydown", onKeyDown);
  window.removeEventListener("keyup", onKeyUp);
});

const contextMenu = ref<{
  x: number;
  y: number;
  messageId: string;
} | null>(null);
const subMenu = ref<"move" | "copy" | null>(null);

const isSingleSelection = () => messagesStore.selectedIds.length <= 1;

function onChildSelect(messageId: string) {
  closeContextMenu();
  messagesStore.selectMessage(messageId, { shiftKey: false, ctrlKey: false, metaKey: false });
}

function onSelect(messageId: string, event?: MouseEvent) {
  closeContextMenu();
  // Check tracked and click-event modifiers because WebKitGTK may lose either.
  const isShift = shiftHeld.value || (event?.shiftKey ?? false);
  const isCtrl = ctrlHeld.value || (event?.ctrlKey ?? false) || (event?.metaKey ?? false);
  messagesStore.selectMessage(messageId, {
    shiftKey: isShift,
    ctrlKey: isCtrl,
    metaKey: false,
  });
}

// WebKitGTK requires mouse-driven drag and drop.
const dragStartPos = ref<{ x: number; y: number } | null>(null);
const dragSourceId = ref<string | null>(null);
const dragGhost = ref<HTMLElement | null>(null);
const DRAG_THRESHOLD = 5;

function onDragMouseDown(event: MouseEvent, messageId: string) {
  if (event.button !== 0) return;
  if ((event.target as HTMLElement).closest(".col-check")) return;
  dragStartPos.value = { x: event.clientX, y: event.clientY };
  dragSourceId.value = messageId;

  const handleMove = (moveEvent: MouseEvent) => {
    if (!dragStartPos.value || !dragSourceId.value) return;

    const dx = moveEvent.clientX - dragStartPos.value.x;
    const dy = moveEvent.clientY - dragStartPos.value.y;
    if (!isDragging.value && Math.hypot(dx, dy) < DRAG_THRESHOLD) return;

    if (!isDragging.value) {
      // Select for drag without triggering read/open side effects.
      if (!messagesStore.isSelected(dragSourceId.value)) {
        messagesStore.selectedIds = [dragSourceId.value];
      }
      dragMessageIds.value = messagesStore.resolveSelectedIds();
      dragSourceAccountId.value = accountsStore.activeAccountId;
      isDragging.value = true;

      // Keep the non-interactive badge away from drop targets.
      const ghost = document.createElement("div");
      ghost.textContent = `${dragMessageIds.value.length} message(s)`;
      ghost.style.cssText = "position:fixed;z-index:99999;padding:4px 10px;background:#3366cc;color:white;border-radius:4px;font-size:12px;font-weight:500;white-space:nowrap;pointer-events:none;";
      document.body.appendChild(ghost);
      dragGhost.value = ghost;
      document.body.classList.add("dragging-messages");
    }

    if (dragGhost.value) {
      dragGhost.value.style.left = moveEvent.clientX + 12 + "px";
      dragGhost.value.style.top = moveEvent.clientY + 12 + "px";
    }
  };

  const handleUp = () => {
    document.body.classList.remove("dragging-messages");
    if (isDragging.value) {
      setTimeout(() => {
        isDragging.value = false;
        dragMessageIds.value = [];
        dragSourceAccountId.value = null;
        if (dragGhost.value) {
          dragGhost.value.remove();
          dragGhost.value = null;
        }
      }, 0);
    }
    dragStartPos.value = null;
    dragSourceId.value = null;
    document.removeEventListener("mousemove", handleMove);
    document.removeEventListener("mouseup", handleUp);
  };

  document.addEventListener("mousemove", handleMove);
  document.addEventListener("mouseup", handleUp);
}

function onOpen(messageId: string) {
  messagesStore.loadMessage(messageId);
  emit("openMessage", messageId);
}

function onThreadSelect(thread: { message_ids: string[] }, event: MouseEvent) {
  if (thread.message_ids.length > 0) {
    onSelect(thread.message_ids[0], event);
  }
}

function onThreadOpen(thread: { thread_id: string; message_ids: string[] }) {
  if (thread.message_ids.length > 0) {
    messagesStore.loadMessage(thread.message_ids[0]);
  }
  emit("openMessage", thread.message_ids[0]);
}

function sortIndicator(column: SortColumn): string {
  if (messagesStore.sortColumn !== column) return "";
  return messagesStore.sortAsc ? " \u25B4" : " \u25BE";
}

function onScroll() {
  const el = scrollContainer.value;
  if (!el) return;
  if (el.scrollHeight - el.scrollTop - el.clientHeight < 200) {
    messagesStore.loadNextPage();
  }
}

function closeContextMenu() {
  contextMenu.value = null;
  subMenu.value = null;
}

function onRowRightClick(event: MouseEvent, messageId: string) {
  event.preventDefault();
  if (!messagesStore.selectedIds.includes(messageId)) {
    messagesStore.selectMessage(messageId, { shiftKey: false, ctrlKey: false, metaKey: false });
  }
  contextMenu.value = { x: event.clientX, y: event.clientY, messageId };
  subMenu.value = null;
}

async function loadContextMessage() {
  const messageId = contextMenu.value?.messageId;
  closeContextMenu();
  if (!messageId) return null;
  await messagesStore.loadMessage(messageId);
  const message = messagesStore.activeMessage;
  return message ? { messageId, message } : null;
}

async function ctxReply() {
  const context = await loadContextMessage();
  if (!context) return;
  const { messageId, message: msg } = context;
  const body = msg.body_text || "";
  const date = new Date(msg.date).toLocaleString(undefined, { hour12: uiStore.hour12 });
  const from = msg.from.name ? `${msg.from.name} <${msg.from.email}>` : msg.from.email;
  const quoted = body.split("\n").map((line) => `> ${line}`).join("\n");
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    replyTo: messageId,
    to: msg.from.email,
    subject: msg.subject?.startsWith("Re:") ? msg.subject : `Re: ${msg.subject || ""}`,
    body: `\n\nOn ${date}, ${from} wrote:\n${quoted}`,
  });
}

async function ctxReplyAll() {
  const context = await loadContextMessage();
  if (!context) return;
  const { messageId, message: msg } = context;
  const myEmail = accountsStore.activeAccount()?.email ?? "";
  const allTo = [msg.from.email, ...msg.to.map((address) => address.email).filter((email) => email !== myEmail)];
  const allCc = msg.cc.map((address) => address.email).filter((email) => email !== myEmail);
  const body = msg.body_text || "";
  const date = new Date(msg.date).toLocaleString(undefined, { hour12: uiStore.hour12 });
  const from = msg.from.name ? `${msg.from.name} <${msg.from.email}>` : msg.from.email;
  const quoted = body.split("\n").map((line) => `> ${line}`).join("\n");
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    replyTo: messageId,
    to: allTo.join(", "),
    cc: allCc.join(", "),
    subject: msg.subject?.startsWith("Re:") ? msg.subject : `Re: ${msg.subject || ""}`,
    body: `\n\nOn ${date}, ${from} wrote:\n${quoted}`,
  });
}

async function ctxForward() {
  const context = await loadContextMessage();
  if (!context) return;
  const { message: msg } = context;
  const text = msg.body_text || "";
  const date = new Date(msg.date).toLocaleString(undefined, { hour12: uiStore.hour12 });
  const from = msg.from.name ? `${msg.from.name} <${msg.from.email}>` : msg.from.email;
  const toStr = msg.to.map((address) => address.name || address.email).join(", ");
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    subject: msg.subject?.startsWith("Fwd:") ? msg.subject : `Fwd: ${msg.subject || ""}`,
    body: `\n\n---------- Forwarded message ----------\nFrom: ${from}\nDate: ${date}\nSubject: ${msg.subject || ""}\nTo: ${toStr}\n\n${text}`,
  });
}

function clearSelectionAndMessage() {
  messagesStore.clearSelection();
  messagesStore.activeMessage = null;
  messagesStore.activeMessageId = null;
}

async function ctxMoveTo(folderPath: string) {
  const accountId = accountsStore.activeAccountId;
  if (!accountId) return;
  const ids = messagesStore.resolveSelectedIds();
  closeContextMenu();
  const toastId = showToast(`Moving ${ids.length} message(s)...`, "info", 0);
  try {
    await api.moveMessages(accountId, ids, folderPath);
    clearSelectionAndMessage();
  } catch (error) {
    console.error("Move failed:", error);
  } finally {
    dismissToast(toastId);
  }
}

async function ctxCopyTo(folderPath: string) {
  const accountId = accountsStore.activeAccountId;
  if (!accountId) return;
  const ids = messagesStore.resolveSelectedIds();
  closeContextMenu();
  try {
    await api.copyMessages(accountId, ids, folderPath);
  } catch (error) {
    console.error("Copy failed:", error);
  }
}

async function ctxDelete() {
  closeContextMenu();
  await messagesStore.deleteSelected();
}

function ctxSetReadStatus(read: boolean) {
  const ids = messagesStore.resolveSelectedIds();
  closeContextMenu();
  messagesStore.setReadStatus(ids, read);
}

function hasSelectedReadStatus(read: boolean): boolean {
  const ids = messagesStore.resolveSelectedIds();
  for (const id of ids) {
    const message = messagesStore.messages.find((candidate) => candidate.id === id);
    if (message && message.flags.includes("seen") === read) return true;
    const thread = messagesStore.threads.find((candidate) => candidate.message_ids.includes(id));
    if (
      thread &&
      (read ? thread.unread_count < thread.message_count : thread.unread_count > 0)
    ) return true;
  }
  return false;
}

async function ctxSaveAsEml() {
  const msgId = contextMenu.value?.messageId;
  const accountId = accountsStore.activeAccountId;
  closeContextMenu();
  if (!msgId || !accountId) return;
  try {
    await api.saveMessageAsEml(accountId, msgId, "message.eml");
  } catch (error) {
    console.error("Save as .eml failed:", error);
    const message = error instanceof Error ? error.message : String(error);
    showToast(`Save as .eml failed: ${message}`, "error");
  }
}

function ctxShowAsThread() {
  if (contextMenu.value) {
    messagesStore.showAsThread(contextMenu.value.messageId);
  }
  closeContextMenu();
}

function ctxUnthread() {
  if (contextMenu.value) {
    messagesStore.unthreadMessage(contextMenu.value.messageId);
  }
  closeContextMenu();
}

const isJunkFolder = () => {
  const active = foldersStore.activeFolderPath;
  if (!active) return false;
  const folder = foldersStore.folders.find((f) => f.path === active);
  return folder?.folder_type === "junk";
};

async function ctxNotSpam() {
  const accountId = accountsStore.activeAccountId;
  if (!accountId) return;
  const inboxFolder = foldersStore.folders.find((f) => f.folder_type === "inbox");
  if (!inboxFolder) return;
  const ids = messagesStore.resolveSelectedIds();
  closeContextMenu();
  try {
    await api.moveMessages(accountId, ids, inboxFolder.path);
    clearSelectionAndMessage();
  } catch (error) {
    console.error("Not Spam failed:", error);
  }
}

const visibleMessageIds = computed(() =>
  uiStore.threadingEnabled
    ? messagesStore.threads.map((thread) => thread.message_ids[0])
    : messagesStore.messages.map((message) => message.id),
);

const allSelected = computed(
  () =>
    visibleMessageIds.value.length > 0 &&
    visibleMessageIds.value.every((messageId) => messagesStore.isSelected(messageId)),
);

function toggleSelectAll() {
  if (allSelected.value) {
    messagesStore.clearSelection();
  } else {
    messagesStore.selectedIds = [...visibleMessageIds.value];
  }
}

function toggleQuickFilter() {
  messagesStore.quickFilterVisible = !messagesStore.quickFilterVisible;
  if (!messagesStore.quickFilterVisible) {
    messagesStore.clearQuickFilters();
  }
}

const displayedCount = () => {
  if (uiStore.threadingEnabled) {
    return `${messagesStore.threads.length} of ${messagesStore.totalThreads} threads (${messagesStore.total} messages)`;
  }
  return `${messagesStore.messages.length} of ${messagesStore.total}`;
};

function formatHitDate(secs: number): string {
  if (!secs) return "";
  return new Date(secs * 1000).toLocaleDateString();
}

interface ThreadChildEntry {
  message: MessageSummary;
  depth: number;
}

// Normalize brackets without changing Message-ID case.
function normMid(value: string | null | undefined): string | null {
  if (!value) return null;
  const trimmed = value.trim();
  if (!trimmed) return null;
  return trimmed.replace(/^<+/, "").replace(/>+$/, "");
}

// Emit children depth-first; missing parents become depth-zero orphans.
function childrenWithDepth(threadId: string): ThreadChildEntry[] {
  const all = messagesStore.threadMessages[threadId] ?? [];
  if (all.length <= 1) return [];

  const root = all[0];
  const rest = all.slice(1);

  const byMid = new Map<string, MessageSummary>();
  for (const message of all) {
    const messageId = normMid(message.message_id);
    if (messageId) byMid.set(messageId, message);
  }

  const childrenByParent = new Map<string, MessageSummary[]>();
  const orphans: MessageSummary[] = [];
  for (const message of rest) {
    const parentMid = normMid(message.in_reply_to);
    if (parentMid && byMid.has(parentMid)) {
      const children = childrenByParent.get(parentMid) ?? [];
      children.push(message);
      childrenByParent.set(parentMid, children);
    } else {
      orphans.push(message);
    }
  }

  const out: ThreadChildEntry[] = [];
  const emitted = new Set<string>();
  function emitChildrenOf(parentMid: string | null, depth: number) {
    const children = parentMid === null ? orphans : childrenByParent.get(parentMid) ?? [];
    for (const message of children) {
      if (emitted.has(message.id)) continue;
      emitted.add(message.id);
      out.push({ message, depth });
      const childMid = normMid(message.message_id);
      if (childMid) emitChildrenOf(childMid, depth + 1);
    }
  }
  const rootMid = normMid(root.message_id);
  if (rootMid) emitChildrenOf(rootMid, 0);
  emitChildrenOf(null, 0);

  // Keep cycles and unreachable children visible.
  for (const message of rest) {
    if (!emitted.has(message.id)) {
      emitted.add(message.id);
      out.push({ message, depth: 0 });
    }
  }

  return out;
}

// Index backend folder IDs once per tree change.
const folderNameByPath = computed(
  () => new Map(foldersStore.getFlatFolders().map((folder) => [folder.path, folder.name])),
);

function resolveFolderName(path: string): string {
  if (!path) return "(unknown folder)";
  return folderNameByPath.value.get(path) ?? path;
}
</script>

<template>
  <div class="message-list" data-testid="message-list" @click.left="closeContextMenu">
    <QuickFilterBar v-if="messagesStore.quickFilterVisible" />
    <div class="column-headers">
      <div
        class="col col-check"
        data-testid="select-all-cell"
        @click.stop="toggleSelectAll"
      >
        <input
          type="checkbox"
          class="row-checkbox"
          data-testid="select-all-checkbox"
          :checked="allSelected"
          @click.stop="toggleSelectAll"
        />
      </div>
      <div class="col col-icons">
        <span class="col-icon" title="Read/Star">&#x2606;</span>
      </div>
      <button
        class="col col-subject sortable"
        :class="{ active: messagesStore.sortColumn === 'subject' }"
        @click="messagesStore.setSort('subject')"
      >
        Subject{{ sortIndicator('subject') }}
      </button>
      <button
        class="col col-from sortable"
        :class="{ active: messagesStore.sortColumn === 'from' }"
        @click="messagesStore.setSort('from')"
      >
        Correspondents{{ sortIndicator('from') }}
      </button>
      <button
        class="col col-date sortable"
        :class="{ active: messagesStore.sortColumn === 'date' }"
        @click="messagesStore.setSort('date')"
      >
        Date{{ sortIndicator('date') }}
      </button>
    </div>
    <div
      v-if="messagesStore.loading && messagesStore.messages.length === 0 && messagesStore.threads.length === 0"
      class="loading"
    >Loading...</div>
    <div
      v-else-if="messagesStore.messages.length === 0 && messagesStore.threads.length === 0"
      class="empty"
    >No messages</div>

    <div
      v-else-if="uiStore.threadingEnabled"
      ref="scrollContainer"
      class="message-items"
      @scroll="onScroll"
    >
      <template v-for="thread in messagesStore.threads" :key="thread.thread_id">
        <div
          @click="onThreadSelect(thread, $event)"
          @contextmenu.prevent="onRowRightClick($event,thread.message_ids[0])"
          @mousedown="onDragMouseDown($event, thread.message_ids[0])"
        >
          <ThreadRow
            :thread="thread"
            :expanded="messagesStore.isThreadExpanded(thread.thread_id)"
            :active="thread.message_ids.includes(messagesStore.activeMessageId ?? '')"
            :selected="messagesStore.isSelected(thread.message_ids[0])"
            @toggle="messagesStore.toggleThread(thread.thread_id)"
            @toggle-select="messagesStore.toggleSelectMessage(thread.message_ids[0])"
            @open="onThreadOpen(thread)"
            @toggle-star="messagesStore.toggleStar(thread.message_ids[0])"
          />
        </div>
        <template v-if="messagesStore.isThreadExpanded(thread.thread_id)">
          <div
            v-for="entry in childrenWithDepth(thread.thread_id)"
            :key="entry.message.id"
            class="thread-child"
            :style="`--reply-depth: ${entry.depth}`"
            @click.stop="onChildSelect(entry.message.id)"
            @contextmenu.prevent.stop="onRowRightClick($event, entry.message.id)"
            @mousedown="onDragMouseDown($event, entry.message.id)"
          >
            <MessageListItem
              :message="entry.message"
              :active="messagesStore.activeMessageId === entry.message.id"
              :selected="messagesStore.isSelected(entry.message.id)"
              :mode="mode"
              @toggle="messagesStore.toggleSelectMessage(entry.message.id)"
              @open="onOpen(entry.message.id)"
              @toggle-star="messagesStore.toggleStar(entry.message.id)"
              @archive="onMobileSwipeArchive(entry.message.id)"
              @delete="onMobileSwipeDelete(entry.message.id)"
            />
          </div>
        </template>
      </template>
      <div v-if="messagesStore.loadingMore" class="loading-more">Loading more...</div>
    </div>

    <div v-else ref="scrollContainer" class="message-items" @scroll="onScroll">
      <div
        v-for="msg in messagesStore.messages"
        :key="msg.id"
        @click="onSelect(msg.id, $event)"
        @contextmenu.prevent="onRowRightClick($event,msg.id)"
        @mousedown="onDragMouseDown($event, msg.id)"
      >
        <MessageListItem
          :message="msg"
          :active="messagesStore.activeMessageId === msg.id"
          :selected="messagesStore.isSelected(msg.id)"
          :mode="mode"
          @toggle="messagesStore.toggleSelectMessage(msg.id)"
          @open="onOpen(msg.id)"
          @toggle-star="messagesStore.toggleStar(msg.id)"
          @archive="onMobileSwipeArchive(msg.id)"
          @delete="onMobileSwipeDelete(msg.id)"
        />
      </div>
      <div v-if="messagesStore.loadingMore" class="loading-more">Loading more...</div>
    </div>

    <div
      v-if="messagesStore.serverHits.length > 0"
      class="server-results"
      data-testid="server-results"
    >
      <div class="server-results-header">
        Server results ({{ messagesStore.serverHits.length }})
      </div>
      <div
        v-for="hit in messagesStore.serverHits"
        :key="hit.backend_id"
        class="server-hit-row"
        data-testid="server-hit-row"
        role="button"
        tabindex="0"
        @click="messagesStore.openServerHit(hit)"
        @keydown.enter.prevent="messagesStore.openServerHit(hit)"
        @keydown.space.prevent="messagesStore.openServerHit(hit)"
      >
        <div class="hit-line">
          <span class="hit-from">{{ hit.from_name || hit.from_email || "(unknown sender)" }}</span>
          <span class="hit-date">{{ formatHitDate(hit.date) }}</span>
        </div>
        <div class="hit-subject">{{ hit.subject || "(no subject)" }}</div>
        <div v-if="hit.snippet" class="hit-snippet">{{ hit.snippet }}</div>
        <div class="hit-folder">in {{ resolveFolderName(hit.folder_path) }}</div>
      </div>
    </div>

    <div class="list-footer">
      <span class="message-count" data-testid="message-count">{{ displayedCount() }}</span>
      <button
        class="quick-filter-toggle"
        data-testid="quick-filter-toggle"
        :class="{ active: messagesStore.quickFilterVisible }"
        @click.stop="toggleQuickFilter"
      >
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polygon points="22 3 2 3 10 12.46 10 19 14 21 14 12.46 22 3"/></svg>
        Quick Filter
      </button>
    </div>

    <Teleport to="body">
      <div
        v-if="contextMenu"
        class="msg-context-menu"
        :style="{ left: contextMenu.x + 'px', top: contextMenu.y + 'px' }"
      >
        <template v-if="isSingleSelection()">
          <button class="ctx-item" data-testid="ctx-reply" @click="ctxReply">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 17 4 12 9 7" /><path d="M20 18v-2a4 4 0 0 0-4-4H4" /></svg>
            Reply
          </button>
          <button class="ctx-item" data-testid="ctx-reply-all" @click="ctxReplyAll">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 17 4 12 9 7" /><path d="M20 18v-2a4 4 0 0 0-4-4H4" /></svg>
            Reply All
          </button>
          <button class="ctx-item" data-testid="ctx-forward" @click="ctxForward">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="15 17 20 12 15 7" /><path d="M4 18v-2a4 4 0 0 1 4-4h12" /></svg>
            Forward
          </button>
          <button class="ctx-item" data-testid="ctx-save-eml" @click="ctxSaveAsEml">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2z" /><polyline points="17 21 17 13 7 13 7 21" /><polyline points="7 3 7 8 15 8" /></svg>
            Save as .eml…
          </button>
          <div class="ctx-separator"></div>
        </template>

        <div class="ctx-item-parent" @mouseenter="subMenu = 'move'" @mouseleave="subMenu = null">
          <button class="ctx-item" data-testid="ctx-move-to" @click="subMenu = 'move'">
            Move To
            <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="9 18 15 12 9 6" /></svg>
          </button>
          <div v-if="subMenu === 'move'" class="ctx-submenu">
            <button
              v-for="folder in foldersStore.getFlatFolders()"
              :key="folder.path"
              class="ctx-item"
              :data-testid="`ctx-move-${folder.name}`"
              :class="{ disabled: folder.path === foldersStore.activeFolderPath }"
              @click="folder.path !== foldersStore.activeFolderPath && ctxMoveTo(folder.path)"
            >{{ folder.name }}</button>
          </div>
        </div>

        <div class="ctx-item-parent" @mouseenter="subMenu = 'copy'" @mouseleave="subMenu = null">
          <button class="ctx-item" data-testid="ctx-copy-to" @click="subMenu = 'copy'">
            Copy To
            <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="9 18 15 12 9 6" /></svg>
          </button>
          <div v-if="subMenu === 'copy'" class="ctx-submenu">
            <button
              v-for="folder in foldersStore.getFlatFolders()"
              :key="folder.path"
              class="ctx-item"
              :data-testid="`ctx-copy-${folder.name}`"
              @click="ctxCopyTo(folder.path)"
            >{{ folder.name }}</button>
          </div>
        </div>

        <div class="ctx-separator"></div>

        <button v-if="hasSelectedReadStatus(true)" class="ctx-item" data-testid="ctx-mark-unread" @click="ctxSetReadStatus(false)">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10" /><circle cx="12" cy="12" r="3" fill="currentColor" /></svg>
          Mark Unread
        </button>
        <button v-if="hasSelectedReadStatus(false)" class="ctx-item" data-testid="ctx-mark-read" @click="ctxSetReadStatus(true)">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10" /><path d="M9 12l2 2 4-4" /></svg>
          Mark Read
        </button>

        <button v-if="isJunkFolder()" class="ctx-item" data-testid="ctx-not-spam" @click="ctxNotSpam">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14" /><polyline points="22 4 12 14.01 9 11.01" /></svg>
          Not Spam
        </button>

        <button class="ctx-item danger" data-testid="ctx-delete" @click="ctxDelete">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /></svg>
          Delete
        </button>

        <div class="ctx-separator"></div>
        <button v-if="!uiStore.threadingEnabled" class="ctx-item" @click="ctxShowAsThread">Show as Thread</button>
        <button v-if="uiStore.threadingEnabled" class="ctx-item" @click="ctxUnthread">Remove from Thread</button>
      </div>
    </Teleport>
  </div>
</template>

<style scoped>
.message-list {
  width: 100%;
  min-width: 0;
  height: 100%;
  box-sizing: border-box;
  display: flex;
  flex-direction: column;
  overflow-x: hidden;
  background: var(--color-bg);
  border-right: 1px solid var(--color-border);
}

.column-headers {
  display: flex;
  align-items: center;
  height: 28px;
  border-bottom: 1px solid var(--color-border);
  background: var(--color-bg-secondary);
  font-size: 11px;
  color: var(--color-text-secondary);
  flex-shrink: 0;
  user-select: none;
}

.col {
  padding: 0 8px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.col-check {
  width: 28px;
  flex-shrink: 0;
  display: grid;
  place-items: center;
  cursor: pointer;
  align-self: stretch;
}

.row-checkbox {
  width: 13px;
  height: 13px;
  cursor: pointer;
  accent-color: var(--color-accent);
}

.col-icons {
  display: grid;
  place-items: center;
  width: 28px;
  flex-shrink: 0;
}

.col-icon {
  font-size: 10px;
  color: var(--color-text-muted);
}

.col-subject {
  flex: 2;
  min-width: 0;
  text-align: left;
}

.col-from {
  flex: 2;
  min-width: 0;
  text-align: left;
}

.col-date {
  width: 110px;
  flex-shrink: 0;
  text-align: left;
}

.sortable {
  transition: color 0.15s;
}

.sortable:hover {
  color: var(--color-text);
}

.sortable.active {
  color: var(--color-accent);
  font-weight: 600;
}

.message-items {
  flex: 1;
  overflow-y: auto;
  user-select: none;
  -webkit-user-select: none;
}

.thread-child {
  background: var(--color-bg-tertiary);
  /* Margin preserves the child layout while applying reply depth. */
  margin-left: calc(20px + var(--reply-depth, 0) * 22px);
  border-left: 2px solid var(--color-border);
}

.loading-more {
  padding: 8px;
  text-align: center;
  font-size: 11px;
  color: var(--color-text-muted);
}

.list-footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 4px 8px;
  border-top: 1px solid var(--color-border);
  font-size: 11px;
  color: var(--color-text-muted);
  flex-shrink: 0;
}

.server-results {
  border-top: 2px solid var(--color-border);
  background: var(--color-bg-secondary);
  max-height: 40vh;
  overflow-y: auto;
  flex-shrink: 0;
}

.server-results-header {
  padding: 6px 12px;
  font-size: 11px;
  font-weight: 600;
  color: var(--color-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.04em;
  border-bottom: 1px solid var(--color-border);
}

.server-hit-row {
  padding: 8px 12px;
  border-bottom: 1px solid var(--color-border);
  font-size: 12px;
  cursor: pointer;
  transition: background 0.12s;
}

.server-hit-row:hover,
.server-hit-row:focus-visible {
  background: var(--color-bg-hover);
  outline: none;
}

.hit-line {
  display: flex;
  justify-content: space-between;
  gap: 8px;
  color: var(--color-text);
}

.hit-from {
  font-weight: 500;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.hit-date {
  color: var(--color-text-muted);
  font-size: 11px;
  flex-shrink: 0;
}

.hit-subject {
  color: var(--color-text);
  margin-top: 2px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.hit-snippet {
  color: var(--color-text-muted);
  font-size: 11px;
  margin-top: 2px;
  overflow: hidden;
  text-overflow: ellipsis;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
}

.hit-folder {
  color: var(--color-text-muted);
  font-size: 10px;
  margin-top: 4px;
  font-style: italic;
}

.quick-filter-toggle {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 2px 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  color: var(--color-text-muted);
  font-size: 11px;
}

.quick-filter-toggle:hover {
  color: var(--color-text);
  background: var(--color-bg-hover);
}

.quick-filter-toggle.active {
  color: var(--color-accent);
  border-color: var(--color-accent);
}

.loading,
.empty {
  padding: 24px;
  text-align: center;
  color: var(--color-text-muted);
}
</style>

<style>
.msg-context-menu {
  position: fixed;
  z-index: 9999;
  background: var(--color-bg);
  border: 0.8px solid var(--color-border);
  border-radius: 8px;
  padding: 4px 0;
  min-width: 200px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.12);
}

.msg-context-menu .ctx-item {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 7px 14px;
  text-align: left;
  font-size: 13px;
  color: var(--color-text);
}

.msg-context-menu .ctx-item:hover {
  background: var(--color-bg-hover);
}

.msg-context-menu .ctx-item.danger {
  color: var(--color-danger-text);
}
.msg-context-menu .ctx-item.danger:hover {
  background: rgba(251, 44, 54, 0.06);
}

.msg-context-menu .ctx-item.disabled {
  opacity: 0.4;
  cursor: default;
}

.msg-context-menu .ctx-separator {
  height: 1px;
  background: var(--color-border);
  margin: 4px 0;
}

.msg-context-menu .ctx-item-parent {
  position: relative;
}

.msg-context-menu .ctx-item-parent > .ctx-item {
  justify-content: space-between;
}

.msg-context-menu .ctx-submenu {
  position: absolute;
  left: 100%;
  top: -4px;
  background: var(--color-bg);
  border: 0.8px solid var(--color-border);
  border-radius: 8px;
  padding: 4px 0;
  min-width: 180px;
  max-height: 300px;
  overflow-y: auto;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.12);
}

.msg-context-menu .ctx-submenu .ctx-item {
  font-size: 13px;
  padding: 6px 14px;
}
</style>
