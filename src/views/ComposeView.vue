<script setup lang="ts">
import { ref, computed, watch, onMounted, onUnmounted } from "vue";
import { useRoute } from "vue-router";
import { useAccountsStore } from "@/stores/accounts";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open, message as tauriMessage } from "@tauri-apps/plugin-dialog";
import type { Account, ComposeAttachment } from "@/lib/types";
import * as api from "@/lib/tauri";

const route = useRoute();
const accountsStore = useAccountsStore();
const currentWindow = getCurrentWindow();

// Compose window has its own Vue instance — stores are empty.
// Fetch accounts directly and manage locally.
const accounts = ref<Account[]>([]);
const initialAccountId = (route.query.accountId as string) || "";

onMounted(async () => {
  // Try store first (works if opened in same window context)
  if (accountsStore.accounts.length > 0) {
    accounts.value = accountsStore.accounts;
  } else {
    // Separate window — fetch accounts via IPC with timeout
    try {
      let timer: ReturnType<typeof setTimeout>;
      const timeoutPromise = new Promise<Account[]>((_, reject) => {
        timer = setTimeout(() => reject(new Error("Accounts fetch timeout (5s)")), 5000);
      });
      accounts.value = await Promise.race([
        api.listAccounts().finally(() => clearTimeout(timer)),
        timeoutPromise,
      ]);
    } catch (e) {
      const errorMsg = e instanceof Error ? e.message : String(e);
      console.error("Failed to fetch accounts:", errorMsg);
      error.value = `Failed to load accounts: ${errorMsg}`;
    }
  }
  // Set selected account from query param or first account
  if (initialAccountId && accounts.value.some(a => a.id === initialAccountId)) {
    selectedAccountId.value = initialAccountId;
  } else if (accounts.value.length > 0) {
    selectedAccountId.value = accounts.value[0].id;
  } else if (!error.value) {
    // Only set error if we didn't already fail to fetch
    error.value = "No accounts found. Please add an account first.";
  }
});

// WebKitGTK on Linux doesn't forward standard editing shortcuts (Ctrl+Z,
// Ctrl+Shift+Z, Ctrl+A, Ctrl+X/C/V) to secondary WebviewWindows.
// Intercept them and delegate to document.execCommand.
function onEditShortcut(e: KeyboardEvent) {
  if (!(e.ctrlKey || e.metaKey)) return;
  const tag = (e.target as HTMLElement)?.tagName;
  if (tag !== "INPUT" && tag !== "TEXTAREA") return;

  let cmd: string | null = null;
  if (e.key === "z" && e.shiftKey) cmd = "redo";
  else if (e.key === "z") cmd = "undo";
  else if (e.key === "a") cmd = "selectAll";
  else if (e.key === "x") cmd = "cut";
  else if (e.key === "c") cmd = "copy";
  else if (e.key === "v") cmd = "paste";
  if (cmd) {
    document.execCommand(cmd);
  }
}
window.addEventListener("keydown", onEditShortcut);
onUnmounted(() => window.removeEventListener("keydown", onEditShortcut));

// Prefill from query params (reply/reply-all/forward)
const replyToMessageId = (route.query.replyTo as string) || "";
const selectedAccountId = ref("");
const to = ref((route.query.to as string) || "");
const cc = ref((route.query.cc as string) || "");
const bcc = ref("");
const subject = ref((route.query.subject as string) || "");
const bodyText = ref((route.query.body as string) || "");
const sending = ref(false);
const savingDraft = ref(false);
const error = ref<string | null>(null);
const showCc = ref(!!cc.value);
const showBcc = ref(false);
const attachments = ref<ComposeAttachment[]>([]);
const sentSuccessfully = ref(false);

// --- Autocomplete ---
interface AutocompleteItem {
  display: string;  // "Alice Smith"
  email: string;    // "alice@example.com"
  source: string;   // "Contacts" or "Recent"
}

const acResults = ref<AutocompleteItem[]>([]);
const acVisible = ref(false);
const acField = ref<"to" | "cc" | "bcc" | null>(null);
const acSelected = ref(0);
let acDebounce: ReturnType<typeof setTimeout> | null = null;

