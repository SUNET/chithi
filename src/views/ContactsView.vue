<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, watch } from "vue";
import { listen } from "@tauri-apps/api/event";
import { useAccountsStore } from "@/stores/accounts";
import type { ContactBook, Contact } from "@/lib/types";
import * as api from "@/lib/tauri";

const accountsStore = useAccountsStore();

const contactBooks = ref<ContactBook[]>([]);
const contacts = ref<Contact[]>([]);
const searchQuery = ref("");
const selectedBookId = ref<string | null>(null);
const selectedContact = ref<Contact | null>(null);
const showForm = ref(false);
const showDeleteConfirm = ref(false);
const deletingContactId = ref<string | null>(null);

// Book colors
const bookColors = ["#3b82f6", "#8b5cf6", "#10b981", "#f59e0b", "#ef4444", "#06b6d4"];
function getBookColor(idx: number): string {
  return bookColors[idx % bookColors.length];
}

// Form state
const formFirstName = ref("");
const formMiddleName = ref("");
const formLastName = ref("");
const formEmails = ref<{ email: string; label: string }[]>([{ email: "", label: "work" }]);
const formPhones = ref<{ number: string; label: string }[]>([]);
const formOrg = ref("");
const formTitle = ref("");
const formNotes = ref("");
const formBookId = ref("");
const editingContactId = ref<string | null>(null);
const saving = ref(false);
const error = ref<string | null>(null);

const syncing = ref(false);

// --- Independent contact sync (30-minute interval, matching Thunderbird) ---
const CONTACT_SYNC_INTERVAL = 30 * 60 * 1000; // 30 minutes
let contactSyncIntervalId: ReturnType<typeof setInterval> | null = null;
let stopContactsChangedListener: (() => void) | null = null;
let disposed = false;

onMounted(async () => {
  // Load local data first, then sync in background
  await fetchBooks();
  syncAllContacts();

  // Start periodic sync
  contactSyncIntervalId = setInterval(() => {
    syncAllContacts();
  }, CONTACT_SYNC_INTERVAL);

  // Listen for backend contacts-changed events
  listen<string>("contacts-changed", async () => {
    if (disposed) return;
    await fetchBooks();
    if (selectedBookId.value) {
      contacts.value = await api.listContacts(selectedBookId.value);
    }
  }).then((unlisten) => {
    if (disposed) {
      unlisten();
      return;
    }
    stopContactsChangedListener = unlisten;
  });
});

onUnmounted(() => {
  disposed = true;
  if (contactSyncIntervalId) {
    clearInterval(contactSyncIntervalId);
  }
  stopContactsChangedListener?.();
});

async function syncAllContacts() {
  if (syncing.value) return; // re-entrancy guard: skip if sync already in progress
  syncing.value = true;
  try {
    for (const account of accountsStore.accounts) {
      try {
        await api.syncContacts(account.id);
      } catch (e) {
        console.error("Contact sync failed for", account.id, e);
      }
    }
    await fetchBooks();
  } finally {
    syncing.value = false;
  }
}

async function fetchBooks() {
  contactBooks.value = [];
  for (const account of accountsStore.accounts) {
    try {
      const books = await api.listContactBooks(account.id);
      contactBooks.value = contactBooks.value.concat(books);
    } catch (e) {
      console.error("Failed to fetch contact books:", e);
    }
  }
  if (contactBooks.value.length > 0 && !selectedBookId.value) {
    selectedBookId.value = contactBooks.value[0].id;
  }
}

watch(selectedBookId, async (bookId) => {
  if (bookId) {
    contacts.value = await api.listContacts(bookId);
    selectedContact.value = null;
  }
});

const filteredContacts = computed(() => {
  if (!searchQuery.value.trim()) return contacts.value;
  const q = searchQuery.value.toLowerCase();
  return contacts.value.filter(
    (c) =>
      c.display_name.toLowerCase().includes(q) ||
      c.emails_json.toLowerCase().includes(q) ||
      (c.organization ?? "").toLowerCase().includes(q),
  );
});

function parseEmails(json: string): { email: string; label: string }[] {
  try { return JSON.parse(json); } catch { return []; }
}

function parsePhones(json: string): { number: string; label: string }[] {
  try { return JSON.parse(json); } catch { return []; }
}

function selectContact(contact: Contact) {
  selectedContact.value = contact;
}

