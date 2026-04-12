<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from "vue";
import { dragMessageIds, dragSourceAccountId, isDragging } from "@/lib/drag-state";
import { showToast, dismissToast } from "@/lib/toast";
import { useMessagesStore } from "@/stores/messages";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useUiStore } from "@/stores/ui";
import type { SortColumn } from "@/stores/messages";
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

// Track modifier keys independently via keydown/keyup since
// WebKitGTK can lose event.shiftKey on click events.
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
    messagesStore.quickFilterVisible = !messagesStore.quickFilterVisible;
    if (!messagesStore.quickFilterVisible) {
      messagesStore.clearQuickFilters();
    }
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

// Right-click context menu
const contextMenu = ref<{
  x: number;
  y: number;
  messageId: string;
  threadId?: string;
} | null>(null);
const subMenu = ref<"move" | "copy" | null>(null);

const isSingleSelection = () => messagesStore.selectedIds.length <= 1;

function onChildSelect(messageId: string) {
  closeContextMenu();
  messagesStore.selectMessage(messageId, { shiftKey: false, ctrlKey: false, metaKey: false });
}

function onSelect(messageId: string, event?: MouseEvent) {
  closeContextMenu();
  // Check both keyboard tracker AND MouseEvent for modifier keys.
  // WebKitGTK may lose one or the other depending on context.
  const isShift = shiftHeld.value || (event?.shiftKey ?? false);
  const isCtrl = ctrlHeld.value || (event?.ctrlKey ?? false) || (event?.metaKey ?? false);
  messagesStore.selectMessage(messageId, {
    shiftKey: isShift,
    ctrlKey: isCtrl,
    metaKey: false,
  });
}

// Custom drag-and-drop via mouse events (WebKitGTK doesn't support HTML5 DnD)
const dragStartPos = ref<{ x: number; y: number } | null>(null);
const dragSourceId = ref<string | null>(null);
const dragGhost = ref<HTMLElement | null>(null);
const DRAG_THRESHOLD = 5; // pixels before drag starts

function onDragMouseDown(event: MouseEvent, messageId: string) {
  if (event.button !== 0) return;
  if ((event.target as HTMLElement).closest(".row-checkbox")) return;
  dragStartPos.value = { x: event.clientX, y: event.clientY };
  dragSourceId.value = messageId;

  const handleMove = (e: MouseEvent) => {
    if (!dragStartPos.value || !dragSourceId.value) return;

    const dx = e.clientX - dragStartPos.value.x;
    const dy = e.clientY - dragStartPos.value.y;
    if (!isDragging.value && Math.sqrt(dx * dx + dy * dy) < DRAG_THRESHOLD) return;

    if (!isDragging.value) {
      // Select for drag without triggering read/open side effects
      if (!messagesStore.isSelected(dragSourceId.value)) {
        messagesStore.selectedIds = [dragSourceId.value];
      }
      dragMessageIds.value = messagesStore.resolveSelectedIds();
      dragSourceAccountId.value = accountsStore.activeAccountId;
      isDragging.value = true;

      // Ghost badge showing drag count — positioned away from cursor
      // so it doesn't intercept mouse events on drop targets
      const ghost = document.createElement("div");
      ghost.textContent = `${dragMessageIds.value.length} message(s)`;
      ghost.style.cssText = "position:fixed;z-index:99999;padding:4px 10px;background:#3366cc;color:white;border-radius:4px;font-size:12px;font-weight:500;white-space:nowrap;pointer-events:none;";
      document.body.appendChild(ghost);
      dragGhost.value = ghost;
      // Test: also change body cursor
      document.body.style.cursor = "grabbing";
      document.body.classList.add("dragging-messages");
    }

    if (dragGhost.value) {
      dragGhost.value.style.left = e.clientX + 12 + "px";
      dragGhost.value.style.top = e.clientY + 12 + "px";
    }
  };

  const handleUp = () => {
    document.body.style.cursor = "";
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
  // Ensure right-clicked message is in selection
  if (!messagesStore.selectedIds.includes(messageId)) {
    messagesStore.selectMessage(messageId, { shiftKey: false, ctrlKey: false, metaKey: false });
  }
  contextMenu.value = { x: event.clientX, y: event.clientY, messageId };
  subMenu.value = null;
}

async function ctxReply() {
  const msgId = contextMenu.value?.messageId;
  closeContextMenu();
  if (!msgId) return;
  await messagesStore.loadMessage(msgId);
  const msg = messagesStore.activeMessage;
  if (!msg) return;
  const body = msg.body_text || "";
  const date = new Date(msg.date).toLocaleString();
  const from = msg.from.name ? `${msg.from.name} <${msg.from.email}>` : msg.from.email;
  const quoted = body.split("\n").map((l: string) => `> ${l}`).join("\n");
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    replyTo: msgId,
    to: msg.from.email,
    subject: msg.subject?.startsWith("Re:") ? msg.subject : `Re: ${msg.subject || ""}`,
    body: `\n\nOn ${date}, ${from} wrote:\n${quoted}`,
  });
}