function getLastTerm(input: string): string {
  // Get the text after the last comma/semicolon (the part being typed)
  const parts = input.split(/[,;]/);
  return (parts[parts.length - 1] || "").trim();
}

function onAddrInput(field: "to" | "cc" | "bcc") {
  acField.value = field;
  const fieldRef = field === "to" ? to : field === "cc" ? cc : bcc;
  const query = getLastTerm(fieldRef.value);

  if (query.length < 2) {
    acVisible.value = false;
    acResults.value = [];
    return;
  }

  if (acDebounce) clearTimeout(acDebounce);
  acDebounce = setTimeout(() => searchAutocomplete(query), 150);
}

async function searchAutocomplete(query: string) {
  try {
    const [contacts, collected] = await Promise.all([
      api.searchContacts(query),
      api.searchCollectedContacts(query),
    ]);

    const items: AutocompleteItem[] = [];
    const seen = new Set<string>();

    // Contacts first (full contacts take priority)
    for (const c of contacts) {
      let emails: { email: string; label: string }[] = [];
      try { emails = JSON.parse(c.emails_json); } catch { continue; }
      for (const e of emails) {
        const key = e.email.toLowerCase();
        if (!seen.has(key)) {
          seen.add(key);
          items.push({
            display: c.display_name,
            email: e.email,
            source: "Contacts",
          });
        }
      }
    }

    // Then collected contacts (recently used)
    for (const c of collected) {
      const key = c.email.toLowerCase();
      if (!seen.has(key)) {
        seen.add(key);
        items.push({
          display: c.name || c.email,
          email: c.email,
          source: "Recent",
        });
      }
    }

    acResults.value = items.slice(0, 8);
    acVisible.value = items.length > 0;
    acSelected.value = 0;
  } catch {
    acVisible.value = false;
  }
}

function selectAutocomplete(item: AutocompleteItem) {
  if (!acField.value) return;
  const fieldRef = acField.value === "to" ? to : acField.value === "cc" ? cc : bcc;
  const parts = fieldRef.value.split(/[,;]/);
  // Replace the last (incomplete) part with the selected email
  parts[parts.length - 1] = ` ${item.display} <${item.email}>`;
  fieldRef.value = parts.join(",") + ", ";
  acVisible.value = false;
  acResults.value = [];
}

function onAddrKeydown(event: KeyboardEvent) {
  if (!acVisible.value || acResults.value.length === 0) return;

  if (event.key === "ArrowDown") {
    event.preventDefault();
    acSelected.value = (acSelected.value + 1) % acResults.value.length;
  } else if (event.key === "ArrowUp") {
    event.preventDefault();
    acSelected.value = (acSelected.value - 1 + acResults.value.length) % acResults.value.length;
  } else if (event.key === "Enter" || event.key === "Tab") {
    if (acVisible.value) {
      event.preventDefault();
      selectAutocomplete(acResults.value[acSelected.value]);
    }
  } else if (event.key === "Escape") {
    acVisible.value = false;
  }
}

function onAddrBlur() {
  // Delay to allow click on dropdown item
  setTimeout(() => {
    acVisible.value = false;
  }, 200);
}

// Signature management — track current signature so we can swap it
// when the user switches accounts in the From dropdown.
const currentSignature = ref("");
const signatureSuffix = ref("");

function buildSignatureBlock(sig: string, hasBody: boolean): string {
  if (!sig) return "";
  // 5 blank lines before signature for new/empty messages,
  // 2 blank lines when appending to existing text (reply/forward)
  const gap = hasBody ? "\n\n" : "\n\n\n\n\n";
  return gap + sig;
}

async function applySignature(accountId: string) {
  try {
    const config = await api.getAccountConfig(accountId);
    const oldBlock = signatureSuffix.value;
    const newSig = config.signature || "";
    const queryBody = (route.query.body as string) || "";
    const hasBody = queryBody.length > 0;
    const newBlock = buildSignatureBlock(newSig, hasBody);

    if (oldBlock && bodyText.value.endsWith(oldBlock)) {
      bodyText.value = bodyText.value.slice(0, -oldBlock.length) + newBlock;
    } else if (newBlock) {
      bodyText.value += newBlock;
    }

    currentSignature.value = newSig;
    signatureSuffix.value = newBlock;
    // Update baseline so signature alone doesn't count as dirty
    baselineBody.value = bodyText.value;
  } catch (e) {
    console.error("Failed to load signature:", e);
  }
}

