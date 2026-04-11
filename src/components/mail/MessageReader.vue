<script setup lang="ts">
import { ref, watch, onMounted, onUnmounted } from "vue";
import { useMessagesStore } from "@/stores/messages";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import type { ParsedInvite, Contact, ContactBook } from "@/lib/types";
import InviteCard from "@/components/calendar/InviteCard.vue";
import { openComposeWindow } from "@/lib/compose-window";
import * as api from "@/lib/tauri";

defineProps<{
  standalone?: boolean;
}>();

const emit = defineEmits<{
  close: [];
}>();

const messagesStore = useMessagesStore();
const accountsStore = useAccountsStore();
const foldersStore = useFoldersStore();

// View mode: plain text by default
const showHtml = ref(false);
const invites = ref<ParsedInvite[]>([]);

// Remote images: per-message, not persisted
const imagesHtml = ref<string | null>(null);
const loadingImages = ref(false);

// Reset view state when switching messages
watch(
  () => messagesStore.activeMessageId,
  () => {
    showHtml.value = false;
    invites.value = [];
    imagesHtml.value = null;
    loadingImages.value = false;
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

        // Auto-process METHOD:REPLY emails (attendee responses) to update participant status
        const replies = all.filter((inv) => inv.method.toUpperCase() === "REPLY");
        if (replies.length > 0) {
          api.processInviteReply(accountId, msgId).catch((e) =>
            console.error("Failed to process invite reply:", e));
        }
      } catch {
        // No invites or parse error — silently ignore
      }
    }
  },
);

const hasHtml = () => !!messagesStore.activeMessage?.body_html;
const hasText = () => !!messagesStore.activeMessage?.body_text;

// Build a sandboxed iframe srcdoc that isolates HTML email from the main webview.
// The iframe has no access to window.__TAURI__ or any IPC commands.
// Links inside the iframe send a postMessage to the parent for clipboard copy.
function iframeSrcdoc(): string {
  const html = imagesHtml.value ?? messagesStore.activeMessage?.body_html ?? "";
  // Strict CSP inside the iframe: no scripts, no external resources except inline styles
  return `<!DOCTYPE html>
<html>
<head>
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src https: data:;">
<style>
  body {
    margin: 0;
    padding: 0;
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
    font-size: 14px;
    line-height: 1.5;
    word-wrap: break-word;
    overflow-wrap: break-word;
    color: inherit;
    background: transparent;
  }
  a { color: #1a73e8; cursor: pointer; }
</style>
</head>
<body>${html}<script>
  // Intercept all link clicks and forward to parent via postMessage
  document.addEventListener('click', function(e) {
    var a = e.target.closest ? e.target.closest('a') : null;
    if (a && a.href) {
      e.preventDefault();
      e.stopPropagation();
      parent.postMessage({ type: 'link-click', href: a.getAttribute('href') }, '*');
    }
  });
  // Intercept right-click and forward to parent
  document.addEventListener('contextmenu', function(e) {
    e.preventDefault();
  });
  // Report content height to parent so iframe can auto-size
  var ro = new ResizeObserver(function() {
    parent.postMessage({ type: 'resize', height: document.documentElement.scrollHeight }, '*');
  });
  ro.observe(document.documentElement);
<\/script></body>
</html>`;
}

// Listen for postMessage from the sandboxed iframe.
// Verify event.source matches our iframe's contentWindow to prevent spoofing.
function handleIframeMessage(event: MessageEvent) {
  if (!event.data || typeof event.data !== 'object') return;
  // Only trust messages from our email sandbox iframe(s)
  const iframes = document.querySelectorAll<HTMLIFrameElement>('.email-sandbox');
  let fromOurIframe = false;
  for (const iframe of iframes) {
    if (event.source === iframe.contentWindow) {
      fromOurIframe = true;
      break;
    }
  }
  if (!fromOurIframe) return;

  if (event.data.type === 'link-click' && typeof event.data.href === 'string') {
    navigator.clipboard.writeText(event.data.href).then(() => {
      showToast("Link copied to clipboard");
    });
  } else if (event.data.type === 'resize' && typeof event.data.height === 'number') {
    // Auto-resize the specific iframe that sent the message
    for (const iframe of iframes) {
      if (event.source === iframe.contentWindow) {
        iframe.style.height = event.data.height + 'px';
      }
    }
  }
}

