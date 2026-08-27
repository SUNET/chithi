<script setup lang="ts">
import { ref, computed, watch, onMounted, onUnmounted } from "vue";
import { useMessagesStore } from "@/stores/messages";
import { useUiStore } from "@/stores/ui";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import type { ParsedInvite, Contact, ContactBook } from "@/lib/types";
import InviteCard from "@/components/calendar/InviteCard.vue";
import ContactFormModal from "@/components/contacts/ContactFormModal.vue";
import { openComposeWindow } from "@/lib/compose-window";
import { parseMailto } from "@/lib/mailto";
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
const uiStore = useUiStore();

// View mode: sticky across messages via uiStore so a user who flips to
// HTML once doesn't have to re-flip on every subsequent message. Falls
// back to plain text if the current message has no HTML part.
const showHtml = computed({
  get: () => uiStore.preferHtmlBody && hasHtml(),
  set: (v: boolean) => uiStore.setPreferHtmlBody(v),
});
const invites = ref<ParsedInvite[]>([]);

// Remote images: per-message, not persisted
const imagesHtml = ref<string | null>(null);
const loadingImages = ref(false);

// Only show "Load images" when the original email contained remote images.
// The backend checks the raw HTML before ammonia strips <img> tags.
const hasRemoteImages = computed(() => {
  return messagesStore.activeMessage?.has_remote_images ?? false;
});

// Reset per-message state when switching messages. The HTML/plain choice
// is preserved in uiStore.preferHtmlBody and intentionally not touched.
watch(
  () => messagesStore.activeMessageId,
  () => {
    invites.value = [];
    imagesHtml.value = null;
    loadingImages.value = false;
    decryptedOverlay.value = null;
    verifyOutcome.value = null;
    decryptError.value = null;
    decryptBusy.value = false;
  },
);

// OpenPGP state. When a message is decrypted we render the decrypted
// MessageBody in place of the original (which is just an encrypted
// blob). For signed messages we auto-verify once on load.
import type { MessageBody as MessageBodyT, PgpVerifyOutcome } from "@/lib/types";
const decryptedOverlay = ref<MessageBodyT | null>(null);
const verifyOutcome = ref<PgpVerifyOutcome | null>(null);
const decryptBusy = ref(false);
const decryptError = ref<string | null>(null);

// The body fields actually rendered are either the live message or the
// decrypted overlay if we successfully decrypted.
const displayedBody = computed<MessageBodyT | null>(
  () => decryptedOverlay.value ?? messagesStore.activeMessage,
);
const pgpKind = computed(() => messagesStore.activeMessage?.pgp_kind);
// Recreate the iframe when its document source changes materially. In
// particular, loading remote images must start with a fresh viewport and a
// fresh height-reporting lifecycle instead of inheriting the old inline
// height from the image-free document.
const iframeRenderKey = computed(() =>
  [
    messagesStore.activeMessageId ?? "",
    decryptedOverlay.value ? "decrypted" : "original",
    imagesHtml.value === null ? "images-blocked" : "images-loaded",
  ].join(":"),
);

async function decryptOpenPGP() {
  const acct = accountsStore.activeAccountId;
  const msgId = messagesStore.activeMessageId;
  if (!acct || !msgId) return;
  decryptError.value = null;
  decryptBusy.value = true;
  try {
    const result = await api.pgpDecryptMessage(acct, msgId);
    decryptedOverlay.value = result.plaintextBody;
    verifyOutcome.value = result.verifyOutcome;
  } catch (e) {
    decryptError.value = e instanceof Error ? e.message : String(e);
  } finally {
    decryptBusy.value = false;
  }
}

// Auto-verify multipart/signed on load. Encrypted messages don't
// auto-decrypt — that needs a passphrase prompt, which is user-driven.
watch(
  () => messagesStore.activeMessage,
  async (msg) => {
    if (!msg || msg.pgp_kind !== "mimeSigned") return;
    const acct = accountsStore.activeAccountId;
    const msgId = messagesStore.activeMessageId;
    if (!acct || !msgId) return;
    try {
      verifyOutcome.value = await api.pgpVerifyMessage(acct, msgId);
    } catch (e) {
      verifyOutcome.value = {
        kind: "error",
        message: e instanceof Error ? e.message : String(e),
      };
    }
  },
);