// Apply signature on initial account selection and when switching accounts
watch(selectedAccountId, (newId) => {
  if (newId) applySignature(newId);
});

// Track initial values to detect changes.
// baselineBody is updated after signature is applied so that
// a new message with only a signature is not considered dirty.
const initialTo = (route.query.to as string) || "";
const initialCc = (route.query.cc as string) || "";
const initialSubject = (route.query.subject as string) || "";
const baselineBody = ref((route.query.body as string) || "");

const isDirty = computed(() =>
  to.value !== initialTo ||
  cc.value !== initialCc ||
  bcc.value !== "" ||
  subject.value !== initialSubject ||
  bodyText.value !== baselineBody.value ||
  attachments.value.length > 0
);

const canSend = computed(() => to.value.trim().length > 0 && !sending.value);

// Intercept window close to prompt for draft save
onMounted(() => {
  currentWindow.onCloseRequested(async (event) => {
    if (sentSuccessfully.value || !isDirty.value) return; // Allow close

    event.preventDefault();

    try {
      const result = await tauriMessage(
        "You have unsaved changes. What would you like to do?",
        {
          title: "Unsaved Changes",
          kind: "warning",
          buttons: { yes: "Save Draft", no: "Discard", cancel: "Cancel" },
        },
      );

      if (result === "Save Draft" || result === "Yes") {
        await saveDraft();
        await currentWindow.destroy();
      } else if (result === "Discard" || result === "No") {
        await currentWindow.destroy();
      }
      // "Cancel" — do nothing, return to compose
    } catch (e) {
      console.error("Close dialog error:", e);
      await currentWindow.destroy();
    }
  });
});

async function saveDraft() {
  const accountId = selectedAccountId.value;
  if (!accountId) return;

  savingDraft.value = true;
  error.value = null;
  try {
    await api.saveDraft(accountId, {
      to: parseAddresses(to.value),
      cc: parseAddresses(cc.value),
      bcc: parseAddresses(bcc.value),
      subject: subject.value,
      body_text: bodyText.value,
      body_html: null,
      attachments: attachments.value,
    });
    // Trigger a sync so the draft appears in the local mailbox
    api.triggerSync(accountId).catch(() => {});
  } catch (e) {
    error.value = `Draft save failed: ${e}`;
  } finally {
    savingDraft.value = false;
  }
}

async function addAttachment() {
  const selected = await open({
    multiple: true,
    title: "Attach Files",
  });
  if (!selected) return;
  const paths = Array.isArray(selected) ? selected : [selected];
  for (const filePath of paths) {
    const name = filePath.split(/[/\\]/).pop() ?? filePath;
    if (!attachments.value.some(a => a.path === filePath)) {
      attachments.value.push({ path: filePath, name });
    }
  }
}

function removeAttachment(index: number) {
  attachments.value.splice(index, 1);
}


function parseAddresses(input: string): string[] {
  return input
    .split(/[,;]/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0)
    .map((s) => {
      // Extract email from "Name <email>" format
      const match = s.match(/<([^>]+)>/);
      return match ? match[1] : s;
    });
}

function mentionsAttachment(): boolean {
  const text = (bodyText.value + "\n" + subject.value).toLowerCase();
  return /\battach(ed|ment|ments|ing)?\b/.test(text);
}

async function send() {
  const accountId = selectedAccountId.value;
  if (!accountId) {
    error.value = "No account selected";
    return;
  }

  const toAddrs = parseAddresses(to.value);
  if (toAddrs.length === 0) {
    error.value = "At least one recipient is required";
    return;
  }

  // Check for missing attachments
  if (attachments.value.length === 0 && mentionsAttachment()) {
    const result = await tauriMessage(
      'Your message mentions an attachment, but no files are attached. Send anyway?',
      {
        title: "No Attachments",
        kind: "warning",
        buttons: { yes: "Send Anyway", no: "Attach Files", cancel: "Cancel" },
      },
    );
    if (result === "Attach Files" || result === "No") {
      await addAttachment();
      return;
    }
    if (result === "Cancel") {
      return;
    }
    // "Send Anyway" / "Yes" — proceed
  }

  sending.value = true;
  error.value = null;

  try {
    await api.sendMessage(accountId, {
      to: toAddrs,
      cc: parseAddresses(cc.value),
      bcc: parseAddresses(bcc.value),
      subject: subject.value,
      body_text: bodyText.value,
      body_html: null,
      attachments: attachments.value,
    });
    if (replyToMessageId) {
      api.setMessageFlags(accountId, [replyToMessageId], ["answered"], true)
        .catch((e) => console.error("Failed to set answered flag:", e));
    }
    sentSuccessfully.value = true;
    currentWindow.close();
  } catch (e) {
    error.value = String(e);
  } finally {
    sending.value = false;
  }
}

