<script setup lang="ts">
import type { MessageSummary } from "@/lib/types";

defineProps<{
  message: MessageSummary;
  active: boolean;
}>();

const emit = defineEmits<{
  select: [];
  open: [];
}>();

function formatDate(dateStr: string): string {
  const date = new Date(dateStr);
  const now = new Date();
  const isToday = date.toDateString() === now.toDateString();
  if (isToday) {
    return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
  const isThisYear = date.getFullYear() === now.getFullYear();
  if (isThisYear) {
    return date.toLocaleDateString([], { month: "short", day: "numeric" });
  }
  return date.toLocaleDateString([], {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

function isUnread(flags: string[]): boolean {
  return !flags.includes("seen");
}

function isStarred(flags: string[]): boolean {
  return flags.includes("flagged");
}
</script>

<template>
  <button
    class="message-row"
    :class="{ active, unread: isUnread(message.flags) }"
    @click="emit('select')"
    @dblclick="emit('open')"
  >
    <div class="col col-icons">
      <span
        class="icon-read"
        :class="{ unread: isUnread(message.flags) }"
        :title="isUnread(message.flags) ? 'Unread' : 'Read'"
      >&#x25CF;</span>
      <span
        class="icon-star"
        :class="{ starred: isStarred(message.flags) }"
        :title="isStarred(message.flags) ? 'Starred' : 'Not starred'"
      >{{ isStarred(message.flags) ? '\u2605' : '\u2606' }}</span>
      <span
        v-if="message.has_attachments"
        class="icon-attachment"
        title="Has attachments"
      >&#x1F4CE;</span>
      <span v-else class="icon-attachment-spacer"></span>
    </div>
    <div class="col col-subject" :class="{ bold: isUnread(message.flags) }">
      {{ message.subject || "(no subject)" }}
    </div>
    <div class="col col-from" :class="{ bold: isUnread(message.flags) }">
      {{ message.from_name || message.from_email }}
    </div>
    <div class="col col-date">
      {{ formatDate(message.date) }}
    </div>
  </button>
</template>

<style scoped>
.message-row {
  display: flex;
  align-items: center;
  width: 100%;
  height: 28px;
  text-align: left;
  border-bottom: 1px solid var(--color-border);
  font-size: 12px;
  transition: background 0.1s;
}

.message-row:hover {
  background: var(--color-bg-hover);
}

.message-row.active {
  background: var(--color-bg-active);
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
  padding: 0 4px;
}

.icon-read {
  font-size: 8px;
  color: transparent;
}

.icon-read.unread {
  color: var(--color-accent);
}

.icon-star {
  font-size: 12px;
  color: var(--color-text-muted);
}

.icon-star.starred {
  color: var(--color-warning);
}

.icon-attachment {
  font-size: 11px;
}

.icon-attachment-spacer {
  width: 11px;
  display: inline-block;
}

.col-subject {
  flex: 2;
  min-width: 0;
  color: var(--color-text-secondary);
}

.col-from {
  flex: 2;
  min-width: 0;
  color: var(--color-text-secondary);
}

.col-date {
  width: 110px;
  flex-shrink: 0;
  color: var(--color-text-muted);
  font-size: 11px;
}

.bold {
  font-weight: 600;
  color: var(--color-text);
}
</style>