// Set up / tear down message listener
onMounted(() => window.addEventListener('message', handleIframeMessage));
onUnmounted(() => window.removeEventListener('message', handleIframeMessage));

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

function handleContextMenu(event: MouseEvent) {
  event.preventDefault();
}

// --- Attachment save ---
const savingAttachment = ref<number | null>(null);

async function saveAttachment(index: number, filename: string | null) {
  const accountId = accountsStore.activeAccountId;
  const messageId = messagesStore.activeMessageId;
  if (!accountId || !messageId) return;

  savingAttachment.value = index;
  try {
    // The save dialog is opened by the backend — the renderer only sends
    // a suggested filename, never a path.
    await api.saveAttachment(accountId, messageId, index, filename || "attachment");
    showToast("Attachment saved");
  } catch (e) {
    const msg = String(e);
    if (!msg.includes("cancelled")) showToast("Failed to save: " + msg);
  } finally {
    savingAttachment.value = null;
  }
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return bytes + " B";
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + " KB";
  return (bytes / (1024 * 1024)).toFixed(1) + " MB";
}

// --- Remote image loading ---
async function loadRemoteImages() {
  const accountId = accountsStore.activeAccountId;
  const messageId = messagesStore.activeMessageId;
  if (!accountId || !messageId) return;

  loadingImages.value = true;
  try {
    imagesHtml.value = await api.getMessageHtmlWithImages(accountId, messageId);
  } catch (e) {
    showToast("Failed to load images: " + String(e));
  } finally {
    loadingImages.value = false;
  }
}

// --- Address right-click → Add/Edit Contact ---

const addrMenu = ref<{ x: number; y: number; email: string; name: string } | null>(null);
const addrMenuContact = ref<Contact | null>(null);
const showContactForm = ref(false);
const contactFormSaving = ref(false);
const contactFormError = ref<string | null>(null);
const contactBooks = ref<ContactBook[]>([]);

// Contact form fields
const cfFirstName = ref("");
const cfMiddleName = ref("");
const cfLastName = ref("");
const cfEmails = ref<{ email: string; label: string }[]>([]);
const cfPhones = ref<{ number: string; label: string }[]>([]);
const cfOrg = ref("");
const cfTitle = ref("");
const cfNotes = ref("");
const cfBookId = ref("");
const cfEditingId = ref<string | null>(null);

function closeAddrMenu() {
  addrMenu.value = null;
}

async function onAddrRightClick(event: MouseEvent, email: string, name: string | null) {
  event.preventDefault();
  event.stopPropagation();
  addrMenu.value = { x: event.clientX, y: event.clientY, email, name: name || "" };
  // Search contacts scoped to the active account's books
  try {
    const accountId = accountsStore.activeAccountId;
    let activeBookIds: Set<string> = new Set();
    if (accountId) {
      const books = await api.listContactBooks(accountId);
      activeBookIds = new Set(books.map((b) => b.id));
    }
    const results = await api.searchContacts(email);
    const exact = results.find((c) => {
      if (!activeBookIds.has(c.book_id)) return false;
      try {
        const emails: { email: string }[] = JSON.parse(c.emails_json);
        return emails.some((e) => e.email.toLowerCase() === email.toLowerCase());
      } catch { return false; }
    });
    addrMenuContact.value = exact || null;
  } catch {
    addrMenuContact.value = null;
  }
}

