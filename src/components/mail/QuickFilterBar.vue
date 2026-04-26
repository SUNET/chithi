<script setup lang="ts">
import { useMessagesStore } from "@/stores/messages";

const messagesStore = useMessagesStore();

function onTextInput() {
  messagesStore.onFilterTextChange();
}
</script>

<template>
  <div class="quick-filter-bar">
    <div class="filter-row">
      <div class="filter-left">
        <div class="input-wrapper">
          <input
            v-model="messagesStore.quickFilterText"
            type="text"
            class="filter-input"
            data-testid="filter-text-input"
            placeholder="Filter messages... (/)"
            @input="onTextInput"
          />
          <button
            v-if="messagesStore.hasActiveFilter"
            class="clear-icon"
            data-testid="filter-clear"
            @click="messagesStore.clearQuickFilters()"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>
          </button>
        </div>
      </div>
      <div class="filter-right">
        <button
          class="filter-btn"
          data-testid="filter-unread"
          :class="{ active: messagesStore.quickFilter.unread }"
          @click="messagesStore.toggleQuickFilter('unread')"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 4h16c1.1 0 2 .9 2 2v12c0 1.1-.9 2-2 2H4c-1.1 0-2-.9-2-2V6c0-1.1.9-2 2-2z"/><polyline points="22,6 12,13 2,6"/></svg>
          Unread
        </button>
        <button
          class="filter-btn"
          data-testid="filter-starred"
          :class="{ active: messagesStore.quickFilter.starred }"
          @click="messagesStore.toggleQuickFilter('starred')"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"/></svg>
          Starred
        </button>
        <button
          class="filter-btn"
          data-testid="filter-contact"
          :class="{ active: messagesStore.quickFilter.contact }"
          @click="messagesStore.toggleQuickFilter('contact')"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/></svg>
          Contact
        </button>
        <button
          class="filter-btn"
          data-testid="filter-attachment"
          :class="{ active: messagesStore.quickFilter.has_attachment }"
          @click="messagesStore.toggleQuickFilter('has_attachment')"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48"/></svg>
          Attachment
        </button>
      </div>
    </div>
    <div v-if="messagesStore.quickFilterText.trim()" class="field-row">
      <span class="field-label">Filter messages by:</span>
      <button
        class="field-btn"
        :class="{ active: messagesStore.quickFilterFields.length === 0 || messagesStore.quickFilterFields.includes('sender') }"
        data-testid="filter-field-sender"
        @click="messagesStore.toggleTextField('sender')"
      >Sender</button>
      <button
        class="field-btn"
        :class="{ active: messagesStore.quickFilterFields.length === 0 || messagesStore.quickFilterFields.includes('recipients') }"
        data-testid="filter-field-recipients"
        @click="messagesStore.toggleTextField('recipients')"
      >Recipients</button>
      <button
        class="field-btn"
        :class="{ active: messagesStore.quickFilterFields.length === 0 || messagesStore.quickFilterFields.includes('subject') }"
        data-testid="filter-field-subject"
        @click="messagesStore.toggleTextField('subject')"
      >Subject</button>
      <button
        class="field-btn"
        :class="{ active: messagesStore.quickFilterFields.length === 0 || messagesStore.quickFilterFields.includes('body') }"
        data-testid="filter-field-body"
        @click="messagesStore.toggleTextField('body')"
      >Body</button>
    </div>
    <div v-if="messagesStore.quickFilterText.trim()" class="server-row">
      <button
        class="server-btn"
        data-testid="search-server-trigger"
        :disabled="messagesStore.serverSearchLoading"
        @click="messagesStore.runServerSearch()"
      >
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/></svg>
        <span v-if="messagesStore.serverSearchLoading" data-testid="search-server-loading">Searching server&hellip;</span>
        <span v-else>Search server for &ldquo;{{ messagesStore.quickFilterText.trim() }}&rdquo;</span>
      </button>
      <span
        v-if="messagesStore.serverSearchError"
        class="server-error"
        data-testid="search-server-error"
      >{{ messagesStore.serverSearchError }}</span>
    </div>
  </div>
</template>

<style scoped>
.quick-filter-bar {
  border-bottom: 1px solid var(--color-border);
  background: var(--color-bg-secondary);
}

.filter-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 4px 8px;
  gap: 8px;
}

.filter-left {
  flex: 1;
  min-width: 150px;
}

.input-wrapper {
  position: relative;
}

.filter-input {
  width: 100%;
  height: 28px;
  padding: 0 28px 0 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  color: var(--color-text);
  font-size: 12px;
  font-family: var(--font-sans);
}

.filter-input:focus {
  outline: none;
  border-color: var(--color-accent);
}

.filter-input::placeholder {
  color: var(--color-text-muted);
}

.clear-icon {
  position: absolute;
  right: 4px;
  top: 50%;
  transform: translateY(-50%);
  background: none;
  border: none;
  cursor: pointer;
  padding: 2px;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--color-text-muted);
  border-radius: 50%;
}

.clear-icon:hover {
  color: var(--color-danger);
}

.filter-right {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-wrap: nowrap;
}

.filter-btn {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 4px 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  color: var(--color-text-secondary);
  font-size: 11px;
  font-weight: 500;
  cursor: pointer;
  white-space: nowrap;
  transition: all 0.12s;
}

.filter-btn:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.filter-btn.active {
  background: var(--color-accent);
  color: white;
  border-color: var(--color-accent);
}

.field-row {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 2px 8px 4px;
}

.field-label {
  font-size: 11px;
  color: var(--color-text-muted);
}

.field-btn {
  padding: 2px 8px;
  border: 1px solid var(--color-border);
  border-radius: 3px;
  background: var(--color-bg);
  color: var(--color-text-secondary);
  font-size: 11px;
  cursor: pointer;
  transition: all 0.12s;
}

.field-btn:hover {
  background: var(--color-bg-hover);
}

.field-btn.active {
  border-color: var(--color-accent);
  color: var(--color-accent);
}

.server-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 2px 8px 6px;
}

.server-btn {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 4px 10px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  color: var(--color-text-secondary);
  font-size: 11px;
  cursor: pointer;
  transition: all 0.12s;
}

.server-btn:hover:not(:disabled) {
  background: var(--color-bg-hover);
  color: var(--color-text);
  border-color: var(--color-accent);
}

.server-btn:disabled {
  opacity: 0.6;
  cursor: progress;
}

.server-error {
  font-size: 11px;
  color: var(--color-danger);
}
</style>
