<script setup lang="ts">
import type { ThreadSummary } from "@/lib/types";
import { useUiStore } from "@/stores/ui";

const props = defineProps<{
  thread: ThreadSummary;
  expanded: boolean;
  active: boolean;
  selected: boolean;
}>();

defineEmits<{
  toggle: [];
  open: [];
  toggleSelect: [];
  toggleStar: [];
}>();

const uiStore = useUiStore();

function formatDate(dateStr: string): string {
  const date = new Date(dateStr);
  const now = new Date();
  const isToday = date.toDateString() === now.toDateString();
  if (isToday) {
    return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: uiStore.hour12 });
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

function hasUnread(): boolean {
  return props.thread.unread_count > 0;
}

function isStarred(): boolean {
  return props.thread.flags.includes("flagged");
}

function isReply(): boolean {
  if (props.thread.flags.includes("answered")) return true;
  if (!props.thread.subject) return false;
  const lower = props.thread.subject.trimStart().toLowerCase();
  return lower.startsWith("re:") || lower.startsWith("fwd:") || lower.startsWith("fw:");
}
</script>

<template>
  <div
    class="thread-row"
    :class="{ active, selected, unread: hasUnread() }"
    @dblclick="$emit('open')"
  >
    <div class="col col-check">
      <input
        type="checkbox"
        class="row-checkbox"
        :checked="selected"
        @click.stop="$emit('toggleSelect')"
      />
    </div>
    <div class="col col-icons">
      <span
        class="expand-icon"
        @click.stop="$emit('toggle')"
      >{{ thread.message_count > 1 ? (expanded ? '\u25BF' : '\u25B9') : '\u00A0' }}</span>
      <span
        class="icon-read"
        :class="{ unread: hasUnread() }"
        data-testid="msg-unread-dot"
      >&#x25CF;</span>
      <span
        class="icon-star"
        :class="{ starred: isStarred() }"
        data-testid="msg-star"
        :data-starred="isStarred()"
        @click.stop="$emit('toggleStar')"
      >{{ isStarred() ? '\u2605' : '\u2606' }}</span>
    </div>
    <div class="col col-subject" :class="{ bold: hasUnread() && !expanded }">
      <span v-if="isReply()" class="reply-icon">&hookleftarrow;</span>
      <span class="subject-text" data-testid="msg-subject">{{ thread.subject || "(no subject)" }}</span>
      <span v-if="thread.message_count > 1" class="thread-count">({{ thread.message_count }})</span>
    </div>
    <div class="col col-from" :class="{ bold: hasUnread() && !expanded }" data-testid="msg-from">
      {{ thread.from_name || thread.from_email }}
    </div>
    <div class="col col-date" data-testid="msg-date">
      {{ formatDate(thread.last_date) }}
    </div>
  </div>
</template>

<style scoped>
.thread-row {
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

.thread-row:hover {
  background: var(--color-bg-hover);
}

.thread-row.active {
  background: var(--color-bg-active);
}

.thread-row.selected {
  background: var(--color-bg-active);
  box-shadow: inset 3px 0 0 var(--color-accent);
}

.thread-row.selected:hover {
  background: var(--color-bg-active);
}

.thread-row.unread .col-subject,
.thread-row.unread .col-from {
  font-weight: 700;
  color: var(--color-text);
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
  width: 46px;
  flex-shrink: 0;
  padding: 0 2px;
}

.icon-read {
  font-size: 8px;
  color: var(--color-accent);
  visibility: hidden;
}

.icon-read.unread {
  visibility: visible;
}

.expand-icon {
  font-size: 11px;
  color: var(--color-text-muted);
  cursor: pointer;
  width: 14px;
  text-align: center;
}

.expand-icon:hover {
  color: var(--color-accent);
}

.icon-star {
  font-size: 13px;
  color: var(--color-text-muted);
  cursor: pointer;
}

.icon-star.starred {
  color: var(--color-star-flag);
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

.thread-count {
  font-size: 10px;
  color: var(--color-text-muted);
  flex-shrink: 0;
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