async function openContactForm() {
  const clickedEmail = addrMenu.value?.email || "";
  const clickedName = addrMenu.value?.name || "";
  closeAddrMenu();
  // Fetch contact books from all accounts
  const allBooks: ContactBook[] = [];
  for (const acc of accountsStore.accounts) {
    try {
      const books = await api.listContactBooks(acc.id);
      allBooks.push(...books);
    } catch { /* skip */ }
  }
  contactBooks.value = allBooks;
  // Default to the active account's first book
  const activeAccountBooks = allBooks.filter(
    (b) => b.account_id === accountsStore.activeAccountId,
  );
  const defaultBookId = activeAccountBooks[0]?.id ?? allBooks[0]?.id ?? "";

  if (addrMenuContact.value) {
    // Edit existing contact
    const c = addrMenuContact.value;
    cfEditingId.value = c.id;
    const parts = c.display_name.trim().split(/\s+/);
    cfFirstName.value = parts[0] || "";
    cfMiddleName.value = parts.length > 2 ? parts.slice(1, -1).join(" ") : "";
    cfLastName.value = parts.length > 1 ? parts[parts.length - 1] : "";
    try { cfEmails.value = JSON.parse(c.emails_json); } catch { cfEmails.value = []; }
    if (cfEmails.value.length === 0) cfEmails.value = [{ email: "", label: "work" }];
    try { cfPhones.value = JSON.parse(c.phones_json); } catch { cfPhones.value = []; }
    cfOrg.value = c.organization ?? "";
    cfTitle.value = c.title ?? "";
    cfNotes.value = c.notes ?? "";
    cfBookId.value = c.book_id;
  } else {
    // New contact — prefill from the address
    cfEditingId.value = null;
    const nameParts = clickedName.trim().split(/\s+/).filter(Boolean);
    cfFirstName.value = nameParts[0] || "";
    cfMiddleName.value = nameParts.length > 2 ? nameParts.slice(1, -1).join(" ") : "";
    cfLastName.value = nameParts.length > 1 ? nameParts[nameParts.length - 1] : "";
    cfEmails.value = [{ email: clickedEmail, label: "work" }];
    cfPhones.value = [];
    cfOrg.value = "";
    cfTitle.value = "";
    cfNotes.value = "";
    cfBookId.value = defaultBookId;
  }
  contactFormError.value = null;
  showContactForm.value = true;
}

