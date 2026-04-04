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

watch(
  () => messagesStore.activeMessageId,
  async () => {
    showHtml.value = false;
    invites.value = [];
    // Check for calendar invites in the message
    const accountId = accountsStore.activeAccountId;
    const msgId = messagesStore.activeMessageId;
    if (accountId && msgId) {
      try {
        invites.value = await api.getEmailInvites(accountId, msgId);
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
        <button class="action-btn" title="Reply" @click="reply">
          <span class="action-icon">&#x21A9;</span> Reply
        </button>
        <button class="action-btn" title="Reply All" @click="replyAll">
          <span class="action-icon">&#x21A9;</span> All
        </button>
        <button class="action-btn" title="Forward" @click="forward">
          <span class="action-icon">&#x21AA;</span> Forward
        </button>
        <div class="action-separator"></div>
        <button class="icon-action" title="Archive" @click="archiveMessage">&#x1F4E6;</button>
        <button class="icon-action" title="Report spam" @click="markSpam">&#x26A0;</button>
        <button class="icon-action danger" title="Delete" @click="deleteMessage">&#x1F5D1;</button>
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
  background: var(--color-bg);
}

.action-btn {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 5px 10px;
  border-radius: 6px;
  font-size: 12px;
  font-weight: 500;
  color: var(--color-text-secondary);
  transition: all 0.12s;
}

.action-btn:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.action-icon {
  font-size: 13px;
}

.icon-action {
  width: 30px;
  height: 30px;
  border-radius: 50%;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 14px;
  color: var(--color-text-secondary);
  transition: background 0.12s;
}

.icon-action:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.icon-action.danger:hover {
  background: rgba(243, 139, 168, 0.1);
  color: var(--color-danger);
}

.action-separator {
  width: 1px;
  height: 20px;
  background: var(--color-border);
  margin: 0 4px;
}

.action-spacer {
  flex: 1;
}

.view-toggle {
  display: flex;
  border: 1px solid var(--color-border);
  border-radius: 20px;
  overflow: hidden;
  flex-shrink: 0;
}

.toggle-btn {
  padding: 4px 12px;
  font-size: 11px;
  font-weight: 500;
  color: var(--color-text-muted);
  border-right: 1px solid var(--color-border);
  transition: all 0.15s;
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
  font-weight: 500;
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