</script>

<template>
  <div class="compose-view">
    <!-- Menu Bar -->
    <div class="compose-menubar">
      <span class="menu-item">File</span>
      <span class="menu-item">Edit</span>
      <span class="menu-item">View</span>
      <span class="menu-item">Options</span>
      <span class="menu-item">Tools</span>
      <span class="menu-item">Help</span>
    </div>

    <!-- Toolbar -->
    <div class="compose-toolbar">
      <button class="toolbar-btn" :class="{ disabled: !canSend }" :disabled="!canSend" data-testid="compose-send" @click="send">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <line x1="22" y1="2" x2="11" y2="13" /><polygon points="22 2 15 22 11 13 2 9 22 2" />
        </svg>
        {{ sending ? "Sending..." : "Send" }}
      </button>
      <button class="toolbar-btn">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <rect x="3" y="11" width="18" height="11" rx="2" ry="2" /><path d="M7 11V7a5 5 0 0 1 10 0v4" />
        </svg>
        Encrypt
      </button>
      <button class="toolbar-btn">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 20h9M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" />
        </svg>
        Spelling
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9" /></svg>
      </button>
      <button class="toolbar-btn" :disabled="savingDraft" data-testid="compose-save-draft" @click="saveDraft">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2z" /><polyline points="17 21 17 13 7 13 7 21" /><polyline points="7 3 7 8 15 8" />
        </svg>
        {{ savingDraft ? "Saving..." : "Save" }}
      </button>
      <button class="toolbar-btn">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M23 21v-2a4 4 0 0 0-3-3.87" /><path d="M16 3.13a4 4 0 0 1 0 7.75" />
        </svg>
        Contacts
      </button>
      <div class="toolbar-spacer"></div>
      <button class="toolbar-btn" data-testid="compose-attach" @click="addAttachment">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48" />
        </svg>
        Attach
      </button>
    </div>

    <div v-if="error" class="compose-error">{{ error }}</div>

    <!-- Fields & Body -->
    <div class="compose-body-area">
      <div class="compose-fields">
        <div class="field-row">
          <label class="field-label">From</label>
          <select v-model="selectedAccountId" class="field-select" data-testid="compose-account-select">
            <option v-for="acc in accounts" :key="acc.id" :value="acc.id">
              {{ acc.display_name }} &lt;{{ acc.email }}&gt;
            </option>
          </select>
        </div>
        <div class="field-row addr-field-row">
          <label class="field-label">To</label>
          <div class="field-input-group">
            <div class="addr-input-wrap">
              <input
                v-model="to"
                type="text"
                class="field-input"
                data-testid="compose-to"
                @input="onAddrInput('to')"
                @keydown="onAddrKeydown"
                @blur="onAddrBlur"
                @focus="onAddrInput('to')"
              />
              <div v-if="acVisible && acField === 'to'" class="ac-dropdown">
                <button
                  v-for="(item, i) in acResults"
                  :key="item.email"
                  class="ac-item"
                  :class="{ selected: i === acSelected }"
                  @mousedown.prevent="selectAutocomplete(item)"
                >
                  <span class="ac-name">{{ item.display }}</span>
                  <span class="ac-email">&lt;{{ item.email }}&gt;</span>
                  <span class="ac-source">{{ item.source }}</span>
                </button>
              </div>
            </div>
            <button v-if="!showCc" class="cc-btn" data-testid="compose-cc-toggle" @click="showCc = true">Cc</button>
            <button v-if="!showBcc" class="cc-btn" data-testid="compose-bcc-toggle" @click="showBcc = true">Bcc</button>
          </div>
        </div>
        <div v-if="showCc" class="field-row addr-field-row">
          <label class="field-label">Cc</label>
          <div class="addr-input-wrap">
            <input
              v-model="cc"
              type="text"
              class="field-input"
              data-testid="compose-cc"
              @input="onAddrInput('cc')"
              @keydown="onAddrKeydown"
              @blur="onAddrBlur"
              @focus="onAddrInput('cc')"
            />
            <div v-if="acVisible && acField === 'cc'" class="ac-dropdown">
              <button
                v-for="(item, i) in acResults"
                :key="item.email"
                class="ac-item"
                :class="{ selected: i === acSelected }"
                @mousedown.prevent="selectAutocomplete(item)"
              >
                <span class="ac-name">{{ item.display }}</span>
                <span class="ac-email">&lt;{{ item.email }}&gt;</span>
                <span class="ac-source">{{ item.source }}</span>
              </button>
            </div>
          </div>
        </div>
        <div v-if="showBcc" class="field-row addr-field-row">
          <label class="field-label">Bcc</label>
          <div class="addr-input-wrap">
            <input
              v-model="bcc"
              type="text"
              class="field-input"
              data-testid="compose-bcc"
              @input="onAddrInput('bcc')"
              @keydown="onAddrKeydown"
              @blur="onAddrBlur"
              @focus="onAddrInput('bcc')"
            />
            <div v-if="acVisible && acField === 'bcc'" class="ac-dropdown">
              <button
                v-for="(item, i) in acResults"
                :key="item.email"
                class="ac-item"
                :class="{ selected: i === acSelected }"
                @mousedown.prevent="selectAutocomplete(item)"
              >
                <span class="ac-name">{{ item.display }}</span>
                <span class="ac-email">&lt;{{ item.email }}&gt;</span>
                <span class="ac-source">{{ item.source }}</span>
              </button>
            </div>
          </div>
        </div>
        <div class="field-row">
          <label class="field-label">Subject</label>
          <input v-model="subject" type="text" class="field-input" data-testid="compose-subject" />
        </div>
      </div>

      <div class="compose-divider"></div>

      <textarea
        v-model="bodyText"
        class="compose-textarea"
        data-testid="compose-body"
        autofocus
      ></textarea>

      <!-- Attachment list -->
      <div v-if="attachments.length > 0" class="attachment-bar">
        <div class="attachment-header">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48" />
          </svg>
          <span>{{ attachments.length }} attachment{{ attachments.length !== 1 ? 's' : '' }}</span>
        </div>
        <div class="attachment-list">
          <div v-for="(att, idx) in attachments" :key="att.path" class="attachment-chip">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
              <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" /><polyline points="14 2 14 8 20 8" />
            </svg>
            <span class="attachment-name">{{ att.name }}</span>
            <button class="attachment-remove" title="Remove" @click="removeAttachment(idx)">
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" />
              </svg>
            </button>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.compose-view {
  display: flex;
  flex-direction: column;
  height: 100vh;
  background: var(--color-bg);
}

