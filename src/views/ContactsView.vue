<script setup lang="ts">
import { ref, onMounted, onUnmounted, watch } from "vue";
import { storeToRefs } from "pinia";
import { listen } from "@tauri-apps/api/event";
import { useAccountsStore } from "@/stores/accounts";
import { useContactsStore } from "@/stores/contacts";
import { usePlatformStore } from "@/stores/platform";
import { useUiStore } from "@/stores/ui";
import type { ContactBook, Contact } from "@/lib/types";
import * as api from "@/lib/tauri";
import ContactFormModal from "@/components/contacts/ContactFormModal.vue";
import BooksSidebar from "@/components/contacts/BooksSidebar.vue";
import ContactListPanel from "@/components/contacts/ContactListPanel.vue";
import ContactDetailPanel from "@/components/contacts/ContactDetailPanel.vue";
import MergeDialog from "@/components/contacts/MergeDialog.vue";
import MobileContactsPane from "@/components/contacts/MobileContactsPane.vue";

const accountsStore = useAccountsStore();
const contactsStore = useContactsStore();
const platformStore = usePlatformStore();
const uiStore = useUiStore();
const { isMobile } = storeToRefs(platformStore);
// Selected-book / selected-contact state is mirrored from the
// contacts store so that navigating away to Mail / Calendar and back
// restores what the user was looking at. Local refs still drive the
// template — we keep them in sync via watchers below.
const { selectedBookId: storeSelectedBookId } = storeToRefs(contactsStore);

const contactBooks = ref<ContactBook[]>([]);
const contacts = ref<Contact[]>([]);
const searchQuery = ref("");
const selectedBookId = ref<string | null>(storeSelectedBookId.value);
const selectedContact = ref<Contact | null>(null);
const showDeleteConfirm = ref(false);
const deletingContactId = ref<string | null>(null);

// The new/edit contact form lives in ContactFormModal (#166); the view
// drives it through the exposed openNew / openEdit handles.
const contactForm = ref<InstanceType<typeof ContactFormModal> | null>(null);

// Multi-select state (#129). The detail panel still shows the
// most-recently-clicked contact (`selectedContact`), but the contact
// list also tracks an ordered set of selected ids so the user can
// pick TWO entries with Ctrl/Cmd-click and merge them. The first id
// in the array becomes the "keeper" — its identity (id, remote_id,
// etag) survives the merge.
const selectedContactIds = ref<string[]>([]);

// Merge dialog state. `mergePair` is the keeper / loser pair the
// user committed to (snapshotted from the list panel's selection when
// they clicked the toolbar button); the field choices and the apply
// call live in MergeDialog.
const mergePair = ref<{ keeper: Contact; loser: Contact } | null>(null);

const syncing = ref(false);

// --- Independent contact sync ---
// Default to 30 minutes (Thunderbird's CardDAV default). Each account's
// contacts binding can override via `contacts_sync_interval_seconds`
// (#43). The timer ticks every minute and fires per-account when its
// interval has elapsed.
const DEFAULT_CONTACT_INTERVAL_MS = 30 * 60 * 1000;
const CONTACT_TICK_MS = 60 * 1000;
let contactSyncIntervalId: ReturnType<typeof setInterval> | null = null;
const lastContactSync = new Map<string, number>();
let stopContactsChangedListener: (() => void) | null = null;
let disposed = false;

async function contactsTick() {
  // Skip the unconditional fetchBooks() at the end of every tick — it
  // ran listContactBooks for each account once a minute even when no
  // sync was due, which churned the UI for no reason. The
  // `contacts-changed` listener registered in onMounted already
  // refreshes books whenever the backend persists new data, so we only
  // need to call fetchBooks here for the (rare) case where a sync ran
  // *and* its event hasn't already fired refresh logic above.
  const now = Date.now();
  let synced = false;
  for (const acc of accountsStore.accounts) {
    if (!acc.enabled) continue;
    const intervalMs =
      (acc.contacts_sync_interval_seconds ?? 0) > 0
        ? (acc.contacts_sync_interval_seconds as number) * 1000
        : DEFAULT_CONTACT_INTERVAL_MS;
    const last = lastContactSync.get(acc.id) ?? 0;
    if (now - last < intervalMs) continue;
    lastContactSync.set(acc.id, now);
    try {
      await api.syncContacts(acc.id);
      synced = true;
    } catch (e) {
      console.error("Periodic contact sync failed for", acc.id, e);
    }
  }
  // If nothing actually synced this tick, the contacts-changed listener
  // also has nothing to do — let it sleep.
  if (synced) {
    await fetchBooks();
  }
}

