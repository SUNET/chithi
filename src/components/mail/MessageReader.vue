<script setup lang="ts">
import { ref, watch } from "vue";
import { useRouter } from "vue-router";
import { useMessagesStore } from "@/stores/messages";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import * as api from "@/lib/tauri";

defineProps<{
  standalone?: boolean;
}>();

const emit = defineEmits<{
  close: [];
}>();

const router = useRouter();
const messagesStore = useMessagesStore();
const accountsStore = useAccountsStore();
const foldersStore = useFoldersStore();

// View mode: plain text by default
const showHtml = ref(false);

watch(
  () => messagesStore.activeMessageId,
  () => {
    showHtml.value = false;
  },
);

const hasHtml = () => !!messagesStore.activeMessage?.body_html;
const hasText = () => !!messagesStore.activeMessage?.body_text;

// Toast
const toast = ref<string | null>(null);
let toastTimer: ReturnType<typeof setTimeout> | null = null;

function showToast(msg: string) {
  toast.value = msg;
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toast.value = null;
  }, 2000);
}

function handleLinkClick(event: MouseEvent) {
  const target = (event.target as HTMLElement).closest("a");
  if (!target) return;
  event.preventDefault();
  event.stopPropagation();
  const href = target.getAttribute("href");
  if (!href) return;
  navigator.clipboard.writeText(href).then(() => {
    showToast("Link copied to clipboard");
  });
}

function handleContextMenu(event: MouseEvent) {
  event.preventDefault();
}

// --- Message actions ---

function quoteBody(): string {
  const msg = messagesStore.activeMessage;
  if (!msg) return "";
  const text = msg.body_text || "";
  const date = new Date(msg.date).toLocaleString();
  const from = msg.from.name
    ? `${msg.from.name} <${msg.from.email}>`
    : msg.from.email;
  const header = `On ${date}, ${from} wrote:`;
  const quoted = text
    .split("\n")
    .map((line) => `> ${line}`)
    .join("\n");
  return `\n\n${header}\n${quoted}`;
}

function reply() {
  const msg = messagesStore.activeMessage;
  if (!msg) return;
  router.push({
    path: "/compose",
    query: {
      to: msg.from.email,
      subject: msg.subject?.startsWith("Re:") ? msg.subject : `Re: ${msg.subject || ""}`,
      body: quoteBody(),
    },
  });
}

function replyAll() {
  const msg = messagesStore.activeMessage;
  if (!msg) return;
  const myEmail = accountsStore.activeAccount()?.email ?? "";
  const allTo = [
    msg.from.email,
    ...msg.to.map((a) => a.email).filter((e) => e !== myEmail),
  ];
  const allCc = msg.cc.map((a) => a.email).filter((e) => e !== myEmail);
  router.push({
    path: "/compose",
    query: {
      to: allTo.join(", "),
      cc: allCc.join(", "),
      subject: msg.subject?.startsWith("Re:") ? msg.subject : `Re: ${msg.subject || ""}`,
      body: quoteBody(),
    },
  });
}

function forward() {
  const msg = messagesStore.activeMessage;
  if (!msg) return;
  const text = msg.body_text || "";
  const date = new Date(msg.date).toLocaleString();
  const from = msg.from.name
    ? `${msg.from.name} <${msg.from.email}>`
    : msg.from.email;
  const toStr = msg.to.map((a) => a.name || a.email).join(", ");
  const fwdHeader = `---------- Forwarded message ----------\nFrom: ${from}\nDate: ${date}\nSubject: ${msg.subject || ""}\nTo: ${toStr}\n\n`;
  router.push({
    path: "/compose",
    query: {
      subject: msg.subject?.startsWith("Fwd:") ? msg.subject : `Fwd: ${msg.subject || ""}`,
      body: `\n\n${fwdHeader}${text}`,
    },
  });
}

async function deleteMessage() {
  const accountId = accountsStore.activeAccountId;
  const msgId = messagesStore.activeMessageId;
  if (!accountId || !msgId) return;
  try {
    await api.deleteMessages(accountId, [msgId]);
    messagesStore.activeMessage = null;
    messagesStore.activeMessageId = null;
    await messagesStore.fetchMessages();
    await foldersStore.fetchFolders();
  } catch (e) {
    console.error("Delete failed:", e);
  }
}