function splitDisplayName(name: string): { first: string; middle: string; last: string } {
  const parts = name.trim().split(/\s+/);
  if (parts.length === 1) return { first: parts[0], middle: "", last: "" };
  if (parts.length === 2) return { first: parts[0], middle: "", last: parts[1] };
  return { first: parts[0], middle: parts.slice(1, -1).join(" "), last: parts[parts.length - 1] };
}

function buildDisplayName(): string {
  const parts = [formFirstName.value.trim(), formMiddleName.value.trim(), formLastName.value.trim()].filter(Boolean);
  return parts.join(" ");
}

function openNewForm() {
  editingContactId.value = null;
  formFirstName.value = "";
  formMiddleName.value = "";
  formLastName.value = "";
  formEmails.value = [{ email: "", label: "work" }];
  formPhones.value = [];
  formOrg.value = "";
  formTitle.value = "";
  formNotes.value = "";
  formBookId.value = selectedBookId.value ?? contactBooks.value[0]?.id ?? "";
  error.value = null;
  showForm.value = true;
}

function openEditForm(contact: Contact) {
  editingContactId.value = contact.id;
  const nameParts = splitDisplayName(contact.display_name);
  formFirstName.value = nameParts.first;
  formMiddleName.value = nameParts.middle;
  formLastName.value = nameParts.last;
  formEmails.value = parseEmails(contact.emails_json);
  if (formEmails.value.length === 0) formEmails.value = [{ email: "", label: "work" }];
  formPhones.value = parsePhones(contact.phones_json);
  formOrg.value = contact.organization ?? "";
  formTitle.value = contact.title ?? "";
  formNotes.value = contact.notes ?? "";
  formBookId.value = contact.book_id;
  error.value = null;
  showForm.value = true;
}

function addEmailField() { formEmails.value.push({ email: "", label: "work" }); }
function removeEmailField(idx: number) { formEmails.value.splice(idx, 1); }
function addPhoneField() { formPhones.value.push({ number: "", label: "mobile" }); }
function removePhoneField(idx: number) { formPhones.value.splice(idx, 1); }

async function saveContact() {
  if (!formFirstName.value.trim()) { error.value = "First name is required"; return; }
  if (!formLastName.value.trim()) { error.value = "Last name is required"; return; }
  saving.value = true;
  error.value = null;

  const displayName = buildDisplayName();
  const emailsFiltered = formEmails.value.filter((e) => e.email.trim());
  const phonesFiltered = formPhones.value.filter((p) => p.number.trim());

  try {
    if (editingContactId.value) {
      const existing = selectedContact.value!;
      await api.updateContact({
        ...existing,
        display_name: displayName,
        emails_json: JSON.stringify(emailsFiltered),
        phones_json: JSON.stringify(phonesFiltered),
        organization: formOrg.value || null,
        title: formTitle.value || null,
        notes: formNotes.value || null,
        book_id: formBookId.value,
      });
    } else {
      await api.createContact({
        book_id: formBookId.value,
        display_name: displayName,
        emails_json: JSON.stringify(emailsFiltered),
        phones_json: JSON.stringify(phonesFiltered),
        addresses_json: "[]",
        organization: formOrg.value || null,
        title: formTitle.value || null,
        notes: formNotes.value || null,
      });
    }
    showForm.value = false;
    if (selectedBookId.value) {
      contacts.value = await api.listContacts(selectedBookId.value);
      // Refresh the detail panel with the updated contact
      if (editingContactId.value && selectedContact.value) {
        const updated = contacts.value.find(c => c.id === editingContactId.value);
        if (updated) selectedContact.value = updated;
      }
    }
  } catch (e) {
    error.value = String(e);
  } finally {
    saving.value = false;
  }
}

function confirmDelete(id: string) {
  deletingContactId.value = id;
  showDeleteConfirm.value = true;
}

async function doDelete() {
  if (!deletingContactId.value) return;
  await api.deleteContact(deletingContactId.value);
  showDeleteConfirm.value = false;
  if (selectedContact.value?.id === deletingContactId.value) selectedContact.value = null;
  deletingContactId.value = null;
  if (selectedBookId.value) contacts.value = await api.listContacts(selectedBookId.value);
}

function getAccountName(accountId: string): string {
  return accountsStore.accounts.find((a) => a.id === accountId)?.display_name ?? "";
}
</script>