function verifyBadgeLabel(o: PgpVerifyOutcome | null): string {
  if (!o) return "";
  switch (o.kind) {
    case "good":
      return `Signed by ${o.signerUid ?? o.signerFingerprint.slice(-16)}`;
    case "bad":
      return `Signature invalid (${o.signerUid ?? o.signerFingerprint.slice(-16)})`;
    case "unknownKey":
      return `Unknown signer (key id ${o.keyId})`;
    case "unsigned":
      return "";
    case "error":
      return `Signature check error: ${o.message}`;
  }
}

function verifyBadgeClass(o: PgpVerifyOutcome | null): string {
  if (!o) return "";
  switch (o.kind) {
    case "good":
      return "pgp-badge pgp-badge-good";
    case "bad":
    case "error":
      return "pgp-badge pgp-badge-bad";
    case "unknownKey":
      return "pgp-badge pgp-badge-warn";
    case "unsigned":
      return "";
  }
}

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

        // Auto-process METHOD:CANCEL emails (event cancellations)
        const cancels = all.filter((inv) => inv.method.toUpperCase() === "CANCEL");
        if (cancels.length > 0) {
          api.processCancelledInvite(accountId, msgId).catch((e) =>
            console.error("Failed to process cancelled invite:", e));
        }
      } catch {
        // No invites or parse error — silently ignore
      }
    }
  },
);

const hasHtml = () => !!(displayedBody.value?.body_html);
const hasText = () => !!(displayedBody.value?.body_text);

// The app-level CSP is inherited by srcdoc documents. Both policies allow
// this exact trusted bootstrap by hash while all email-provided scripts stay
// blocked. The iframe regression test detects drift when the script changes.
const IFRAME_BOOTSTRAP_HASH = "sha256-+vePiogHMK6Dv7W4Iq5+OZ1HRJzVYJFdrMV44DV/Bk4=";