async function archiveMessage() {
  const accountId = accountsStore.activeAccountId;
  const msgId = messagesStore.activeMessageId;
  if (!accountId || !msgId) return;
  const folder = foldersStore.folders.find((f) => f.folder_type === "archive");
  if (!folder) {
    showToast("No archive folder found");
    return;
  }
  try {
    await api.moveMessages(accountId, [msgId], folder.path);
    messagesStore.activeMessage = null;
    messagesStore.activeMessageId = null;
    await messagesStore.fetchMessages();
    await foldersStore.fetchFolders();
  } catch (e) {
    console.error("Archive failed:", e);
  }
}

async function markSpam() {
  const accountId = accountsStore.activeAccountId;
  const msgId = messagesStore.activeMessageId;
  if (!accountId || !msgId) return;
  const folder = foldersStore.folders.find((f) => f.folder_type === "junk");
  if (!folder) {
    showToast("No spam folder found");
    return;
  }
  try {
    await api.moveMessages(accountId, [msgId], folder.path);
    messagesStore.activeMessage = null;
    messagesStore.activeMessageId = null;
    await messagesStore.fetchMessages();
    await foldersStore.fetchFolders();
  } catch (e) {
    console.error("Spam move failed:", e);
  }
}
</script>

<template>
  <div class="message-reader">
    <div v-if="standalone" class="reader-toolbar">
      <button class="close-btn" title="Close" @click="emit('close')">&times;</button>
    </div>
    <div v-if="messagesStore.loadingBody" class="loading">Loading message...</div>
    <div v-else-if="!messagesStore.activeMessage" class="empty">
      Select a message to read
    </div>
    <div v-else class="message-content">
      <!-- Action bar -->
      <div class="message-actions">
        <button class="action-btn" title="Reply" @click="reply">Reply</button>
        <button class="action-btn" title="Reply All" @click="replyAll">Reply All</button>
        <button class="action-btn" title="Forward" @click="forward">Forward</button>
        <div class="action-separator"></div>
        <button class="action-btn" title="Archive" @click="archiveMessage">Archive</button>
        <button class="action-btn" title="Spam" @click="markSpam">Spam</button>
        <button class="action-btn action-danger" title="Delete" @click="deleteMessage">Delete</button>
        <div class="action-spacer"></div>
        <div v-if="hasHtml()" class="view-toggle">
          <button
            class="toggle-btn"
            :class="{ active: !showHtml }"
            @click="showHtml = false"
          >Plain Text</button>
          <button
            class="toggle-btn"
            :class="{ active: showHtml }"
            @click="showHtml = true"
          >HTML</button>
        </div>
      </div>

      <div class="message-headers">
        <h2 class="message-subject">{{ messagesStore.activeMessage.subject || "(no subject)" }}</h2>
        <div class="header-row">
          <span class="header-label">From:</span>
          <span class="header-value">
            {{ messagesStore.activeMessage.from.name }}
            &lt;{{ messagesStore.activeMessage.from.email }}&gt;
          </span>
        </div>
        <div class="header-row">
          <span class="header-label">To:</span>
          <span class="header-value">
            <span v-for="(addr, i) in messagesStore.activeMessage.to" :key="i">
              {{ addr.name || addr.email }}{{ i < messagesStore.activeMessage.to.length - 1 ? ", " : "" }}
            </span>
          </span>
        </div>
        <div v-if="messagesStore.activeMessage.cc.length" class="header-row">
          <span class="header-label">Cc:</span>
          <span class="header-value">
            <span v-for="(addr, i) in messagesStore.activeMessage.cc" :key="i">
              {{ addr.name || addr.email }}{{ i < messagesStore.activeMessage.cc.length - 1 ? ", " : "" }}
            </span>
          </span>
        </div>
        <div class="header-row">
          <span class="header-label">Date:</span>
          <span class="header-value">{{ new Date(messagesStore.activeMessage.date).toLocaleString() }}</span>
        </div>
        <div v-if="messagesStore.activeMessage.list_id" class="header-row">
          <span class="header-label">List:</span>
          <span class="header-value list-id">{{ messagesStore.activeMessage.list_id }}</span>
        </div>
      </div>
      <div class="message-body">
        <div
          v-if="showHtml && hasHtml()"
          class="body-html-wrapper"
          @click="handleLinkClick"
          @contextmenu="handleContextMenu"
        >
          <div class="no-remote-notice">Remote content blocked</div>
          <div
            class="body-html"
            v-html="messagesStore.activeMessage.body_html"
          />
        </div>
        <pre
          v-else-if="hasText()"
          class="body-text"
          @contextmenu="handleContextMenu"
        >{{ messagesStore.activeMessage.body_text }}</pre>
        <div
          v-else-if="hasHtml()"
          class="body-html-wrapper"
          @click="handleLinkClick"
          @contextmenu="handleContextMenu"
        >
          <div class="no-remote-notice">Remote content blocked</div>
          <div
            class="body-html"
            v-html="messagesStore.activeMessage.body_html"
          />
        </div>
        <div v-else class="empty">No message content</div>
      </div>
    </div>

    <div v-if="toast" class="toast">{{ toast }}</div>
  </div>