<template>
  <div class="contacts-view">
    <!-- Toolbar -->
    <div class="contacts-toolbar">
      <button class="btn-new" data-testid="contacts-new-btn" @click="openNewForm">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M16 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" /><circle cx="8.5" cy="7" r="4" /><line x1="20" y1="8" x2="20" y2="14" /><line x1="23" y1="11" x2="17" y2="11" /></svg>
        New Contact
      </button>
      <div class="toolbar-sep"></div>
      <button class="btn-sync" :disabled="syncing" @click="syncAllContacts">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" :class="{ spinning: syncing }"><path d="M21 2v6h-6M3 12a9 9 0 0 1 15-6.7L21 8M3 22v-6h6M21 12a9 9 0 0 1-15 6.7L3 16" /></svg>
        {{ syncing ? "Syncing..." : "Sync" }}
      </button>
    </div>

    <div class="contacts-body">
      <!-- Left: Contact Books -->
      <div class="books-sidebar" data-testid="contacts-book-select">
        <div
          v-for="(book, idx) in contactBooks"
          :key="book.id"
          class="book-item"
          :class="{ active: selectedBookId === book.id }"
          @click="selectedBookId = book.id"
        >
          <span class="book-avatar" :style="{ background: getBookColor(idx) }">
            {{ book.name.charAt(0).toUpperCase() }}
          </span>
          <span class="book-info">
            <span class="book-name">{{ book.name }}</span>
            <span class="book-meta">{{ getAccountName(book.account_id) }}</span>
          </span>
          <svg class="book-chevron" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 18 15 12 9 6" /></svg>
        </div>
        <div v-if="contactBooks.length === 0" class="empty-text">No contact books</div>
      </div>

      <!-- Middle: Contact List -->
      <div class="contact-list-panel">
        <div class="search-bar">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" /></svg>
          <input v-model="searchQuery" type="text" placeholder="Search contacts..." data-testid="contacts-search" />
        </div>
        <div class="contact-list">
          <div
            v-for="contact in filteredContacts"
            :key="contact.id"
            class="contact-row"
            :class="{ active: selectedContact?.id === contact.id }"
            :data-testid="`contact-${contact.id}`"
            @click="selectContact(contact)"
          >
            <div class="contact-avatar">{{ contact.display_name.charAt(0).toUpperCase() }}</div>
            <div class="contact-info">
              <span class="contact-name">{{ contact.display_name }}</span>
              <span class="contact-email">{{ parseEmails(contact.emails_json)[0]?.email ?? "" }}</span>
              <span v-if="contact.organization" class="contact-org">{{ contact.organization }}</span>
            </div>
          </div>
          <div v-if="filteredContacts.length === 0 && selectedBookId" class="empty-text">
            {{ searchQuery ? "No matches" : "No contacts" }}
          </div>
        </div>
      </div>

      <!-- Right: Detail -->
      <div class="detail-panel">
        <template v-if="selectedContact">
          <div class="detail-header">
            <div class="detail-avatar">{{ selectedContact.display_name.charAt(0).toUpperCase() }}</div>
            <div class="detail-info">
              <h2 data-testid="contact-detail-name">{{ selectedContact.display_name }}</h2>
              <span v-if="selectedContact.organization" class="detail-org">
                {{ selectedContact.title ? `${selectedContact.title}, ` : "" }}{{ selectedContact.organization }}
              </span>
            </div>
          </div>
          <div class="detail-actions">
            <button class="action-btn" data-testid="contact-edit-btn" @click="openEditForm(selectedContact!)">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" /></svg>
              Edit
            </button>
            <button class="action-btn danger" data-testid="contact-delete-btn" @click="confirmDelete(selectedContact!.id)">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /></svg>
              Delete
            </button>
          </div>
          <div class="detail-fields">
            <div v-for="em in parseEmails(selectedContact.emails_json)" :key="em.email" class="field-row">
              <span class="field-label">{{ em.label }}</span>
              <span class="field-value" data-testid="contact-detail-email">{{ em.email }}</span>
            </div>
            <div v-for="ph in parsePhones(selectedContact.phones_json)" :key="ph.number" class="field-row">
              <span class="field-label">{{ ph.label }}</span>
              <span class="field-value" data-testid="contact-detail-phone">{{ ph.number }}</span>
            </div>
            <div v-if="selectedContact.notes" class="field-row">
              <span class="field-label">Notes</span>
              <span class="field-value notes">{{ selectedContact.notes }}</span>
            </div>
          </div>
        </template>
        <div v-else class="empty-text">Select a contact to view details</div>
      </div>
    </div>

    <!-- New/Edit Contact Modal -->
    <Teleport to="body">
      <div v-if="showForm" class="modal-overlay" @click.self="showForm = false">
        <div class="modal">
          <div class="modal-header">
            <h3>{{ editingContactId ? "Edit Contact" : "New Contact" }}</h3>
            <button class="modal-close" @click="showForm = false">
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
            </button>
          </div>
          <div class="modal-body">
            <div v-if="error" class="form-error">{{ error }}</div>

            <div class="form-group">
              <label>Contact Book</label>
              <select v-model="formBookId">
                <option v-for="book in contactBooks" :key="book.id" :value="book.id">
                  {{ book.name }} ({{ getAccountName(book.account_id) }})
                </option>
              </select>
            </div>

            <div class="name-row">
              <div class="form-group">
                <label>First Name *</label>
                <input v-model="formFirstName" type="text" placeholder="First" autofocus />
              </div>
              <div class="form-group">
                <label>Middle Name</label>
                <input v-model="formMiddleName" type="text" placeholder="Middle" />
              </div>
              <div class="form-group">
                <label>Last Name *</label>
                <input v-model="formLastName" type="text" placeholder="Last" />
              </div>
            </div>

            <div class="form-group">
              <label>Email</label>
              <div v-for="(em, idx) in formEmails" :key="idx" class="multi-row">
                <input v-model="em.email" type="email" placeholder="email@example.com" />
                <select v-model="em.label">
                  <option value="work">Work</option>
                  <option value="home">Home</option>
                  <option value="other">Other</option>
                </select>
                <button v-if="formEmails.length > 1" class="rm-btn" @click="removeEmailField(idx)">&times;</button>
              </div>
              <button class="add-btn" @click="addEmailField">+ Add email</button>
            </div>

            <div class="form-group">
              <label>Phone</label>
              <div v-for="(ph, idx) in formPhones" :key="idx" class="multi-row">
                <input v-model="ph.number" type="tel" placeholder="+1 (555) 123-4567" />
                <select v-model="ph.label">
                  <option value="mobile">Mobile</option>
                  <option value="work">Work</option>
                  <option value="home">Home</option>
                </select>
                <button class="rm-btn" @click="removePhoneField(idx)">&times;</button>
              </div>
              <button class="add-btn" @click="addPhoneField">+ Add phone</button>
            </div>

            <div class="form-group">
              <label>Organization</label>
              <input v-model="formOrg" type="text" placeholder="Company name" />
            </div>

            <div class="form-group">
              <label>Job Title</label>
              <input v-model="formTitle" type="text" placeholder="Job title" />
            </div>

            <div class="form-group">
              <label>Notes</label>
              <textarea v-model="formNotes" rows="3" placeholder="Notes"></textarea>
            </div>
          </div>
          <div class="modal-footer">
            <button class="btn-cancel" @click="showForm = false">Cancel</button>
            <button class="btn-save" :disabled="saving" data-testid="contact-save-btn" @click="saveContact">
              {{ saving ? "Saving..." : editingContactId ? "Save" : "Add Contact" }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>

    <!-- Delete Confirm -->
    <Teleport to="body">
      <div v-if="showDeleteConfirm" class="modal-overlay" @click.self="showDeleteConfirm = false">
        <div class="modal modal-sm">
          <div class="modal-body">
            <h3 class="confirm-title">Delete Contact</h3>
            <p class="confirm-text">Are you sure? This cannot be undone.</p>
          </div>
          <div class="modal-footer">
            <button class="btn-cancel" @click="showDeleteConfirm = false">Cancel</button>
            <button class="btn-delete" @click="doDelete">Delete</button>
          </div>
        </div>
      </div>
    </Teleport>
  </div>
</template>

<style scoped>
.contacts-view {
  display: flex;
  flex-direction: column;
  height: 100%;
  background: var(--color-bg);
}

/* Toolbar */
.contacts-toolbar {
  display: flex;
  align-items: center;
  gap: 4px;
  height: 48px;
  padding: 0 16px;
  background: var(--color-bg-secondary);
  border-bottom: 0.8px solid var(--color-border);
  flex-shrink: 0;
}

.btn-new {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 32px;
  padding: 0 16px;
  background: var(--color-accent);
  color: white;
  border-radius: 999px;
  font-size: 14px;
  font-weight: 500;
  transition: background 0.12s;
}
.btn-new:hover { background: var(--color-accent-hover); }

.toolbar-sep {
  width: 1px;
  height: 24px;
  background: var(--color-border);
  margin: 0 8px;
}

.btn-sync {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 32px;
  padding: 0 12px;
  background: #00a63e;
  color: white;
  border-radius: 4px;
  font-size: 14px;
  font-weight: 500;
  transition: background 0.12s;
}
.btn-sync:hover { background: #008f35; }
.btn-sync:disabled { opacity: 0.7; }
.spinning { animation: spin 1s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

/* Body */
.contacts-body {
  flex: 1;
  display: flex;
  overflow: hidden;
}

/* Books Sidebar */
.books-sidebar {
  width: 220px;
  flex-shrink: 0;
  background: var(--color-bg-secondary);
  border-right: 0.8px solid var(--color-border);
  overflow-y: auto;
  padding: 8px;
}

.book-item {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 8px;
  border-radius: 6px;
  text-align: left;
  transition: background 0.12s;
  margin-bottom: 2px;
}
.book-item:hover { background: var(--color-bg-hover); }
.book-item.active { background: var(--color-bg-tertiary); }

.book-avatar {
  width: 24px;
  height: 24px;
  border-radius: 50%;
  color: white;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 10px;
  font-weight: 600;
  flex-shrink: 0;
}

.book-info {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
}

.book-name {
  font-size: 12px;
  font-weight: 500;
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.book-meta {
  font-size: 10px;
  color: var(--color-text-muted);
}

.book-chevron {
  flex-shrink: 0;
  color: var(--color-text-muted);
}

/* Contact List */
.contact-list-panel {
  flex: 1;
  display: flex;
  flex-direction: column;
  min-width: 0;
  border-right: 0.8px solid var(--color-border);
}

.search-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 32px;
  margin: 8px;
  padding: 0 12px;
  background: var(--color-bg-secondary);
  border: 0.8px solid var(--color-border);
  border-radius: 6px;
  color: var(--color-text-muted);
  flex-shrink: 0;
}

.search-bar input {
  flex: 1;
  border: none;
  background: transparent;
  font-size: 14px;
  outline: none;
  color: var(--color-text);
}

.contact-list {
  flex: 1;
  overflow-y: auto;
}

.contact-row {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 12px 16px;
  border-bottom: 0.8px solid var(--color-border);
  cursor: pointer;
  transition: background 0.12s;
}
.contact-row:hover { background: var(--color-bg-hover); }
.contact-row.active { background: var(--color-bg-tertiary); }

.contact-avatar {
  width: 48px;
  height: 48px;
  border-radius: 50%;
  background: var(--color-accent);
  color: white;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 16px;
  font-weight: 600;
  flex-shrink: 0;
}

.contact-info {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.contact-name { font-size: 18px; font-weight: 500; color: var(--color-text); }
.contact-email { font-size: 14px; color: var(--color-text-secondary); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.contact-org { font-size: 12px; color: var(--color-text-muted); }

/* Detail Panel */
.detail-panel {
  width: 400px;
  flex-shrink: 0;
  overflow-y: auto;
}

.detail-header {
  display: flex;
  align-items: center;
  gap: 16px;
  padding: 24px 20px;
  border-bottom: 0.8px solid var(--color-border);
}

.detail-avatar {
  width: 56px;
  height: 56px;
  border-radius: 50%;
  background: var(--color-accent);
  color: white;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 20px;
  font-weight: 600;
  flex-shrink: 0;
}

.detail-info { flex: 1; }
.detail-info h2 { font-size: 20px; font-weight: 600; }
.detail-org { font-size: 14px; color: var(--color-text-muted); }

.detail-actions {
  display: flex;
  gap: 8px;
  padding: 12px 20px;
  border-bottom: 0.8px solid var(--color-border);
}

.action-btn {
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
}
.action-btn:hover { background: var(--color-border); }
.action-btn.danger { color: var(--color-danger-text); }
.action-btn.danger:hover { background: rgba(251, 44, 54, 0.08); }

.detail-fields { padding: 16px 20px; }

.field-row {
  display: flex;
  gap: 12px;
  padding: 10px 0;
  border-bottom: 0.8px solid var(--color-border);
  align-items: baseline;
}

.field-label {
  width: 70px;
  flex-shrink: 0;
  font-size: 12px;
  font-weight: 500;
  color: var(--color-text-muted);
  text-transform: capitalize;
}

.field-value { font-size: 14px; color: var(--color-text); }
.field-value.notes { white-space: pre-wrap; color: var(--color-text-secondary); }

.empty-text { padding: 32px 20px; text-align: center; color: var(--color-text-muted); font-size: 14px; }

/* Modal */
.modal-overlay {
  position: fixed; top: 0; left: 0; right: 0; bottom: 0;
  background: rgba(0, 0, 0, 0.6);
  display: flex; align-items: center; justify-content: center;
  z-index: 1000;
}

.modal {
  background: var(--color-bg-secondary);
  border-radius: 10px;
  width: 540px;
  max-height: 85vh;
  overflow-y: auto;
  box-shadow: 0 20px 25px -5px rgba(0,0,0,0.1), 0 8px 10px -6px rgba(0,0,0,0.1);
}
.modal-sm { width: 400px; }

.modal-header {
  display: flex; justify-content: space-between; align-items: center;
  padding: 16px 20px;
  border-bottom: 0.8px solid var(--color-border);
}
.modal-header h3 { font-size: 18px; font-weight: 600; }

.modal-close {
  width: 32px; height: 32px; border-radius: 4px;
  display: flex; align-items: center; justify-content: center;
  color: var(--color-text-muted);
}
.modal-close:hover { background: var(--color-bg-hover); }

.modal-body { padding: 20px; }

.modal-footer {
  display: flex; justify-content: flex-end; gap: 8px;
  padding: 12px 20px;
  border-top: 0.8px solid var(--color-border);
}

.form-error {
  padding: 8px 12px; background: rgba(251,44,54,0.06);
  color: var(--color-danger-text); border-radius: 6px; margin-bottom: 16px; font-size: 12px;
}

.form-group { margin-bottom: 16px; }
.form-group label { display: block; margin-bottom: 4px; font-size: 14px; font-weight: 500; color: var(--color-text-secondary); }
.form-group input, .form-group select, .form-group textarea {
  width: 100%; height: 36px; padding: 0 12px;
  border: 0.8px solid var(--color-border); border-radius: 4px;
  background: var(--color-bg-secondary); font-size: 16px;
}
.form-group textarea { height: 96px; padding: 8px 12px; resize: vertical; line-height: 1.5; }
.form-group input:focus, .form-group select:focus, .form-group textarea:focus {
  outline: none; border-color: var(--color-accent);
}

.name-row { display: flex; gap: 8px; margin-bottom: 16px; }
.name-row .form-group { flex: 1; margin-bottom: 0; }

.multi-row { display: flex; gap: 6px; margin-bottom: 6px; }
.multi-row input { flex: 1; }
.multi-row select { width: 100px; flex-shrink: 0; }

.rm-btn {
  width: 36px; height: 36px; border-radius: 4px; font-size: 18px;
  color: var(--color-text-muted); display: flex; align-items: center; justify-content: center;
}
.rm-btn:hover { background: rgba(251,44,54,0.08); color: var(--color-danger-text); }

.add-btn { font-size: 13px; font-weight: 500; color: var(--color-accent); padding: 4px 0; }

.btn-cancel {
  height: 32px; padding: 0 20px; background: var(--color-bg-tertiary);
  border-radius: 4px; font-size: 16px; font-weight: 500; color: var(--color-text);
}
.btn-save {
  height: 32px; padding: 0 20px; background: var(--color-accent);
  border-radius: 4px; font-size: 16px; font-weight: 500; color: white;
}
.btn-save:disabled { opacity: 0.5; }
.btn-delete {
  height: 32px; padding: 0 20px; background: var(--color-danger);
  border-radius: 4px; font-size: 16px; font-weight: 500; color: white;
}

.confirm-title { font-size: 16px; font-weight: 600; margin-bottom: 8px; }
.confirm-text { font-size: 13px; color: var(--color-text-secondary); line-height: 1.5; }
</style>
