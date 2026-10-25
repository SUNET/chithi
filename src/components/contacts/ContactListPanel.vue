<script setup lang="ts">
import { computed } from "vue";
import type { Contact, ContactBook } from "@/lib/types";
import { acctColor } from "@/lib/account-colors";
import { parseFirstEmail } from "@/lib/contact-json";

const props = defineProps<{
  contacts: Contact[];
  books: ContactBook[];
  selectedContactId: string | null;
  selectedIds: string[];
  hasBook: boolean;
}>();
const emit = defineEmits<{
  select: [contact: Contact, event: MouseEvent];
  merge: [keeper: Contact, loser: Contact];
  clearSelection: [];
}>();

const search = defineModel<string>("search", { required: true });

function bookAccountId(bookId: string): string {
  return props.books.find((book) => book.id === bookId)?.account_id ?? "";
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

const selectedContacts = computed(() =>
  props.selectedIds
    .map((id) => props.contacts.find((contact) => contact.id === id))
    .filter((contact): contact is Contact => contact !== undefined),
);

// Only contacts from the same address book can be merged.
const canMergeSelected = computed(
  () =>
    props.selectedIds.length === 2 &&
    selectedContacts.value.length === 2 &&
    selectedContacts.value[0].book_id === selectedContacts.value[1].book_id,
);

const selectedContactNames = computed(() =>
  selectedContacts.value
    .map((contact) => contact.display_name)
    .filter(Boolean),
);

// The first selected contact is the keeper.
function startMerge() {
  if (!canMergeSelected.value) return;
  const [keeper, loser] = selectedContacts.value;
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
          <span class="contact-email">{{ parseFirstEmail(contact.emails_json) }}</span>
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

/* Picked rows remain distinct from the active detail row. */
.contact-row.picked {
  outline: 2px solid var(--color-accent);
  outline-offset: -2px;
}

.contact-avatar {
  width: 40px;
  height: 40px;
  border-radius: 50%;
  color: #fff;
  display: grid;
  place-items: center;
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