async function saveContactForm() {
  if (!cfFirstName.value.trim()) { contactFormError.value = "First name is required"; return; }
  if (!cfLastName.value.trim()) { contactFormError.value = "Last name is required"; return; }
  contactFormSaving.value = true;
  try {
    const displayName = [cfFirstName.value.trim(), cfMiddleName.value.trim(), cfLastName.value.trim()]
      .filter(Boolean).join(" ");
    const emailsFiltered = cfEmails.value.filter((e) => e.email.trim());
    const phonesFiltered = cfPhones.value.filter((p) => p.number.trim());

    if (cfEditingId.value) {
      const existing = addrMenuContact.value!;
      await api.updateContact({
        ...existing,
        display_name: displayName,
        emails_json: JSON.stringify(emailsFiltered),
        phones_json: JSON.stringify(phonesFiltered),
        organization: cfOrg.value || null,
        title: cfTitle.value || null,
        notes: cfNotes.value || null,
        book_id: cfBookId.value,
      });
      showToast("Contact updated");
    } else {
      await api.createContact({
        book_id: cfBookId.value,
        display_name: displayName,
        emails_json: JSON.stringify(emailsFiltered),
        phones_json: JSON.stringify(phonesFiltered),
        addresses_json: "[]",
        organization: cfOrg.value || null,
        title: cfTitle.value || null,
        notes: cfNotes.value || null,
      });
      showToast("Contact added");
    }
    showContactForm.value = false;
  } catch (e) {
    contactFormError.value = String(e);
  } finally {
    contactFormSaving.value = false;
  }
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
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    replyTo: msg.id,
    to: msg.from.email,
    subject: msg.subject?.startsWith("Re:") ? msg.subject : `Re: ${msg.subject || ""}`,
    body: quoteBody(),
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
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    replyTo: msg.id,
    to: allTo.join(", "),
    cc: allCc.join(", "),
    subject: msg.subject?.startsWith("Re:") ? msg.subject : `Re: ${msg.subject || ""}`,
    body: quoteBody(),
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
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    subject: msg.subject?.startsWith("Fwd:") ? msg.subject : `Fwd: ${msg.subject || ""}`,
    body: `\n\n${fwdHeader}${text}`,
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
              data-testid="reader-html-toggle"
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
        <h2 class="message-subject" data-testid="reader-subject">{{ messagesStore.activeMessage.subject || "(no subject)" }}</h2>
        <div class="header-row" data-testid="reader-from">
          <span class="header-label">From:</span>
          <span class="header-value">
            <span class="addr-clickable" @contextmenu="onAddrRightClick($event, messagesStore.activeMessage.from.email, messagesStore.activeMessage.from.name)">
              {{ messagesStore.activeMessage.from.name }}
              &lt;{{ messagesStore.activeMessage.from.email }}&gt;
            </span>
          </span>
        </div>
        <div class="header-row" data-testid="reader-to">
          <span class="header-label">To:</span>
          <span class="header-value">
            <span v-for="(addr, i) in messagesStore.activeMessage.to" :key="i" class="addr-clickable" @contextmenu="onAddrRightClick($event, addr.email, addr.name)">
              {{ addr.name || addr.email }}{{ i < messagesStore.activeMessage.to.length - 1 ? ", " : "" }}
            </span>
          </span>
        </div>
        <div v-if="messagesStore.activeMessage.cc.length" class="header-row">
          <span class="header-label">Cc:</span>
          <span class="header-value">
            <span v-for="(addr, i) in messagesStore.activeMessage.cc" :key="i" class="addr-clickable" @contextmenu="onAddrRightClick($event, addr.email, addr.name)">
              {{ addr.name || addr.email }}{{ i < messagesStore.activeMessage.cc.length - 1 ? ", " : "" }}
            </span>
          </span>
        </div>
        <div class="header-row" data-testid="reader-date">
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

      <!-- Attachments -->
      <div v-if="messagesStore.activeMessage.attachments.length > 0" class="attachments-section">
        <div class="attachments-header">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48"/></svg>
          {{ messagesStore.activeMessage.attachments.length }} attachment{{ messagesStore.activeMessage.attachments.length > 1 ? 's' : '' }}
        </div>
        <div class="attachments-list">
          <button
            v-for="att in messagesStore.activeMessage.attachments"
            :key="att.index"
            class="attachment-chip"
            :data-testid="`attachment-${att.index}`"
            :disabled="savingAttachment === att.index"
            @click="saveAttachment(att.index, att.filename)"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>
            <span class="att-name">{{ att.filename || 'attachment' }}</span>
            <span class="att-size">{{ formatSize(att.size) }}</span>
          </button>
        </div>
      </div>

      <div class="message-body">
        <div
          v-if="showHtml && hasHtml()"
          class="body-html-wrapper"
        >
          <div v-if="!imagesHtml" class="no-remote-notice">
            Remote content blocked
            <button class="load-images-btn" data-testid="reader-load-images" :disabled="loadingImages" @click="loadRemoteImages">
              {{ loadingImages ? 'Loading...' : 'Load images' }}
            </button>
          </div>
          <iframe
            class="email-sandbox"
            data-testid="reader-body-iframe"
            :srcdoc="iframeSrcdoc()"
            sandbox="allow-scripts"
            referrerpolicy="no-referrer"
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
        >
          <div v-if="!imagesHtml" class="no-remote-notice">
            Remote content blocked
            <button class="load-images-btn" data-testid="reader-load-images" :disabled="loadingImages" @click="loadRemoteImages">
              {{ loadingImages ? 'Loading...' : 'Load images' }}
            </button>
          </div>
          <iframe
            class="email-sandbox"
            data-testid="reader-body-iframe"
            :srcdoc="iframeSrcdoc()"
            sandbox="allow-scripts"
            referrerpolicy="no-referrer"
          />
        </div>
        <div v-else class="empty">No message content</div>
      </div>
    </div>

    <div v-if="toast" class="toast">{{ toast }}</div>

    <!-- Address right-click context menu -->
    <Teleport to="body">
      <div
        v-if="addrMenu"
        class="addr-context-menu"
        :style="{ left: addrMenu.x + 'px', top: addrMenu.y + 'px' }"
        @click.stop
      >
        <button class="ctx-item" @click="openContactForm">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2" /><circle cx="12" cy="7" r="4" />
          </svg>
          {{ addrMenuContact ? 'Edit Contact' : 'Add to Contacts' }}
        </button>
      </div>
      <div v-if="addrMenu" class="addr-menu-overlay" @click="closeAddrMenu"></div>
    </Teleport>

    <!-- Contact form modal -->
    <Teleport to="body">
      <div v-if="showContactForm" class="modal-overlay" @click.self="showContactForm = false">
        <div class="modal contact-form-modal">
          <div class="modal-header">
            <h3>{{ cfEditingId ? 'Edit Contact' : 'Add to Contacts' }}</h3>
            <button class="close-btn" @click="showContactForm = false">&times;</button>
          </div>
          <div class="modal-body">
            <div v-if="contactFormError" class="form-error">{{ contactFormError }}</div>

            <div v-if="contactBooks.length > 0" class="form-group">
              <label>Contact Book</label>
              <select v-model="cfBookId" class="form-select">
                <option v-for="book in contactBooks" :key="book.id" :value="book.id">{{ book.name }}</option>
              </select>
            </div>

            <div class="form-row">
              <div class="form-group">
                <label>First Name *</label>
                <input v-model="cfFirstName" type="text" class="form-input" />
              </div>
              <div class="form-group">
                <label>Middle</label>
                <input v-model="cfMiddleName" type="text" class="form-input" />
              </div>
              <div class="form-group">
                <label>Last Name *</label>
                <input v-model="cfLastName" type="text" class="form-input" />
              </div>
            </div>

            <div class="form-group">
              <label>Emails</label>
              <div v-for="(e, i) in cfEmails" :key="i" class="multi-field-row">
                <input v-model="e.email" type="email" class="form-input" placeholder="email@example.com" />
                <select v-model="e.label" class="form-select form-select-sm">
                  <option value="work">Work</option>
                  <option value="home">Home</option>
                  <option value="other">Other</option>
                </select>
                <button v-if="cfEmails.length > 1" class="remove-btn" @click="cfEmails.splice(i, 1)">&times;</button>
              </div>
              <button class="add-field-btn" @click="cfEmails.push({ email: '', label: 'work' })">+ Add Email</button>
            </div>

            <div class="form-group">
              <label>Phones</label>
              <div v-for="(p, i) in cfPhones" :key="i" class="multi-field-row">
                <input v-model="p.number" type="tel" class="form-input" placeholder="+1 555-0100" />
                <select v-model="p.label" class="form-select form-select-sm">
                  <option value="mobile">Mobile</option>
                  <option value="work">Work</option>
                  <option value="home">Home</option>
                </select>
                <button class="remove-btn" @click="cfPhones.splice(i, 1)">&times;</button>
              </div>
              <button class="add-field-btn" @click="cfPhones.push({ number: '', label: 'mobile' })">+ Add Phone</button>
            </div>

            <div class="form-row">
              <div class="form-group">
                <label>Organization</label>
                <input v-model="cfOrg" type="text" class="form-input" />
              </div>
              <div class="form-group">
                <label>Job Title</label>
                <input v-model="cfTitle" type="text" class="form-input" />
              </div>
            </div>

            <div class="form-group">
              <label>Notes</label>
              <textarea v-model="cfNotes" rows="2" class="form-input"></textarea>
            </div>
          </div>
          <div class="modal-footer">
            <button class="btn-secondary" @click="showContactForm = false">Cancel</button>
            <button class="btn-primary" :disabled="contactFormSaving" @click="saveContactForm">
              {{ contactFormSaving ? 'Saving...' : (cfEditingId ? 'Save' : 'Add Contact') }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>
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

.attachments-section {
  padding: 8px 16px;
  border-bottom: 1px solid var(--color-border);
}

.attachments-header {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--color-text-muted);
  margin-bottom: 6px;
}

.attachments-list {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.attachment-chip {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 5px 10px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  background: var(--color-bg-secondary);
  color: var(--color-text);
  font-size: 12px;
  cursor: pointer;
  transition: all 0.12s;
}

.attachment-chip:hover {
  background: var(--color-bg-hover);
  border-color: var(--color-accent);
}

.attachment-chip:disabled {
  opacity: 0.5;
  cursor: wait;
}

.att-name {
  max-width: 200px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.att-size {
  color: var(--color-text-muted);
  flex-shrink: 0;
}

.message-body {
  padding: 16px;
  line-height: 1.5;
}

.no-remote-notice {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 11px;
  color: var(--color-text-muted);
  background: var(--color-bg-tertiary);
  padding: 4px 8px;
  border-radius: 3px;
  margin-bottom: 8px;
}

.load-images-btn {
  font-size: 11px;
  padding: 2px 8px;
  border: 1px solid var(--color-border);
  border-radius: 3px;
  background: var(--color-bg-secondary);
  color: var(--color-accent);
  cursor: pointer;
}

.load-images-btn:hover {
  background: var(--color-bg-hover);
}

.load-images-btn:disabled {
  opacity: 0.5;
  cursor: wait;
}

.body-html-wrapper {
  background: var(--color-email-body-bg);
  color: var(--color-email-body-text);
  border-radius: 6px;
  padding: 16px;
  border: 1px solid var(--color-border);
}

.email-sandbox {
  width: 100%;
  min-height: 100px;
  border: none;
  display: block;
  background: transparent;
  color-scheme: auto;
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

.addr-clickable {
  cursor: default;
  border-radius: 3px;
  padding: 0 2px;
}

.addr-clickable:hover {
  background: var(--color-bg-hover);
}

.addr-menu-overlay {
  position: fixed;
  inset: 0;
  z-index: 9998;
}

.addr-context-menu {
  position: fixed;
  z-index: 9999;
  background: var(--color-bg);
  border: 0.8px solid var(--color-border);
  border-radius: 8px;
  padding: 4px 0;
  min-width: 180px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.12);
}

.addr-context-menu .ctx-item {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 7px 14px;
  text-align: left;
  font-size: 13px;
  color: var(--color-text);
  background: none;
  border: none;
  cursor: pointer;
}

.addr-context-menu .ctx-item:hover {
  background: var(--color-bg-hover);
}

.contact-form-modal {
  width: 480px;
  max-height: 80vh;
  overflow-y: auto;
}

.contact-form-modal .modal-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 16px;
  border-bottom: 1px solid var(--color-border);
}

.contact-form-modal .modal-header h3 {
  margin: 0;
  font-size: 15px;
}

.contact-form-modal .close-btn {
  font-size: 20px;
  background: none;
  border: none;
  color: var(--color-text-muted);
  cursor: pointer;
}

.contact-form-modal .modal-body {
  padding: 16px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.contact-form-modal .modal-footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 12px 16px;
  border-top: 1px solid var(--color-border);
}

.contact-form-modal .form-error {
  color: var(--color-danger-text);
  font-size: 12px;
  padding: 6px 8px;
  background: rgba(251, 44, 54, 0.06);
  border-radius: 4px;
}

.contact-form-modal .form-group {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.contact-form-modal .form-group label {
  font-size: 11px;
  font-weight: 600;
  color: var(--color-text-secondary);
}

.contact-form-modal .form-row {
  display: flex;
  gap: 8px;
}

.contact-form-modal .form-row .form-group {
  flex: 1;
  min-width: 0;
}

.contact-form-modal .form-input,
.contact-form-modal .form-select {
  width: 100%;
  box-sizing: border-box;
  padding: 6px 8px;
  font-size: 13px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  color: var(--color-text);
}

.contact-form-modal .form-input:focus,
.contact-form-modal .form-select:focus {
  outline: none;
  border-color: var(--color-accent);
}

.contact-form-modal .form-select-sm {
  width: 80px;
  flex-shrink: 0;
}

.contact-form-modal .multi-field-row {
  display: flex;
  gap: 6px;
  align-items: center;
  margin-bottom: 4px;
}

.contact-form-modal .multi-field-row .form-input {
  flex: 1;
}

.contact-form-modal .remove-btn {
  width: 22px;
  height: 22px;
  display: flex;
  align-items: center;
  justify-content: center;
  border-radius: 50%;
  background: none;
  border: none;
  color: var(--color-text-muted);
  cursor: pointer;
  font-size: 16px;
}

.contact-form-modal .remove-btn:hover {
  color: var(--color-danger-text);
  background: rgba(251, 44, 54, 0.06);
}

.contact-form-modal .add-field-btn {
  background: none;
  border: none;
  color: var(--color-accent);
  font-size: 12px;
  cursor: pointer;
  padding: 2px 0;
}

.contact-form-modal .add-field-btn:hover {
  text-decoration: underline;
}

.contact-form-modal .btn-primary {
  padding: 6px 16px;
  background: var(--color-accent);
  color: white;
  border: none;
  border-radius: 6px;
  font-size: 13px;
  cursor: pointer;
}

.contact-form-modal .btn-primary:hover {
  background: var(--color-accent-hover);
}

.contact-form-modal .btn-secondary {
  padding: 6px 16px;
  background: var(--color-bg-hover);
  color: var(--color-text);
  border: none;
  border-radius: 6px;
  font-size: 13px;
  cursor: pointer;
}

.modal-overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.3);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 10000;
}

.modal {
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 10px;
  box-shadow: 0 8px 32px rgba(0, 0, 0, 0.15);
}
</style>
