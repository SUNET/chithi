<script setup lang="ts">
import { computed, onMounted, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { storeToRefs } from "pinia";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useMessagesStore } from "@/stores/messages";
import { useUiStore } from "@/stores/ui";
import MessageReader from "@/components/mail/MessageReader.vue";
import MobileIconButton from "@/components/mobile/MobileIconButton.vue";
import { acctColor } from "@/lib/account-colors";
import * as api from "@/lib/tauri";

const route = useRoute();
const router = useRouter();
const accountsStore = useAccountsStore();
const foldersStore = useFoldersStore();
const messagesStore = useMessagesStore();
const uiStore = useUiStore();

const { activeMessage } = storeToRefs(messagesStore);

async function loadFromRoute() {
  const rawId = route.params.id;
  const messageId = Array.isArray(rawId) ? rawId[0] : rawId;
  if (!messageId) return;
  try {
    if (accountsStore.accounts.length === 0) {
      await accountsStore.fetchAccounts();
    }
    if (foldersStore.folders.length === 0 && accountsStore.activeAccountId) {
      await foldersStore.fetchFolders();
    }
    await messagesStore.loadMessage(messageId);
  } catch (e) {
    console.error("MobileThreadView: loadMessage failed", e);
  }
}

onMounted(loadFromRoute);
watch(() => route.params.id, loadFromRoute);

function onClose() {
  if (window.history.length > 1) {
    router.back();
  } else {
    router.replace("/");
  }
}

const activeAccountId = computed(() => accountsStore.activeAccountId ?? "");
const acct = computed(() => acctColor(activeAccountId.value));

const accountTypeLabel = computed(() => {
  const acc = accountsStore.accounts.find((a) => a.id === activeAccountId.value);
  if (!acc) return "";
  if (acc.provider === "gmail") return "GMAIL";
  if (acc.provider === "o365") return "MICROSOFT 365";
  return (acc.mail_protocol ?? "").toUpperCase();
});

const starred = computed(() =>
  (activeMessage.value?.flags ?? []).includes("flagged"),
);

const subject = computed(() => activeMessage.value?.subject ?? "(no subject)");

const headerMeta = computed(() => {
  if (!activeMessage.value) return "";
  const d = new Date(activeMessage.value.date);
  const isToday = d.toDateString() === new Date().toDateString();
  const whenLabel = isToday
    ? "today"
    : d.toLocaleDateString([], { month: "short", day: "numeric" });
  return `1 message · ${whenLabel}`;
});

function toggleStar() {
  if (!activeMessage.value) return;
  messagesStore.toggleStar(activeMessage.value.id);
}

async function archiveThread() {
  const msg = activeMessage.value;
  if (!msg || !activeAccountId.value) return;
  const archive = foldersStore.folders.find(
    (f) => f.path.toLowerCase() === "archive" || f.name.toLowerCase() === "archive",
  );
  try {
    if (archive) {
      await api.moveMessages(activeAccountId.value, [msg.id], archive.path);
    } else {
      await api.setMessageFlags(activeAccountId.value, [msg.id], ["seen"], true);
    }
  } catch (e) {
    console.error("archive failed", e);
  }
  onClose();
}

async function deleteThread() {
  const msg = activeMessage.value;
  if (!msg || !activeAccountId.value) return;
  messagesStore.selectedIds.splice(0, messagesStore.selectedIds.length, msg.id);
  try {
    await messagesStore.deleteSelected();
  } catch (e) {
    console.error("delete failed", e);
  }
  onClose();
}

function doReply() {
  uiStore.openCompose({ replyTo: activeMessage.value?.id ?? null, kind: "reply" });
}

function doReplyAll() {
  uiStore.openCompose({ replyTo: activeMessage.value?.id ?? null, kind: "reply-all" });
}

function doForward() {
  uiStore.openCompose({ replyTo: activeMessage.value?.id ?? null, kind: "forward" });
}
</script>