// Build a sandboxed iframe srcdoc that isolates HTML email from the main webview.
// Uses a CSP hash instead of 'unsafe-inline' so only our bootstrap script runs.
// Email HTML is embedded in srcdoc but sanitized by ammonia on the backend.
function iframeSrcdoc(): string {
  const html = imagesHtml.value ?? displayedBody.value?.body_html ?? "";
  return `<!DOCTYPE html>
<html>
<head>
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src '${IFRAME_BOOTSTRAP_HASH}'; style-src 'unsafe-inline'; img-src https: data:;">
<style>
  html {
    width: 100%;
    max-width: 100%;
    overflow-x: auto;
  }
  body {
    margin: 0;
    padding: 0;
    width: 100%;
    max-width: 100%;
    box-sizing: border-box;
    overflow-x: auto;
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
    font-size: 14px;
    line-height: 1.5;
    word-wrap: break-word;
    overflow-wrap: break-word;
    color: ${uiStore.theme === "dark" ? "#e5e5e5" : "#1a1a1a"};
    background: ${uiStore.theme === "dark" ? "#171717" : "#ffffff"};
  }
  body * {
    max-width: 100% !important;
    box-sizing: border-box;
  }
  img { height: auto !important; }
  td, th, pre {
    overflow-wrap: anywhere;
    word-break: break-word;
  }
  pre { white-space: pre-wrap; }
  a { color: #1a73e8; cursor: pointer; }
</style>
</head>
<body>${html}<script>
  // WebKit may report a text node as the event target when the visible link
  // text is clicked. Promote non-element targets before looking for an anchor.
  function closestAnchor(target) {
    if (!target) return null;
    var element = target.nodeType === 1 ? target : target.parentElement;
    return element && element.closest ? element.closest('a') : null;
  }
  // Anchor's raw href attribute; null/empty for fragment-only or hrefless
  // anchors. Use getAttribute (not .href, which auto-resolves relatives
  // against the iframe location and would surface "about:srcdoc#foo").
  function anchorHref(a) {
    var h = a ? a.getAttribute('href') : '';
    if (!h) return '';
    h = h.trim();
    if (!h || h.charAt(0) === '#') return '';
    return h;
  }
  // Intercept all link clicks and forward to parent via postMessage
  document.addEventListener('click', function(e) {
    var a = closestAnchor(e.target);
    var href = anchorHref(a);
    if (href) {
      e.preventDefault();
      e.stopPropagation();
      parent.postMessage({ type: 'link-click', href: href }, '*');
    }
  });
  // Forward hover state on links so the parent can preview the URL in the
  // status bar. mouseover/mouseout bubble (mouseenter/leave do not), so a
  // single document-level listener is enough.
  document.addEventListener('mouseover', function(e) {
    var a = closestAnchor(e.target);
    var href = anchorHref(a);
    if (href) {
      parent.postMessage({ type: 'link-hover', href: href }, '*');
    }
  });
  document.addEventListener('mouseout', function(e) {
    var a = closestAnchor(e.target);
    if (!anchorHref(a)) return;
    // mouseout also fires when the pointer moves between an anchor's own
    // child nodes; relatedTarget is where the pointer went next. Only
    // emit link-leave when the cursor actually left this <a>.
    var rel = e.relatedTarget;
    var relAnchor = closestAnchor(rel);
    if (relAnchor === a) return;
    parent.postMessage({ type: 'link-leave' }, '*');
  });
  // Intercept right-click and forward to parent
  document.addEventListener('contextmenu', function(e) {
    e.preventDefault();
  });
  // Report content height to the parent so the iframe can auto-size. The
  // documentElement's own box follows the iframe viewport, so observing only
  // that element misses later body growth (notably when images finish
  // loading). Observe the body and also report at lifecycle boundaries.
  function reportHeight() {
    var body = document.body;
    var root = document.documentElement;
    var height = Math.max(
      body ? body.scrollHeight : 0,
      body ? body.offsetHeight : 0,
      root ? root.scrollHeight : 0,
      root ? root.offsetHeight : 0
    );
    parent.postMessage({ type: 'resize', height: Math.ceil(height) }, '*');
  }
  if (typeof ResizeObserver !== 'undefined') {
    var ro = new ResizeObserver(reportHeight);
    ro.observe(document.body);
  }
  Array.prototype.forEach.call(document.images, function(img) {
    img.addEventListener('load', reportHeight);
    img.addEventListener('error', reportHeight);
  });
  window.addEventListener('load', reportHeight);
  reportHeight();
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
    uiStore.setHoverUrl(null);
    // mailto: goes straight to compose, matching the expected behavior of
    // an email client; users almost never want a confirmation step on
    // their own send action. Everything else routes through the popup so
    // the user can choose copy / open / cancel and preview the cleaned URL.
    const mailto = parseMailto(event.data.href);
    if (mailto) {
      openComposeWindow({
        accountId: accountsStore.activeAccountId ?? undefined,
        ...mailto,
      });
    } else {
      uiStore.openLinkPopup(event.data.href);
    }
  } else if (event.data.type === 'link-hover' && typeof event.data.href === 'string') {
    uiStore.setHoverUrl(event.data.href);
  } else if (event.data.type === 'link-leave') {
    uiStore.setHoverUrl(null);
  } else if (event.data.type === 'resize' && typeof event.data.height === 'number') {
    // Auto-resize the specific iframe that sent the message
    const height = Math.ceil(event.data.height);
    if (!Number.isFinite(height) || height <= 0) return;
    for (const iframe of iframes) {
      if (event.source === iframe.contentWindow) {
        iframe.style.height = Math.max(100, height) + 'px';
      }
    }
  }
}

// Set up / tear down message listener
onMounted(() => window.addEventListener('message', handleIframeMessage));
onUnmounted(() => {
  window.removeEventListener('message', handleIframeMessage);
  // The iframe's last hover state can otherwise outlive the component (e.g.
  // the user navigates away mid-hover and no mouseout ever fires).
  uiStore.setHoverUrl(null);
});

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
const contactBooks = ref<ContactBook[]>([]);

// The new/edit form itself is the shared ContactFormModal (#166); the
// reader only decides edit-vs-new and hands over the prefill.
const contactForm = ref<InstanceType<typeof ContactFormModal> | null>(null);

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
    contactForm.value?.openEdit(addrMenuContact.value);
  } else {
    // New contact — prefill from the clicked address
    const nameParts = clickedName.trim().split(/\s+/).filter(Boolean);
    contactForm.value?.openNew(defaultBookId, {
      firstName: nameParts[0] || "",
      middleName: nameParts.length > 2 ? nameParts.slice(1, -1).join(" ") : "",
      lastName: nameParts.length > 1 ? nameParts[nameParts.length - 1] : "",
      email: clickedEmail,
    });
  }
}

function onContactSaved(editedId: string | null) {
  showToast(editedId ? "Contact updated" : "Contact added");
}

// --- Message actions ---

// Content the reply/forward quote is built from. When the message was
// decrypted in the reader, use the decrypted plaintext and the recovered
// real subject: the stored copy of an encrypted message carries only the
// ciphertext placeholder body and, for protected-headers mail, a "..."
// placeholder subject.
function effectiveBodyText(): string {
  return (
    decryptedOverlay.value?.body_text ??
    messagesStore.activeMessage?.body_text ??
    ""
  );
}

function effectiveSubject(): string {
  return (
    decryptedOverlay.value?.subject ??
    messagesStore.activeMessage?.subject ??
    ""
  );
}

/** True when the open message arrived PGP-encrypted, so a reply to it
 *  should itself default to encrypted (and signed). */
function repliedMessageWasEncrypted(): boolean {
  return pgpKind.value === "mimeEncrypted" || pgpKind.value === "inlineArmor";
}

function quoteBody(): string {
  const msg = messagesStore.activeMessage;
  if (!msg) return "";
  const text = effectiveBodyText();
  const date = new Date(msg.date).toLocaleString(undefined, { hour12: uiStore.hour12 });
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
  const subject = effectiveSubject();
  // Replying to an encrypted message defaults the reply to sign+encrypt
  // (ComposeView still gates Sign on the account having a signing key).
  const encrypted = repliedMessageWasEncrypted();
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    replyTo: msg.id,
    to: msg.from.email,
    subject: subject.startsWith("Re:") ? subject : `Re: ${subject}`,
    body: quoteBody(),
    pgpEncrypt: encrypted,
    pgpSign: encrypted,
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
  const subject = effectiveSubject();
  const encrypted = repliedMessageWasEncrypted();
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    replyTo: msg.id,
    to: allTo.join(", "),
    cc: allCc.join(", "),
    subject: subject.startsWith("Re:") ? subject : `Re: ${subject}`,
    body: quoteBody(),
    pgpEncrypt: encrypted,
    pgpSign: encrypted,
  });
}

function forward() {
  const msg = messagesStore.activeMessage;
  if (!msg) return;
  const text = effectiveBodyText();
  const subject = effectiveSubject();
  const date = new Date(msg.date).toLocaleString(undefined, { hour12: uiStore.hour12 });
  const from = msg.from.name
    ? `${msg.from.name} <${msg.from.email}>`
    : msg.from.email;
  const toStr = msg.to.map((a) => a.name || a.email).join(", ");
  const fwdHeader = `---------- Forwarded message ----------\nFrom: ${from}\nDate: ${date}\nSubject: ${subject}\nTo: ${toStr}\n\n`;
  openComposeWindow({
    accountId: accountsStore.activeAccountId ?? undefined,
    subject: subject.startsWith("Fwd:") ? subject : `Fwd: ${subject}`,
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
          <span class="header-value">{{ new Date(messagesStore.activeMessage.date).toLocaleString(undefined, { hour12: uiStore.hour12 }) }}</span>
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

      <!-- OpenPGP banners — appear above the body so they're seen
           before the cipher-blob (encrypted) or signed body. -->
      <div v-if="pgpKind === 'mimeEncrypted' || pgpKind === 'inlineArmor'" class="pgp-banner pgp-banner-encrypted">
        <div class="pgp-banner-row">
          <span class="pgp-banner-icon">🔒</span>
          <span v-if="!decryptedOverlay">
            This message is encrypted with OpenPGP.
          </span>
          <span v-else>This message was decrypted.</span>
          <span class="spacer"></span>
          <button
            v-if="!decryptedOverlay"
            class="pgp-btn"
            :disabled="decryptBusy"
            data-testid="reader-decrypt-btn"
            @click="decryptOpenPGP"
          >
            {{ decryptBusy ? "Decrypting…" : "Decrypt" }}
          </button>
          <span
            v-if="decryptedOverlay && verifyOutcome && verifyOutcome.kind !== 'unsigned'"
            :class="verifyBadgeClass(verifyOutcome)"
          >{{ verifyBadgeLabel(verifyOutcome) }}</span>
        </div>
        <div v-if="decryptError" class="pgp-banner-error">{{ decryptError }}</div>
      </div>
      <div
        v-else-if="pgpKind === 'mimeSigned' && verifyOutcome"
        :class="['pgp-banner', verifyBadgeClass(verifyOutcome)]"
        data-testid="reader-signature-badge"
      >
        <div class="pgp-banner-row">
          <span class="pgp-banner-icon">✍️</span>
          <span>{{ verifyBadgeLabel(verifyOutcome) }}</span>
        </div>
      </div>

      <div class="message-body">
        <div
          v-if="showHtml && hasHtml()"
          class="body-html-wrapper"
        >
          <div v-if="hasRemoteImages && !imagesHtml" class="no-remote-notice">
            Remote content blocked
            <button class="load-images-btn" data-testid="reader-load-images" :disabled="loadingImages" @click="loadRemoteImages">
              {{ loadingImages ? 'Loading...' : 'Load images' }}
            </button>
          </div>
          <iframe
            :key="iframeRenderKey"
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
        >{{ displayedBody?.body_text }}</pre>
        <div
          v-else-if="hasHtml()"
          class="body-html-wrapper"
        >
          <div v-if="hasRemoteImages && !imagesHtml" class="no-remote-notice">
            Remote content blocked
            <button class="load-images-btn" data-testid="reader-load-images" :disabled="loadingImages" @click="loadRemoteImages">
              {{ loadingImages ? 'Loading...' : 'Load images' }}
            </button>
          </div>
          <iframe
            :key="iframeRenderKey"
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

    <!-- Shared new/edit contact modal (#166) -->
    <ContactFormModal
      ref="contactForm"
      :books="contactBooks"
      @saved="onContactSaved"
    />
  </div>
</template>

<style scoped>
.message-reader {
  width: 100%;
  min-width: 0;
  height: 100%;
  box-sizing: border-box;
  overflow-x: hidden;
  overflow-y: auto;
  background: var(--color-reader-bg);
  position: relative;
  display: flex;
  flex-direction: column;
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
  flex: 1 0 auto;
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.message-actions {
  display: flex;
  align-items: center;
  justify-content: space-between;
  flex-wrap: wrap;
  gap: 6px 8px;
  padding: 6px 12px;
  border-bottom: 1px solid var(--color-border);
  background: var(--color-bg);
}

.actions-left {
  display: flex;
  align-items: center;
  flex: 1 1 240px;
  flex-wrap: wrap;
  min-width: 0;
  gap: 6px;
}

.actions-right {
  display: flex;
  align-items: center;
  flex: 0 0 auto;
  flex-wrap: wrap;
  justify-content: flex-end;
  margin-left: auto;
  gap: 4px;
}

.pill-btn {
  display: flex;
  align-items: center;
  flex-shrink: 0;
  gap: 5px;
  padding: 5px 12px;
  border-radius: 4px;
  border: none;
  font-size: 14px;
  font-weight: 500;
  color: var(--color-text);
  background: var(--color-bg-tertiary);
  transition: all 0.12s;
  white-space: nowrap;
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
  min-width: 0;
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
  min-width: 0;
  overflow-wrap: anywhere;
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
  flex: 1 0 auto;
  display: flex;
  flex-direction: column;
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
  flex: 1 0 auto;
  display: flex;
  flex-direction: column;
  min-width: 0;
  overflow: hidden;
}

.email-sandbox {
  width: 100%;
  min-height: 100px;
  flex: 1 0 auto;
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

/* OpenPGP banner — shown above the message body. */
.pgp-banner {
  margin: 8px 16px 0 16px;
  padding: 8px 12px;
  border-radius: 6px;
  border: 0.8px solid var(--color-border);
  font-size: 12px;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.pgp-banner-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.pgp-banner-icon {
  font-size: 14px;
}
.pgp-banner-error {
  color: var(--color-danger, #fb2c36);
  font-size: 11px;
}
.pgp-banner-encrypted {
  background: var(--color-bg-tertiary);
}
.pgp-btn {
  padding: 4px 10px;
  border-radius: 4px;
  border: 0.8px solid var(--color-border);
  background: var(--color-accent);
  color: #fff;
  font-size: 12px;
  cursor: pointer;
}
.pgp-btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.pgp-badge {
  padding: 2px 8px;
  border-radius: 4px;
  font-size: 11px;
  font-weight: 600;
}
.pgp-badge-good {
  background: #16a34a;
  color: #fff;
}
.pgp-badge-bad {
  background: var(--color-danger, #fb2c36);
  color: #fff;
}
.pgp-badge-warn {
  background: #d97706;
  color: #fff;
}
.spacer {
  flex: 1;
}
</style>