/* Menu Bar */
.compose-menubar {
  display: flex;
  align-items: center;
  height: 32px;
  padding: 0 8px;
  background: var(--color-bg-secondary);
  border-bottom: 0.8px solid var(--color-border);
  flex-shrink: 0;
  gap: 0;
}

.menu-item {
  padding: 4px 12px;
  font-size: 16px;
  font-weight: 500;
  color: var(--color-text);
  cursor: pointer;
  border-radius: 4px;
}

.menu-item:hover {
  background: var(--color-bg-hover);
}

/* Toolbar */
.compose-toolbar {
  display: flex;
  align-items: center;
  gap: 4px;
  height: 48px;
  padding: 0 12px;
  background: var(--color-bg-secondary);
  border-bottom: 0.8px solid var(--color-border);
  flex-shrink: 0;
}

.toolbar-btn {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 32px;
  padding: 0 12px;
  background: var(--color-bg-tertiary);
  border-radius: 4px;
  font-size: 14px;
  font-weight: 500;
  color: var(--color-text);
  transition: background 0.12s;
  white-space: nowrap;
}

.toolbar-btn:hover:not(:disabled) {
  background: var(--color-border);
}

.toolbar-btn.disabled,
.toolbar-btn:disabled {
  opacity: 0.5;
}