<template>
  <div class="mobile-thread-view">
    <!-- Top app bar: back + archive + trash + overflow -->
    <header class="thread-bar">
      <MobileIconButton aria-label="Back" @click="onClose">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="15 18 9 12 15 6" />
        </svg>
      </MobileIconButton>
      <div class="bar-spacer" />
      <MobileIconButton aria-label="Archive" @click="archiveThread">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="21 8 21 21 3 21 3 8" />
          <rect x="1" y="3" width="22" height="5" />
          <line x1="10" y1="12" x2="14" y2="12" />
        </svg>
      </MobileIconButton>
      <MobileIconButton aria-label="Delete" @click="deleteThread">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="3 6 5 6 21 6" />
          <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
        </svg>
      </MobileIconButton>
      <MobileIconButton aria-label="More">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="12" cy="5" r="1.5" />
          <circle cx="12" cy="12" r="1.5" />
          <circle cx="12" cy="19" r="1.5" />
        </svg>
      </MobileIconButton>
    </header>

    <!-- Subject block -->
    <section class="subject-block">
      <div class="subject-line">
        <h1 class="subject">{{ subject }}</h1>
        <button
          class="subject-star"
          :class="{ on: starred }"
          :aria-label="starred ? 'Unstar' : 'Star'"
          @click="toggleStar"
        >
          {{ starred ? "\u2605" : "\u2606" }}
        </button>
      </div>
      <div class="subject-meta">
        <span
          v-if="accountTypeLabel"
          class="acct-chip"
          :style="{ background: acct.soft, color: acct.fill, boxShadow: 'inset 0 0 0 1px ' + acct.fill }"
        >
          {{ accountTypeLabel }}
        </span>
        <span class="meta-text">{{ headerMeta }}</span>
      </div>
    </section>

    <!-- Body: reuse MessageReader -->
    <div class="thread-body">
      <MessageReader :standalone="false" @close="onClose" />
    </div>

    <!-- Sticky reply dock -->
    <div class="reply-dock">
      <button class="dock-btn" @click="doReply">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="9 17 4 12 9 7" />
          <path d="M20 18v-2a4 4 0 0 0-4-4H4" />
        </svg>
        <span>Reply</span>
      </button>
      <button class="dock-btn" @click="doReplyAll">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="7 17 2 12 7 7" />
          <polyline points="12 17 7 12 12 7" />
          <path d="M22 18v-2a4 4 0 0 0-4-4H7" />
        </svg>
        <span>Reply all</span>
      </button>
      <button class="dock-btn" @click="doForward">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="15 17 20 12 15 7" />
          <path d="M4 18v-2a4 4 0 0 1 4-4h12" />
        </svg>
        <span>Forward</span>
      </button>
    </div>
  </div>
</template>

<style scoped>
.mobile-thread-view {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
  background: var(--color-bg);
}

.thread-bar {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 4px 6px;
  padding-top: max(10px, env(safe-area-inset-top));
  border-bottom: 1px solid var(--color-divider, #e9e0cd);
  background: var(--color-bg);
  color: var(--color-accent);
}

.bar-spacer {
  flex: 1;
}

/* Subject block */
.subject-block {
  flex-shrink: 0;
  padding: 12px 16px 10px;
  border-bottom: 1px solid var(--color-divider, #e9e0cd);
  background: var(--color-bg);
}

.subject-line {
  display: flex;
  align-items: flex-start;
  gap: 10px;
}

.subject {
  flex: 1;
  margin: 0;
  font-size: 20px;
  line-height: 1.25;
  font-weight: 700;
  letter-spacing: -0.3px;
  color: var(--color-text);
  word-break: break-word;
}

.subject-star {
  flex-shrink: 0;
  border: 0;
  background: transparent;
  color: var(--color-text-muted);
  font-size: 22px;
  line-height: 1;
  padding: 2px 4px;
  cursor: pointer;
}

.subject-star.on {
  color: var(--color-star-flag);
}

.subject-meta {
  margin-top: 6px;
  display: flex;
  align-items: center;
  gap: 8px;
  color: var(--color-text-muted);
  font-size: 12px;
}

.acct-chip {
  display: inline-flex;
  align-items: center;
  height: 18px;
  padding: 0 8px;
  border-radius: 999px;
  font-size: 10px;
  font-weight: 700;
  letter-spacing: 0.5px;
}

.meta-text {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.thread-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  font-size: 15px;
  line-height: 1.55;
}

/* Reply dock */
.reply-dock {
  flex-shrink: 0;
  display: flex;
  gap: 8px;
  padding: 10px 12px;
  padding-bottom: max(10px, env(safe-area-inset-bottom));
  background: var(--color-bg);
  border-top: 1px solid var(--color-divider, #e9e0cd);
}

.dock-btn {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  height: 40px;
  border: 0;
  border-radius: 10px;
  background: var(--color-accent-light);
  color: var(--color-accent);
  font-family: inherit;
  font-size: 14px;
  font-weight: 600;
  cursor: pointer;
}

.dock-btn svg {
  width: 16px;
  height: 16px;
  stroke-width: 1.8;
}

.dock-btn:active {
  filter: brightness(0.95);
}
</style>
