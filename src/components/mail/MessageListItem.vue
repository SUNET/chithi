<script setup lang="ts">
import { computed, ref } from "vue";
import type { MessageSummary } from "@/lib/types";
import { useUiStore } from "@/stores/ui";
import { useAccountsStore } from "@/stores/accounts";
import { acctColor } from "@/lib/account-colors";

const props = defineProps<{
  message: MessageSummary;
  active: boolean;
  selected: boolean;
  mode?: "desktop" | "mobile";
  accountId?: string;
}>();

const emit = defineEmits<{
  toggle: [];
  open: [];
  toggleStar: [];
  archive: [];
  delete: [];
}>();

const uiStore = useUiStore();
const accountsStore = useAccountsStore();

const resolvedMode = computed<"desktop" | "mobile">(() => props.mode ?? "desktop");

// Resolve the per-account color. If the caller passed an explicit accountId
// use that, otherwise fall back to the active account (the common case when
// the list is filtered to a single mailbox).
const acct = computed(() =>
  acctColor(props.accountId ?? accountsStore.activeAccountId ?? ""),
);

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

function senderInitial(msg: MessageSummary): string {
  const src = (msg.from_name ?? msg.from_email ?? "?").trim();
  return src.charAt(0).toUpperCase() || "?";
}

// --- Swipe gestures (§6.2) — mobile only ---
const SWIPE_THRESHOLD = 72; // px to commit the action
const translateX = ref(0);
const swiping = ref(false);
const pointerStartX = ref(0);
const pointerStartY = ref(0);
const axisLocked = ref<"x" | "y" | null>(null);