onMounted(async () => {
  // Load local data first, then sync in background.
  const idBefore = selectedBookId.value;
  await fetchBooks();
  // fetchBooks only mutates selectedBookId when the previously-saved
  // id is gone, in which case the watcher above already fires off
  // the contacts fetch. When the selection survives (the common
  // case on remount after navigating back to contacts), the value
  // didn't change so the watcher doesn't fire — call the load
  // explicitly so the user lands on the same contact they left.
  // (#150) Skipping it when the id changed avoids an extra
  // listContacts() round-trip on first-ever mount.
  if (selectedBookId.value === idBefore) {
    await loadContactsForSelectedBook();
  }
  syncAllContacts();

  // Start periodic sync (per-account cadence via contacts binding)
  contactSyncIntervalId = setInterval(() => {
    contactsTick().catch((e) => console.error("Contacts tick failed:", e));
  }, CONTACT_TICK_MS);

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

// Monotonic token: the latest fetchBooks() invocation wins. An
// earlier-started call that resolves later is dropped before its
// `contactBooks.value = next` write so stale data can't overwrite
// fresh data.
let fetchBooksSeq = 0;

async function fetchBooks() {
  // Build the list in a local then commit it once at the end. Two
  // concurrent fetchBooks calls (e.g. one from contacts-changed while
  // another is mid-flight after a manual sync) used to interleave
  // appends into the shared `contactBooks.value`, producing duplicates
  // in the sidebar (#130). Dedupe by id along the way as a belt-and-
  // braces guard.
  fetchBooksSeq++;
  const ourSeq = fetchBooksSeq;
  const seen = new Set<string>();
  const next: ContactBook[] = [];
  for (const account of accountsStore.accounts) {
    try {
      const books = await api.listContactBooks(account.id);
      for (const b of books) {
        if (seen.has(b.id)) continue;
        seen.add(b.id);
        next.push(b);
      }
    } catch (e) {
      console.error("Failed to fetch contact books:", e);
    }
  }
  // Drop this result if a newer fetchBooks() has already started; the
  // newer call's commit is authoritative.
  if (ourSeq !== fetchBooksSeq) return;
  contactBooks.value = next;
  // Validate the current selection against the new list — the user's
  // previously-selected book might have been removed (unsubscribed,
  // account deleted, etc). Fall back to the first available book or
  // clear the selection so subsequent listContacts() calls don't fire
  // against a dead id.
  const stillExists =
    selectedBookId.value !== null &&
    next.some((b) => b.id === selectedBookId.value);
  if (!stillExists) {
    selectedBookId.value = next.length > 0 ? next[0].id : null;
  }
}

async function loadContactsForSelectedBook() {
  const bookId = selectedBookId.value;
  if (!bookId) return;
  contacts.value = await api.listContacts(bookId);
  // Restore the previously selected contact from the store on remount,
  // if it still exists in the loaded list. Falls through to null when
  // the user just switched books or the contact is gone.
  const wantedId = contactsStore.selectedContactId;
  selectedContact.value = wantedId
    ? contacts.value.find((c) => c.id === wantedId) ?? null
    : null;
  // Multi-select state belongs to the previously-shown book; drop it
  // on book switch so the merge toolbar can't sit visible with ids
  // that aren't in the current list.
  selectedContactIds.value = [];
}

watch(selectedBookId, () => {
  loadContactsForSelectedBook();
});

// Mirror local selection state into the contacts store so navigating
// away to Mail / Calendar and back lands us on the same contact (#150).
// Sync runs in one direction (local → store) because the store fields
// are never mutated externally; on remount we read them to seed the
// local refs and the watcher above re-loads contacts.
watch(selectedBookId, (id) => {
  if (id !== contactsStore.selectedBookId) contactsStore.setSelectedBook(id);
});
watch(selectedContact, (c) => {
  contactsStore.setSelectedContact(c?.id ?? null);
});

// Prune `selectedContactIds` against the current contact list whenever
// it changes (e.g. after a sync or a CRUD op deletes one of the picked
// contacts). Without this the toolbar can stay visible referencing
// gone-from-the-DOM rows, with `canMergeSelected` permanently false.
watch(contacts, (next) => {
  if (selectedContactIds.value.length === 0) return;
  const visible = new Set(next.map((c) => c.id));
  const pruned = selectedContactIds.value.filter((id) => visible.has(id));
  if (pruned.length !== selectedContactIds.value.length) {
    selectedContactIds.value = pruned;
  }
});

function selectContact(contact: Contact, event?: MouseEvent) {
  // Ctrl/Cmd-click toggles the contact in the multi-select set without
  // disturbing the others. A plain click resets the selection to just
  // this contact (matching the long-standing single-select behavior
  // for users who never use the merge flow).
  selectedContact.value = contact;
  if (event && (event.ctrlKey || event.metaKey)) {
    const idx = selectedContactIds.value.indexOf(contact.id);
    if (idx === -1) {
      selectedContactIds.value = [...selectedContactIds.value, contact.id];
    } else {
      selectedContactIds.value = selectedContactIds.value.filter(
        (id) => id !== contact.id,
      );
    }
  } else {
    selectedContactIds.value = [contact.id];
  }
}

function clearSelection() {
  selectedContactIds.value = selectedContact.value
    ? [selectedContact.value.id]
    : [];
}

/// Open the field-picker dialog for the keeper/loser pair the list
/// panel resolved (first selected wins). Snapshot the pair so
/// subsequent selection changes don't yank the dialog out from under
/// the user.
function startMerge(keeper: Contact, loser: Contact) {
  mergePair.value = { keeper, loser };
}

function openNewForm() {
  contactForm.value?.openNew(selectedBookId.value ?? contactBooks.value[0]?.id ?? "");
}

function openEditForm(contact: Contact) {
  contactForm.value?.openEdit(contact);
}

/// Post-save tail, mirroring the pre-split saveContact: reload the
/// visible list and re-point the detail panel at the updated row. The
/// mobile flat list catches up via the contacts-changed event, as
/// before.
async function onContactSaved(editedId: string | null) {
  if (selectedBookId.value) {
    contacts.value = await api.listContacts(selectedBookId.value);
    // Refresh the detail panel with the updated contact
    if (editedId && selectedContact.value) {
      const updated = contacts.value.find((c) => c.id === editedId);
      if (updated) selectedContact.value = updated;
    }
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

// ---------- Merge (#129) ----------------------------------------------------

/// Merge committed on the server (MergeDialog already ran update-then-
/// delete). Close the dialog *now* so a stuck refresh (or a refresh
/// failure) can't leave it open and tempt the user into clicking Merge
/// a second time — which would attempt to delete the already-deleted
/// loser.
async function onMerged(surviving: Contact) {
  const loserId = mergePair.value?.loser.id;
  mergePair.value = null;
  if (
    selectedContact.value?.id === loserId
    || selectedContact.value?.id === surviving.id
  ) {
    selectedContact.value = surviving;
  }
  selectedContactIds.value = [surviving.id];

  // Best-effort refresh of the visible list. If listContacts throws,
  // log it but don't surface a user-facing error: the merge already
  // committed on the server and the list will catch up on the next
  // sync or book switch.
  if (selectedBookId.value) {
    try {
      contacts.value = await api.listContacts(selectedBookId.value);
    } catch (e) {
      console.error("Post-merge refresh failed:", e);
    }
  }
}
</script>

<template>
  <!-- Mobile: large-title app bar + account chips + sticky letter groups -->
  <MobileContactsPane
    v-if="isMobile"
    v-model:search="searchQuery"
    :books="contactBooks"
    @new-contact="openNewForm"
    @select="selectContact"
  />

  <!-- Desktop layout — unchanged -->
  <div v-else class="contacts-view">
    <div
      class="contacts-body"
      :class="`contacts-layout-${uiStore.contactViewMode}`"
    >
      <!-- Left: Contact Books -->
      <BooksSidebar
        :books="contactBooks"
        :selected-book-id="selectedBookId"
        @select="selectedBookId = $event"
      />

      <!-- Middle + Right wrapper. Direction toggles via the
           `contacts-layout-{right,bottom}` class on the parent: row
           (default) puts the detail card to the right of the list,
           column drops it underneath. (#150) -->
      <div class="contacts-main">

      <!-- Toolbar (#150). Lives inside the main pane so the
           "+ New Contact" button sits above the contact list, mirror-
           ing how Calendar's "+ Event" sits in its own toolbar. The
           "ADDRESS BOOKS" header on the left is the visual partner. -->
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

      <!-- Inner wrapper. Carries the layout-toggle so the toolbar
           above stays at the top regardless of right-vs-bottom mode. -->
      <div class="contacts-content">
      <!-- Middle: Contact List -->
      <ContactListPanel
        v-model:search="searchQuery"
        :contacts="contacts"
        :books="contactBooks"
        :selected-contact-id="selectedContact?.id ?? null"
        :selected-ids="selectedContactIds"
        :has-book="!!selectedBookId"
        @select="selectContact"
        @merge="startMerge"
        @clear-selection="clearSelection"
      />

      <!-- Right: Detail -->
      <ContactDetailPanel
        :contact="selectedContact"
        :books="contactBooks"
        @edit="openEditForm"
        @delete="confirmDelete"
      />
      </div><!-- /.contacts-content -->
      </div><!-- /.contacts-main -->
    </div>

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

    <!-- Merge: field-by-field picker dialog (#129) -->
    <MergeDialog :pair="mergePair" @merged="onMerged" @cancel="mergePair = null" />
  </div>

  <!-- Shared new/edit contact modal (desktop full form, mobile compact
       field subset). Rendered outside the mobile/desktop branches so
       one instance serves both. -->
  <ContactFormModal
    ref="contactForm"
    :books="contactBooks"
    :compact="isMobile"
    @saved="onContactSaved"
  />
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
  background: var(--color-sync-green);
  color: #fff;
  border-radius: var(--radius);
  font-size: 14px;
  font-weight: 500;
  transition: filter 0.12s;
}
.btn-sync:hover { filter: brightness(0.92); }
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
  display: flex;
  flex-direction: column;
}

/* Wrapper around the toolbar + contact-list + detail panes. The
   toolbar always sits at the top so its layout never depends on the
   View > Contact Pane choice (the toggle below applies to
   .contacts-content, not the .contacts-main wrapper). */
.contacts-main {
  flex: 1;
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
}

/* Inner content frame that the right/bottom layout toggle targets. */
.contacts-content {
  flex: 1;
  display: flex;
  min-width: 0;
  min-height: 0;
}
.contacts-layout-right .contacts-content {
  flex-direction: row;
}
.contacts-layout-bottom .contacts-content {
  flex-direction: column;
}

/* Contact List */
.contact-list-panel {
  flex: 1;
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
}
.contacts-layout-right .contact-list-panel {
  border-right: 0.8px solid var(--color-border);
}
.contacts-layout-bottom .contact-list-panel {
  border-bottom: 0.8px solid var(--color-border);
}

/* Detail Panel. Sized differently per layout mode:
   - "right": fixed 400px column, list takes the remaining width.
   - "bottom": flexible split, both panes share the column. */
.detail-panel {
  flex-shrink: 0;
  overflow-y: auto;
}
.contacts-layout-right .detail-panel {
  width: 400px;
}
.contacts-layout-bottom .detail-panel {
  width: auto;
  flex: 1;
  min-height: 0;
}


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

.modal-body { padding: 20px; }

.modal-footer {
  display: flex; justify-content: flex-end; gap: 8px;
  padding: 12px 20px;
  border-top: 0.8px solid var(--color-border);
}

.btn-cancel {
  height: 32px; padding: 0 20px; background: var(--color-bg-tertiary);
  border-radius: 4px; font-size: 16px; font-weight: 500; color: var(--color-text);
}
.btn-delete {
  height: 32px; padding: 0 20px; background: var(--color-danger);
  border-radius: 4px; font-size: 16px; font-weight: 500; color: white;
}

.confirm-title { font-size: 16px; font-weight: 600; margin-bottom: 8px; }
.confirm-text { font-size: 13px; color: var(--color-text-secondary); line-height: 1.5; }
</style>