.toolbar-spacer {
  flex: 1;
}

.compose-error {
  padding: 8px 16px;
  background: rgba(251, 44, 54, 0.06);
  color: var(--color-danger-text);
  font-size: 12px;
  flex-shrink: 0;
}

/* Body area */
.compose-body-area {
  flex: 1;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.compose-fields {
  padding: 16px;
  display: flex;
  flex-direction: column;
  gap: 8px;
  flex-shrink: 0;
}

.field-row {
  display: flex;
  align-items: center;
  gap: 12px;
  height: 32px;
}

.field-label {
  width: 80px;
  flex-shrink: 0;
  font-size: 14px;
  font-weight: 500;
  color: var(--color-text-secondary);
}

.field-input {
  flex: 1;
  height: 32px;
  padding: 0 8px;
  border: 0.8px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg-secondary);
  color: var(--color-text);
  font-size: 14px;
}

.field-input:focus {
  outline: none;
  border-color: var(--color-accent);
}

.field-select {
  width: 306px;
  height: 32px;
  padding: 0 8px;
  border: 0.8px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg-secondary);
  color: var(--color-text);
  font-size: 14px;
  appearance: auto;
}

.field-input-group {
  flex: 1;
  display: flex;
  align-items: center;
  gap: 8px;
}

.field-input-group .field-input {
  flex: 1;
}

.addr-input-wrap {
  position: relative;
  flex: 1;
}

.addr-input-wrap .field-input {
  width: 100%;
  box-sizing: border-box;
}

.ac-dropdown {
  position: absolute;
  top: 100%;
  left: 0;
  right: 0;
  z-index: 100;
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 6px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.12);
  margin-top: 2px;
  max-height: 240px;
  overflow-y: auto;
}

.ac-item {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  padding: 8px 12px;
  border: none;
  background: none;
  text-align: left;
  font-size: 13px;
  cursor: pointer;
  color: var(--color-text);
}

.ac-item:hover,
.ac-item.selected {
  background: var(--color-bg-hover);
}

.ac-name {
  font-weight: 500;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.ac-email {
  color: var(--color-text-muted);
  font-size: 12px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.ac-source {
  margin-left: auto;
  font-size: 10px;
  color: var(--color-text-muted);
  background: var(--color-bg-secondary);
  padding: 1px 6px;
  border-radius: 3px;
  flex-shrink: 0;
}

.cc-btn {
  height: 24px;
  padding: 0 6px;
  font-size: 12px;
  font-weight: 500;
  color: var(--color-text-secondary);
  border-radius: 4px;
  transition: all 0.12s;
}

.cc-btn:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.compose-divider {
  height: 1px;
  margin: 0 16px;
  background: var(--color-border);
  flex-shrink: 0;
}

.compose-textarea {
  flex: 1;
  margin: 13px 16px 16px;
  padding: 12px;
  border: 0.8px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg-secondary);
  font-size: 14px;
  line-height: 1.6;
  resize: none;
  color: var(--color-text);
}

.compose-textarea:focus {
  outline: none;
  border-color: var(--color-accent);
}

/* Attachment bar */
.attachment-bar {
  border-top: 0.8px solid var(--color-border);
  padding: 12px 16px;
  flex-shrink: 0;
}

.attachment-header {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  font-weight: 500;
  color: var(--color-text-muted);
  margin-bottom: 8px;
}

.attachment-list {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}

.attachment-chip {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 32px;
  padding: 0 8px 0 10px;
  background: var(--color-bg-secondary);
  border: 0.8px solid var(--color-border);
  border-radius: 6px;
  font-size: 13px;
  color: var(--color-text);
  max-width: 250px;
}

.attachment-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  flex: 1;
}

.attachment-remove {
  width: 20px;
  height: 20px;
  border-radius: 4px;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--color-text-muted);
  flex-shrink: 0;
  transition: all 0.12s;
}

.attachment-remove:hover {
  background: rgba(251, 44, 54, 0.1);
  color: var(--color-danger-text);
}
</style>
