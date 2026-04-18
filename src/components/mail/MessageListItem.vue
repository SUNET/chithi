<script setup lang="ts">
import type { MessageSummary } from "@/lib/types";

defineProps<{
  message: MessageSummary;
  active: boolean;
  selected: boolean;
}>();

defineEmits<{
  toggle: [];
  open: [];
  toggleStar: [];
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

function isReply(subject: string | null, flags: string[]): boolean {
  if (flags.includes("answered")) return true;
  if (!subject) return false;
  const lower = subject.trimStart().toLowerCase();
  return lower.startsWith("re:") || lower.startsWith("fwd:") || lower.startsWith("fw:");
}
</script>

<template>
  <div
    class="message-row"
    :class="{ active, selected, unread: isUnread(message.flags) }"
    @dblclick="$emit('open')"
  >
    <div class="col col-check">
      <input
        type="checkbox"
        class="row-checkbox"
        :checked="selected"
        @click.stop="$emit('toggle')"
      />
    </div>
    <div class="col col-icons">
      <span
        class="icon-read"
        :class="{ unread: isUnread(message.flags) }"
        data-testid="msg-unread-dot"
      >&#x25CF;</span>
      <span
        class="icon-star"
        :class="{ starred: isStarred(message.flags) }"
        data-testid="msg-star"
        :data-starred="isStarred(message.flags)"
        @click.stop="$emit('toggleStar')"
      >{{ isStarred(message.flags) ? '\u2605' : '\u2606' }}</span>
    </div>
    <div class="col col-subject" :class="{ bold: isUnread(message.flags) }">
      <span v-if="isReply(message.subject, message.flags)" class="reply-icon">&hookleftarrow;</span>
      <span class="subject-text" data-testid="msg-subject">{{ message.subject || "(no subject)" }}</span>
    </div>
    <div class="col col-from" :class="{ bold: isUnread(message.flags) }" data-testid="msg-from">
      {{ message.from_name || message.from_email }}
    </div>
    <div class="col col-date" data-testid="msg-date">
      {{ formatDate(message.date) }}
    </div>
  </div>
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
  cursor: default;
  user-select: none;
}

.message-row:hover {
  background: var(--color-bg-hover);
}

.message-row.active {
  background: var(--color-bg-active);
}

.message-row.selected {
  background: var(--color-bg-active);
  box-shadow: inset 3px 0 0 var(--color-accent);
}

.message-row.selected:hover {
  background: var(--color-bg-active);
}

.message-row.unread .col-subject,
.message-row.unread .col-from {
  font-weight: 700;
  color: var(--color-text);
}

.icon-star.starred {
  color: var(--color-star-flag);
}

.col {
  padding: 0 6px;
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
  padding: 0;
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
  padding: 0 4px;
}

.icon-read {
  font-size: 8px;
  color: var(--color-accent);
  visibility: hidden;
}

.icon-read.unread {
  visibility: visible;
}

.icon-star {
  font-size: 13px;
  color: var(--color-text-muted);
  cursor: pointer;
}

.col-subject {
  flex: 2;
  min-width: 0;
  color: var(--color-text-secondary);
  display: flex;
  align-items: center;
  gap: 4px;
}

.reply-icon {
  color: var(--color-accent);
  font-size: 13px;
  flex-shrink: 0;
}

.subject-text {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
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
