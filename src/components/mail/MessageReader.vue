<script setup lang="ts">
import { ref, watch } from "vue";
import { useRouter } from "vue-router";
import { useMessagesStore } from "@/stores/messages";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import type { ParsedInvite } from "@/lib/types";
import InviteCard from "@/components/calendar/InviteCard.vue";
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
const invites = ref<ParsedInvite[]>([]);

// Reset view state when switching messages
watch(
  () => messagesStore.activeMessageId,
  () => {
    showHtml.value = false;
    invites.value = [];
  },
);

// Check for calendar invites AFTER body is loaded (body must be on disk for parsing)
watch(
  () => messagesStore.activeMessage,
  async (msg) => {
    invites.value = [];
    if (!msg) return;
    const accountId = accountsStore.activeAccountId;
    const msgId = messagesStore.activeMessageId;
    if (accountId && msgId) {
      try {
        const all = await api.getEmailInvites(accountId, msgId);
        // Only show invite card for METHOD:REQUEST (new invites), not REPLY/CANCEL
        invites.value = all.filter((inv) => inv.method.toUpperCase() === "REQUEST");
      } catch {
        // No invites or parse error — silently ignore
      }
    }
  },
);

const hasHtml = () => !!messagesStore.activeMessage?.body_html;
const hasText = () => !!messagesStore.activeMessage?.body_text;

// Defense-in-depth: strip any JS vectors that might survive backend sanitization.
// The Rust ammonia sanitizer is the primary defense; this is a second layer.
function sanitizeHtml(html: string): string {
  const div = document.createElement("div");
  div.innerHTML = html;
  // Remove script/style/iframe elements
  for (const tag of ["script", "style", "iframe", "object", "embed"]) {
    for (const el of Array.from(div.getElementsByTagName(tag))) {
      el.remove();
    }
  }
  // Remove event handler attributes (on*) and javascript: hrefs
  for (const el of Array.from(div.querySelectorAll("*"))) {
    for (const attr of Array.from(el.attributes)) {
      if (attr.name.startsWith("on") || (attr.name === "href" && attr.value.trim().toLowerCase().startsWith("javascript:"))) {
        el.removeAttribute(attr.name);
      }
    }
  }
  return div.innerHTML;
}

function safeHtml(): string {
  return sanitizeHtml(messagesStore.activeMessage?.body_html ?? "");
}

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
      replyTo: msg.id,
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
      replyTo: msg.id,
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
        <div class="actions-left">
          <button class="pill-btn" title="Reply" @click="reply">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 17 4 12 9 7" /><path d="M20 18v-2a4 4 0 0 0-4-4H4" /></svg>
            Reply
          </button>
          <button class="pill-btn" title="Reply All" @click="replyAll">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 17 4 12 9 7" /><path d="M20 18v-2a4 4 0 0 0-4-4H4" /></svg>
            Reply All
          </button>
          <button class="pill-btn" title="Forward" @click="forward">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="15 17 20 12 15 7" /><path d="M4 18v-2a4 4 0 0 1 4-4h12" /></svg>
            Forward
          </button>
        </div>
        <div class="actions-right">
          <button class="icon-action" title="Archive" @click="archiveMessage">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="21 8 21 21 3 21 3 8" /><rect x="1" y="3" width="22" height="5" /><line x1="10" y1="12" x2="14" y2="12" /></svg>
          </button>
          <button class="icon-action" title="Report spam" @click="markSpam">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10" /><line x1="12" y1="8" x2="12" y2="12" /><line x1="12" y1="16" x2="12.01" y2="16" /></svg>
          </button>
          <button class="icon-action danger" title="Delete" @click="deleteMessage">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /></svg>
          </button>
          <div v-if="hasHtml()" class="view-toggle">
            <button
              class="toggle-btn"
              :class="{ active: !showHtml }"
              title="Plain Text"
              @click="showHtml = false"
            >
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" /><polyline points="14 2 14 8 20 8" /><line x1="16" y1="13" x2="8" y2="13" /><line x1="16" y1="17" x2="8" y2="17" /><polyline points="10 9 9 9 8 9" />
              </svg>
            </button>
            <button
              class="toggle-btn"
              :class="{ active: showHtml }"
              title="HTML"
              @click="showHtml = true"
            >
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                <polyline points="16 18 22 12 16 6" /><polyline points="8 6 2 12 8 18" />
              </svg>
            </button>
          </div>
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

      <!-- Calendar invites -->
      <div v-if="invites.length > 0" class="invite-section">
        <InviteCard
          v-for="invite in invites"
          :key="invite.uid"
          :invite="invite"
          :message-id="messagesStore.activeMessageId!"
        />
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
            v-html="safeHtml()"
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
            v-html="safeHtml()"
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
  background: var(--color-reader-bg);
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
  justify-content: space-between;
  padding: 6px 12px;
  border-bottom: 1px solid var(--color-border);
  background: var(--color-bg);
}

.actions-left {
  display: flex;
  align-items: center;
  gap: 6px;
}

.actions-right {
  display: flex;
  align-items: center;
  gap: 4px;
}

.pill-btn {
  display: flex;
  align-items: center;
  gap: 5px;
  padding: 5px 12px;
  border-radius: 4px;
  border: none;
  font-size: 14px;
  font-weight: 500;
  color: var(--color-text);
  background: var(--color-bg-tertiary);
  transition: all 0.12s;
}

.pill-btn:hover {
  background: var(--color-border);
}

.icon-action {
  width: 30px;
  height: 30px;
  border-radius: 6px;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--color-text-muted);
  transition: all 0.12s;
}

.icon-action:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.icon-action.danger:hover {
  background: rgba(220, 53, 69, 0.08);
  color: var(--color-danger);
}

.view-toggle {
  display: flex;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  overflow: hidden;
  flex-shrink: 0;
}

.toggle-btn {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 32px;
  height: 28px;
  color: var(--color-text-muted);
  border-right: 1px solid var(--color-border);
  transition: all 0.12s;
}

.toggle-btn:last-child {
  border-right: none;
}

.toggle-btn:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.toggle-btn.active {
  background: var(--color-accent);
  color: white;
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

.invite-section {
  padding: 12px 16px 0;
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