function onPointerDown(e: PointerEvent) {
  if (resolvedMode.value !== "mobile") return;
  // Only primary button / touch / pen; ignore right-click and mouse drags
  // that interfere with the desktop drag-to-move flow.
  if (e.pointerType === "mouse" && e.button !== 0) return;
  pointerStartX.value = e.clientX;
  pointerStartY.value = e.clientY;
  axisLocked.value = null;
  swiping.value = true;
  (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
}

function onPointerMove(e: PointerEvent) {
  if (!swiping.value) return;
  const dx = e.clientX - pointerStartX.value;
  const dy = e.clientY - pointerStartY.value;

  // Lock to an axis on first significant movement so vertical scroll still
  // works inside the list without the row fighting the scroll container.
  if (!axisLocked.value) {
    if (Math.abs(dx) < 6 && Math.abs(dy) < 6) return;
    axisLocked.value = Math.abs(dx) > Math.abs(dy) ? "x" : "y";
  }
  if (axisLocked.value !== "x") return;

  e.preventDefault();
  // Soft clamp so the row can't slide fully off — makes the reveal feel
  // attached to the finger but not runaway.
  const clamped = Math.max(-160, Math.min(160, dx));
  translateX.value = clamped;
}

function onPointerUp() {
  if (!swiping.value) return;
  swiping.value = false;
  const dx = translateX.value;
  if (dx >= SWIPE_THRESHOLD) {
    emit("archive");
  } else if (dx <= -SWIPE_THRESHOLD) {
    emit("delete");
  }
  translateX.value = 0;
  axisLocked.value = null;
}
</script>

<template>
  <!-- Desktop: unchanged 5-column row -->
  <div
    v-if="resolvedMode === 'desktop'"
    class="message-row"
    :class="{ active, selected, unread: isUnread(message.flags) }"
    @dblclick="emit('open')"
  >
    <div class="col col-check">
      <input
        type="checkbox"
        class="row-checkbox"
        :checked="selected"
        @click.stop="emit('toggle')"
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
        @click.stop="emit('toggleStar')"
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

  <!-- Mobile: 2-line comfortable row with swipe track -->
  <div
    v-else
    class="message-row-mobile-track"
    :class="{ active, selected, unread: isUnread(message.flags) }"
  >
    <div class="swipe-action swipe-archive" aria-hidden="true">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
        <polyline points="21 8 21 21 3 21 3 8" />
        <rect x="1" y="3" width="22" height="5" />
        <line x1="10" y1="12" x2="14" y2="12" />
      </svg>
    </div>
    <div class="swipe-action swipe-delete" aria-hidden="true">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
        <polyline points="3 6 5 6 21 6" />
        <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
      </svg>
    </div>

    <div
      class="message-row-mobile"
      :style="{ transform: 'translateX(' + translateX + 'px)', transition: swiping ? 'none' : 'transform 180ms cubic-bezier(.2,.8,.2,1)' }"
      @click="emit('open')"
      @pointerdown="onPointerDown"
      @pointermove="onPointerMove"
      @pointerup="onPointerUp"
      @pointercancel="onPointerUp"
    >
      <div class="mobile-avatar-wrap">
        <div
          class="mobile-avatar"
          :style="{ background: acct.soft, color: acct.fill, boxShadow: 'inset 0 0 0 1.5px ' + acct.fill }"
        >
          {{ senderInitial(message) }}
        </div>
        <span
          class="mobile-acct-dot"
          :style="{ background: acct.fill }"
          aria-hidden="true"
        />
      </div>

      <div class="mobile-body">
        <div class="mobile-line1">
          <span
            v-if="isUnread(message.flags)"
            class="mobile-unread-dot"
            aria-label="Unread"
            data-testid="msg-unread-dot"
          />
          <span class="mobile-sender" :class="{ unread: isUnread(message.flags) }" data-testid="msg-from">
            {{ message.from_name || message.from_email }}
          </span>
          <span class="mobile-time" :class="{ unread: isUnread(message.flags) }" data-testid="msg-date">
            {{ formatDate(message.date) }}
          </span>
        </div>
        <div class="mobile-line2" :class="{ unread: isUnread(message.flags) }">
          <span v-if="isReply(message.subject, message.flags)" class="reply-icon">&hookleftarrow;</span>
          <span class="mobile-subject" data-testid="msg-subject">
            {{ message.subject || "(no subject)" }}
          </span>
        </div>
        <div class="mobile-line3">
          <svg
            v-if="message.has_attachments"
            class="mobile-paperclip"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.6"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
          >
            <path d="M21.44 11.05l-9.19 9.19a6 6 0 1 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48" />
          </svg>
          <span class="mobile-preview">{{ message.snippet ?? "" }}</span>
          <span
            v-if="isStarred(message.flags)"
            class="mobile-star"
            data-testid="msg-star"
            :data-starred="true"
            @click.stop="emit('toggleStar')"
          >&#9733;</span>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* ============================================================
   Desktop — unchanged
   ============================================================ */
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

/* ============================================================
   Mobile — 2-line row with swipe reveal
   ============================================================ */
.message-row-mobile-track {
  position: relative;
  overflow: hidden;
  border-bottom: 1px solid var(--color-border);
  background: var(--color-bg);
}

.message-row-mobile-track.active,
.message-row-mobile-track.selected {
  background: var(--color-bg-active);
  box-shadow: inset 3px 0 0 var(--color-accent);
}

.message-row-mobile {
  position: relative;
  z-index: 1;
  display: flex;
  align-items: flex-start;
  gap: 12px;
  padding: 10px 14px;
  background: inherit;
  touch-action: pan-y;
  cursor: pointer;
  user-select: none;
}

.mobile-avatar-wrap {
  position: relative;
  flex-shrink: 0;
  width: 40px;
  height: 40px;
}

.mobile-avatar {
  width: 40px;
  height: 40px;
  border-radius: 50%;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 15px;
  font-weight: 600;
}

.mobile-acct-dot {
  position: absolute;
  right: -1px;
  bottom: -1px;
  width: 14px;
  height: 14px;
  border-radius: 50%;
  box-shadow: 0 0 0 2px var(--color-bg);
}

.mobile-body {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.mobile-line1 {
  display: flex;
  align-items: center;
  gap: 6px;
}

.mobile-unread-dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--color-accent);
  flex-shrink: 0;
}

.mobile-sender {
  flex: 1;
  min-width: 0;
  font-size: 15px;
  font-weight: 500;
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.mobile-sender.unread {
  font-weight: 700;
}

.mobile-time {
  flex-shrink: 0;
  font-size: 12px;
  color: var(--color-text-muted);
}

.mobile-time.unread {
  color: var(--color-accent);
  font-weight: 600;
}

.mobile-line2 {
  display: flex;
  align-items: baseline;
  gap: 4px;
  font-size: 14px;
  font-weight: 500;
  color: var(--color-text);
}

.mobile-line2.unread {
  font-weight: 600;
}

.mobile-subject {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.mobile-line3 {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
  color: var(--color-text-muted);
}

.mobile-paperclip {
  width: 13px;
  height: 13px;
  flex-shrink: 0;
}

.mobile-preview {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.mobile-star {
  flex-shrink: 0;
  font-size: 15px;
  color: var(--color-star-flag);
}

/* Swipe-reveal backgrounds — color the track under the translating row. */
.swipe-action {
  position: absolute;
  top: 0;
  bottom: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 72px;
  color: #fff;
}

.swipe-action svg {
  width: 22px;
  height: 22px;
}

.swipe-archive {
  left: 0;
  background: #8b9d62;
}

.swipe-delete {
  right: 0;
  background: #c36b4b;
}
</style>
