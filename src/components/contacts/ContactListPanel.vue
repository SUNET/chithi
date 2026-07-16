<script setup lang="ts">
/// Middle pane of the desktop contacts view: search, the multi-select
/// merge toolbar (#129) and the contact list. Filtering and the
/// merge-eligibility check live here; selection state and the merge
/// dialog itself stay in ContactsView. The root keeps the
/// `contact-list-panel` class so the view's layout-mode rules
/// (right/bottom split borders) still hit it.
import { computed } from "vue";
import type { Contact, ContactBook } from "@/lib/types";
import { acctColor } from "@/lib/account-colors";
import { parseEmails } from "@/lib/contact-json";

const props = defineProps<{
  contacts: Contact[];
  books: ContactBook[];
  selectedContactId: string | null;
  selectedIds: string[];
  /// Whether a book is selected at all — drives the empty-state row,
  /// which only renders when a book is open but has no matches.
  hasBook: boolean;
}>();
const emit = defineEmits<{
  select: [contact: Contact, event: MouseEvent];
  merge: [keeper: Contact, loser: Contact];
  clearSelection: [];
}>();

const search = defineModel<string>("search", { required: true });

// Per-book / per-contact color is derived from the owning account's UID
// (see src/lib/account-colors.ts) so two books on the same provider get
// distinct colors.
function bookAccountId(bookId: string): string {
  return props.books.find((b) => b.id === bookId)?.account_id ?? "";
}

const filteredContacts = computed(() => {
  if (!search.value.trim()) return props.contacts;
  const q = search.value.toLowerCase();
  return props.contacts.filter(
    (c) =>
      c.display_name.toLowerCase().includes(q) ||
      c.emails_json.toLowerCase().includes(q) ||
      (c.organization ?? "").toLowerCase().includes(q),
  );
});

/// Whether the multi-select toolbar's "Merge" action is enabled.
/// Only fires when exactly two contacts are selected AND both belong
/// to the same address book — cross-book merges aren't supported
/// because the loser's deletion would have to land on a different
/// remote and the surviving vCard would have to be rewritten across
/// two backends.
const canMergeSelected = computed(() => {
  if (props.selectedIds.length !== 2) return false;
  const [a, b] = props.selectedIds
    .map((id) => props.contacts.find((c) => c.id === id))
    .filter((c): c is Contact => !!c);
  if (!a || !b) return false;
  return a.book_id === b.book_id;
});

const selectedContactNames = computed(() =>
  props.selectedIds
    .map((id) => props.contacts.find((c) => c.id === id)?.display_name ?? "")
    .filter((s) => s.length > 0),
);

/// Resolve the keeper/loser pair (first selected wins) and hand it to
/// the parent, which snapshots it and opens the merge dialog.
function startMerge() {
  if (!canMergeSelected.value) return;
  const [keeperId, loserId] = props.selectedIds;
  const keeper = props.contacts.find((c) => c.id === keeperId);
  const loser = props.contacts.find((c) => c.id === loserId);
  if (!keeper || !loser) return;
  emit("merge", keeper, loser);
}
</script>

<template>
  <div class="contact-list-panel">
    <div class="search-bar">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" /></svg>
      <input v-model="search" type="text" placeholder="Search contacts..." data-testid="contacts-search" />
    </div>
    <!-- Multi-select merge toolbar (#129). Visible whenever 2+
         contacts are picked via Ctrl/Cmd-click; the Merge button
         only enables when exactly two are selected within the
         same book. -->
    <div
      v-if="selectedIds.length >= 2"
      class="merge-toolbar"
      data-testid="merge-toolbar"
    >
      <span class="merge-toolbar-text">
        {{ selectedIds.length }} selected{{
          selectedContactNames.length >= 2
            ? `: ${selectedContactNames[0]} + ${selectedContactNames[1]}`
            : ""
        }}
      </span>
      <button
        class="merge-toolbar-btn"
        data-testid="merge-toolbar-btn"
        :disabled="!canMergeSelected"
        :title="canMergeSelected
          ? 'Merge the two selected contacts'
          : 'Pick two contacts in the same address book to merge'"
        @click="startMerge"
      >Merge</button>
      <button class="merge-toolbar-cancel" @click="emit('clearSelection')">Clear</button>
    </div>
    <div class="contact-list">
      <div
        v-for="contact in filteredContacts"
        :key="contact.id"
        class="contact-row"
        :class="{
          active: selectedContactId === contact.id,
          picked: selectedIds.includes(contact.id),
        }"
        :data-testid="`contact-${contact.id}`"
        @click="emit('select', contact, $event)"
      >
        <div
          class="contact-avatar"
          :style="{ background: acctColor(bookAccountId(contact.book_id)).fill }"
        >{{ contact.display_name.charAt(0).toUpperCase() }}</div>
        <div class="contact-info">
          <span class="contact-name">{{ contact.display_name }}</span>
          <span class="contact-email">{{ parseEmails(contact.emails_json)[0]?.email ?? "" }}</span>
          <span v-if="contact.organization" class="contact-org">{{ contact.organization }}</span>
        </div>
      </div>
      <div v-if="filteredContacts.length === 0 && hasBook" class="empty-text">
        {{ search ? "No matches" : "No contacts" }}
      </div>
    </div>
  </div>
</template>

<style scoped>
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

/* Multi-select toolbar above the contact list. Flat strip that
   appears the moment a second contact is picked via Ctrl/Cmd-click. */
.merge-toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
  background: var(--color-bg-secondary);
  border-top: 0.8px solid var(--color-border);
  border-bottom: 0.8px solid var(--color-border);
  font-size: 13px;
}
.merge-toolbar-text {
  flex: 1;
  color: var(--color-text);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.merge-toolbar-btn {
  height: 28px;
  padding: 0 12px;
  border-radius: 4px;
  background: var(--color-accent);
  color: #fff;
  font-size: 13px;
  font-weight: 500;
}
.merge-toolbar-btn:disabled {
  background: var(--color-border);
  color: var(--color-text-muted);
  cursor: not-allowed;
}
.merge-toolbar-cancel {
  height: 28px;
  padding: 0 10px;
  border-radius: 4px;
  background: transparent;
  color: var(--color-text-muted);
  font-size: 13px;
}
.merge-toolbar-cancel:hover { color: var(--color-text); }

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
.contact-row.active {
  background: var(--color-bg-active);
  box-shadow: inset 3px 0 0 var(--color-accent);
}

/* Picked-state highlight for rows in the contact list. Distinct
   from `.active` (which marks the row currently shown in the
   detail panel) so multi-select state stays visible. */
.contact-row.picked {
  outline: 2px solid var(--color-accent);
  outline-offset: -2px;
}

.contact-avatar {
  width: 40px;
  height: 40px;
  border-radius: 50%;
  /* background set inline by acctColor() */
  color: #fff;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 15px;
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

.empty-text { padding: 32px 20px; text-align: center; color: var(--color-text-muted); font-size: 14px; }
</style>