</template>

<style scoped>
.message-reader {
  height: 100%;
  overflow-y: auto;
  background: var(--color-bg);
  position: relative;
}

.reader-toolbar {
  display: flex;
  justify-content: flex-end;
  padding: 4px 8px;
  border-bottom: 1px solid var(--color-border);
  background: var(--color-bg-secondary);
}

.close-btn {
  width: 24px;
  height: 24px;
  border-radius: 4px;
  font-size: 18px;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--color-text-muted);
}

.close-btn:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.loading,
.empty {
  padding: 24px;
  text-align: center;
  color: var(--color-text-muted);
}

.message-content {
  padding: 0;
}

.message-actions {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 6px 12px;
  border-bottom: 1px solid var(--color-border);
  background: var(--color-bg-secondary);
}

.action-btn {
  padding: 4px 10px;
  border-radius: 4px;
  font-size: 12px;
  color: var(--color-text-secondary);
}

.action-btn:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.action-danger {
  color: var(--color-danger);
}

.action-danger:hover {
  background: rgba(243, 139, 168, 0.1);
  color: var(--color-danger);
}

.action-separator {
  width: 1px;
  height: 18px;
  background: var(--color-border);
  margin: 0 4px;
}

.action-spacer {
  flex: 1;
}

.view-toggle {
  display: flex;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  overflow: hidden;
  flex-shrink: 0;
}

.toggle-btn {
  padding: 3px 10px;
  font-size: 11px;
  color: var(--color-text-muted);
  border-right: 1px solid var(--color-border);
}

.toggle-btn:last-child {
  border-right: none;
}

.toggle-btn:hover {
  background: var(--color-bg-hover);
}

.toggle-btn.active {
  background: var(--color-bg-active);
  color: var(--color-text);
  font-weight: 600;
}

.message-headers {
  padding: 12px 16px;
  border-bottom: 1px solid var(--color-border);
}

.message-subject {
  font-size: 18px;
  font-weight: 600;
  margin-bottom: 12px;
  line-height: 1.3;
}

.header-row {
  display: flex;
  gap: 8px;
  margin-bottom: 4px;
  font-size: 13px;
}

.header-label {
  color: var(--color-text-muted);
  flex-shrink: 0;
  min-width: 40px;
}

.header-value {
  color: var(--color-text-secondary);
}

.list-id {
  font-family: var(--font-mono);
  font-size: 12px;
  color: var(--color-text-muted);
}

.message-body {
  padding: 16px;
  line-height: 1.5;
}

.no-remote-notice {
  font-size: 11px;
  color: var(--color-text-muted);
  background: #f0f0f0;
  padding: 4px 8px;
  border-radius: 3px;
  margin-bottom: 8px;
}

.body-html-wrapper {
  background: var(--color-email-body-bg);
  color: var(--color-email-body-text);
  border-radius: 6px;
  padding: 16px;
  border: 1px solid var(--color-border);
}

.body-html {
  word-wrap: break-word;
  overflow-wrap: break-word;
}

.body-html :deep(a) {
  color: #1a73e8;
  cursor: pointer;
}

.body-text {
  white-space: pre-wrap;
  font-family: var(--font-mono);
  font-size: 13px;
}

.toast {
  position: absolute;
  bottom: 16px;
  left: 50%;
  transform: translateX(-50%);
  background: var(--color-bg-active);
  color: var(--color-text);
  padding: 6px 16px;
  border-radius: 6px;
  font-size: 12px;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3);
  pointer-events: none;
}
</style>
