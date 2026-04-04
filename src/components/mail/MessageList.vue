<script setup lang="ts">
import { ref, onMounted, onUnmounted } from "vue";
import { useMessagesStore } from "@/stores/messages";
import { useUiStore } from "@/stores/ui";
import type { SortColumn } from "@/stores/messages";
import MessageListItem from "./MessageListItem.vue";
import ThreadRow from "./ThreadRow.vue";

const messagesStore = useMessagesStore();
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

function onRowContextMenu(event: MouseEvent, messageId: string) {
  event.preventDefault();
  contextMenu.value = { x: event.clientX, y: event.clientY, messageId };
}

function closeContextMenu() {
  contextMenu.value = null;
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

const displayedCount = () => {
  if (uiStore.threadingEnabled) {
    return `${messagesStore.threads.length} of ${messagesStore.totalThreads} threads (${messagesStore.total} messages)`;
  }
  return `${messagesStore.messages.length} of ${messagesStore.total}`;
};
</script>

<template>
  <div class="message-list" @click="closeContextMenu">
    <div class="column-headers">
      <div class="col col-check">
        <!-- select-all checkbox could go here -->
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
          @contextmenu.prevent="onRowContextMenu($event, thread.message_ids[0])"
        >
          <ThreadRow
            :thread="thread"
            :expanded="messagesStore.expandedThreads.includes(thread.thread_id)"
            :active="thread.message_ids.includes(messagesStore.activeMessageId ?? '')"
            :selected="messagesStore.isSelected(thread.message_ids[0])"
            @toggle="messagesStore.toggleThread(thread.thread_id)"
            @toggle-select="messagesStore.toggleSelectMessage(thread.message_ids[0])"
            @open="onThreadOpen(thread)"
          />
        </div>
        <!-- Expanded thread messages -->
        <template v-if="messagesStore.expandedThreads.includes(thread.thread_id)">
          <div
            v-for="msg in (messagesStore.threadMessages[thread.thread_id] ?? []).slice(1)"
            :key="msg.id"
            class="thread-child"
            @click.stop="onChildSelect(msg.id)"
            @contextmenu.prevent.stop="onRowContextMenu($event, msg.id)"
          >
            <MessageListItem
              :message="msg"
              :active="messagesStore.activeMessageId === msg.id"
              :selected="messagesStore.isSelected(msg.id)"
              @toggle="messagesStore.toggleSelectMessage(msg.id)"
              @open="onOpen(msg.id)"
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
        @contextmenu.prevent="onRowContextMenu($event, msg.id)"
      >
        <MessageListItem
          :message="msg"
          :active="messagesStore.activeMessageId === msg.id"
          :selected="messagesStore.isSelected(msg.id)"
          @toggle="messagesStore.toggleSelectMessage(msg.id)"
          @open="onOpen(msg.id)"
        />
      </div>
      <div v-if="messagesStore.loadingMore" class="loading-more">Loading more...</div>
    </div>

    <div class="list-footer">
      <span class="message-count">{{ displayedCount() }}</span>
    </div>

    <!-- Right-click context menu -->
    <Teleport to="body">
      <div
        v-if="contextMenu"
        class="msg-context-menu"
        :style="{ left: contextMenu.x + 'px', top: contextMenu.y + 'px' }"
      >
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
  width: 24px;
  flex-shrink: 0;
}

.col-icons {
  display: flex;
  align-items: center;
  gap: 4px;
  width: 40px;
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
  padding: 4px 8px;
  border-top: 1px solid var(--color-border);
  font-size: 11px;
  color: var(--color-text-muted);
  flex-shrink: 0;
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
  background: var(--color-bg-secondary);
  border: 1px solid var(--color-border);
  border-radius: 6px;
  padding: 4px 0;
  min-width: 180px;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
}

.msg-context-menu .ctx-item {
  display: block;
  width: 100%;
  padding: 6px 16px;
  text-align: left;
  font-size: 12px;
  color: var(--color-text);
  background: none;
  border: none;
  cursor: pointer;
}

.msg-context-menu .ctx-item:hover {
  background: var(--color-bg-hover);
}
</style>