async function ctxReplyAll() {
  const msgId = contextMenu.value?.messageId;
  closeContextMenu();
  if (!msgId) return;
  await messagesStore.loadMessage(msgId);
  const msg = messagesStore.activeMessage;
  if (!msg) return;
  const myEmail = accountsStore.activeAccount()?.email ?? "";
  const allTo = [msg.from.email, ...msg.to.map((a: { email: string }) => a.email).filter((e: string) => e !== myEmail)];
  const allCc = msg.cc.map((a: { email: string }) => a.email).filter((e: string) => e !== myEmail);
  const body = msg.body_text || "";
  const date = new Date(msg.date).toLocaleString();
  const from = msg.from.name ? `${msg.from.name} <${msg.from.email}>` : msg.from.email;
  const quoted = body.split("\n").map((l: string) => `> ${l}`).join("\n");
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    replyTo: msgId,
    to: allTo.join(", "),
    cc: allCc.join(", "),
    subject: msg.subject?.startsWith("Re:") ? msg.subject : `Re: ${msg.subject || ""}`,
    body: `\n\nOn ${date}, ${from} wrote:\n${quoted}`,
  });
}

async function ctxForward() {
  const msgId = contextMenu.value?.messageId;
  closeContextMenu();
  if (!msgId) return;
  await messagesStore.loadMessage(msgId);
  const msg = messagesStore.activeMessage;
  if (!msg) return;
  const text = msg.body_text || "";
  const date = new Date(msg.date).toLocaleString();
  const from = msg.from.name ? `${msg.from.name} <${msg.from.email}>` : msg.from.email;
  const toStr = msg.to.map((a: { name: string | null; email: string }) => a.name || a.email).join(", ");
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    subject: msg.subject?.startsWith("Fwd:") ? msg.subject : `Fwd: ${msg.subject || ""}`,
    body: `\n\n---------- Forwarded message ----------\nFrom: ${from}\nDate: ${date}\nSubject: ${msg.subject || ""}\nTo: ${toStr}\n\n${text}`,
  });
}

async function ctxMoveTo(folderPath: string) {
  const accountId = accountsStore.activeAccountId;
  if (!accountId) return;
  const ids = messagesStore.resolveSelectedIds();
  closeContextMenu();
  const toastId = showToast(`Moving ${ids.length} message(s)...`, "info", 0);
  try {
    await api.moveMessages(accountId, ids, folderPath);
    messagesStore.clearSelection();
    messagesStore.activeMessage = null;
    messagesStore.activeMessageId = null;
  } catch (e) {
    console.error("Move failed:", e);
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
  } catch (e) {
    console.error("Copy failed:", e);
  }
}

async function ctxDelete() {
  closeContextMenu();
  await messagesStore.deleteSelected();
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
    messagesStore.clearSelection();
    messagesStore.activeMessage = null;
    messagesStore.activeMessageId = null;
  } catch (e) {
    console.error("Not Spam failed:", e);
  }
}

const allSelected = computed(() => {
  if (uiStore.threadingEnabled) {
    return messagesStore.threads.length > 0 &&
      messagesStore.threads.every(t => messagesStore.isSelected(t.message_ids[0]));
  }
  return messagesStore.messages.length > 0 &&
    messagesStore.messages.every(m => messagesStore.isSelected(m.id));
});

