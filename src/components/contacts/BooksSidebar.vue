<script setup lang="ts">
import { useAccountsStore } from "@/stores/accounts";
import type { ContactBook } from "@/lib/types";
import { acctColor } from "@/lib/account-colors";

defineProps<{
  books: ContactBook[];
  selectedBookId: string | null;
}>();
const emit = defineEmits<{ select: [bookId: string] }>();

const accountsStore = useAccountsStore();

function getAccountName(accountId: string): string {
  return accountsStore.accounts.find((account) => account.id === accountId)?.display_name ?? "";
}

function avatarStyle(accountId: string) {
  const { fill, soft } = acctColor(accountId);
  return { background: soft, color: fill, boxShadow: `inset 0 0 0 1.5px ${fill}` };
}
</script>

<template>
  <div class="books-sidebar" data-testid="contacts-book-select">
    <div class="app-sidebar-header">Address Books</div>
    <div
      v-for="book in books"
      :key="book.id"
      class="book-item"
      :class="{ active: selectedBookId === book.id }"
      @click="emit('select', book.id)"
    >
      <span
        class="book-avatar"
        :style="avatarStyle(book.account_id)"
      >
        {{ book.name.charAt(0).toUpperCase() }}
      </span>
      <span class="book-info">
        <span class="book-name">{{ book.name }}</span>
        <span class="book-meta">{{ getAccountName(book.account_id) }}</span>
      </span>
      <svg class="book-chevron" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 18 15 12 9 6" /></svg>
    </div>
    <div v-if="books.length === 0" class="empty-text">No contact books</div>
  </div>
</template>

<style scoped>
.books-sidebar .book-item,
.books-sidebar .empty-text {
  margin: 0 8px;
}
.books-sidebar > .app-sidebar-header + .book-item {
  margin-top: 8px;
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
.book-item.active {
  background: var(--color-accent-light);
  border: 1px solid var(--color-border);
}

.book-avatar {
  width: 30px;
  height: 30px;
  border-radius: 50%;
  display: grid;
  place-items: center;
  font-size: 11px;
  font-weight: 700;
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

.empty-text { padding: 32px 20px; text-align: center; color: var(--color-text-muted); font-size: 14px; }
</style>
