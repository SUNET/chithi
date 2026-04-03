<script setup lang="ts">
import { ref } from "vue";
import { useMessagesStore } from "@/stores/messages";
import type { SortColumn } from "@/stores/messages";
import MessageListItem from "./MessageListItem.vue";

const messagesStore = useMessagesStore();
const scrollContainer = ref<HTMLElement | null>(null);

const emit = defineEmits<{
  openMessage: [messageId: string];
}>();

function onSelect(messageId: string) {
  messagesStore.loadMessage(messageId);
}

function onOpen(messageId: string) {
  messagesStore.loadMessage(messageId);
  emit("openMessage", messageId);
}

function sortIndicator(column: SortColumn): string {
  if (messagesStore.sortColumn !== column) return "";
  return messagesStore.sortAsc ? " \u25B4" : " \u25BE";
}

function onScroll() {
  const el = scrollContainer.value;
  if (!el) return;
  // Load more when within 200px of the bottom
  if (el.scrollHeight - el.scrollTop - el.clientHeight < 200) {
    messagesStore.loadNextPage();
  }
}
</script>

<template>
  <div class="message-list">
    <div class="column-headers">
      <div class="col col-icons">
        <span class="col-icon" title="Read status">&#x25CF;</span>
        <span class="col-icon" title="Starred">&#x2606;</span>
        <span class="col-icon" title="Attachment">&#x1F4CE;</span>
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
    <div v-if="messagesStore.loading && messagesStore.messages.length === 0" class="loading">Loading...</div>
    <div v-else-if="messagesStore.messages.length === 0" class="empty">
      No messages
    </div>
    <div v-else ref="scrollContainer" class="message-items" @scroll="onScroll">
      <MessageListItem
        v-for="msg in messagesStore.messages"
        :key="msg.id"
        :message="msg"
        :active="messagesStore.activeMessageId === msg.id"
        @select="onSelect(msg.id)"
        @open="onOpen(msg.id)"
      />
      <div v-if="messagesStore.loadingMore" class="loading-more">
        Loading more...
      </div>
    </div>
    <div class="list-footer">
      <span class="message-count">{{ messagesStore.messages.length }} of {{ messagesStore.total }}</span>
    </div>
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

.col-icons {
  display: flex;
  align-items: center;
  gap: 2px;
  width: 60px;
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