function toggleSelectAll() {
  if (allSelected.value) {
    messagesStore.clearSelection();
  } else {
    if (uiStore.threadingEnabled) {
      messagesStore.selectedIds = messagesStore.threads.map(t => t.message_ids[0]);
    } else {
      messagesStore.selectedIds = messagesStore.messages.map(m => m.id);
    }
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
</script>

<template>
  <div class="message-list" data-testid="message-list" @click="closeContextMenu">
    <QuickFilterBar v-if="messagesStore.quickFilterVisible" />
    <div class="column-headers">
      <div class="col col-check">
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

    <!-- Threaded view -->
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
            :expanded="messagesStore.expandedThreads.includes(thread.thread_id)"
            :active="thread.message_ids.includes(messagesStore.activeMessageId ?? '')"
            :selected="messagesStore.isSelected(thread.message_ids[0])"
            @toggle="messagesStore.toggleThread(thread.thread_id)"
            @toggle-select="messagesStore.toggleSelectMessage(thread.message_ids[0])"
            @open="onThreadOpen(thread)"
            @toggle-star="messagesStore.toggleStar(thread.message_ids[0])"
          />
        </div>
        <!-- Expanded thread messages -->
        <template v-if="messagesStore.expandedThreads.includes(thread.thread_id)">
          <div
            v-for="msg in (messagesStore.threadMessages[thread.thread_id] ?? []).slice(1)"
            :key="msg.id"
            class="thread-child"
            @click.stop="onChildSelect(msg.id)"
            @contextmenu.prevent.stop="onRowRightClick($event, msg.id)"
            @mousedown="onDragMouseDown($event, msg.id)"
          >
            <MessageListItem
              :message="msg"
              :active="messagesStore.activeMessageId === msg.id"
              :selected="messagesStore.isSelected(msg.id)"
              @toggle="messagesStore.toggleSelectMessage(msg.id)"
              @open="onOpen(msg.id)"
              @toggle-star="messagesStore.toggleStar(msg.id)"
            />
          </div>
        </template>
      </template>
      <div v-if="messagesStore.loadingMore" class="loading-more">Loading more...</div>
    </div>

    <!-- Flat view -->
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
          @toggle="messagesStore.toggleSelectMessage(msg.id)"
          @open="onOpen(msg.id)"
          @toggle-star="messagesStore.toggleStar(msg.id)"
        />
      </div>
      <div v-if="messagesStore.loadingMore" class="loading-more">Loading more...</div>
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

    <!-- Right-click context menu -->
    <Teleport to="body">
      <div
        v-if="contextMenu"
        class="msg-context-menu"
        :style="{ left: contextMenu.x + 'px', top: contextMenu.y + 'px' }"
      >
        <!-- Single message actions -->
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
          <div class="ctx-separator"></div>
        </template>

        <!-- Move To submenu -->
        <div class="ctx-item-parent" @mouseenter="subMenu = 'move'" @mouseleave="subMenu = null">
          <button class="ctx-item" data-testid="ctx-move-to">
            Move To
            <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="9 18 15 12 9 6" /></svg>
          </button>
          <div v-if="subMenu === 'move'" class="ctx-submenu">
            <button
              v-for="folder in foldersStore.getFlatFolders()"
              :key="folder.path"
              class="ctx-item"
              :class="{ disabled: folder.path === foldersStore.activeFolderPath }"
              @click="folder.path !== foldersStore.activeFolderPath && ctxMoveTo(folder.path)"
            >{{ folder.name }}</button>
          </div>
        </div>

        <!-- Copy To submenu -->
        <div class="ctx-item-parent" @mouseenter="subMenu = 'copy'" @mouseleave="subMenu = null">
          <button class="ctx-item" data-testid="ctx-copy-to">
            Copy To
            <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="9 18 15 12 9 6" /></svg>
          </button>
          <div v-if="subMenu === 'copy'" class="ctx-submenu">
            <button
              v-for="folder in foldersStore.getFlatFolders()"
              :key="folder.path"
              class="ctx-item"
              @click="ctxCopyTo(folder.path)"
            >{{ folder.name }}</button>
          </div>
        </div>

        <div class="ctx-separator"></div>

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
  height: 100%;
  display: flex;
  flex-direction: column;
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
  width: 20px;
  flex-shrink: 0;
  display: flex;
  align-items: center;
  justify-content: center;
}

.row-checkbox {
  width: 13px;
  height: 13px;
  cursor: pointer;
  accent-color: var(--color-accent);
}

.col-icons {
  display: flex;
  align-items: center;
  gap: 2px;
  width: 28px;
  flex-shrink: 0;
  justify-content: center;
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
  cursor: pointer;
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
  padding-left: 20px;
  background: var(--color-bg-tertiary);
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
  cursor: pointer;
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
  background: none;
  border: none;
  cursor: pointer;
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
