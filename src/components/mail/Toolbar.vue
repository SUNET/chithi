<script setup lang="ts">
import { useRouter } from "vue-router";
import { useAccountsStore } from "@/stores/accounts";
import { useMessagesStore } from "@/stores/messages";
import { useFoldersStore } from "@/stores/folders";
import * as api from "@/lib/tauri";

const router = useRouter();
const accountsStore = useAccountsStore();
const messagesStore = useMessagesStore();
const foldersStore = useFoldersStore();

const getSelectedIds = () => [...messagesStore.selectedIds];
const hasSelection = () => messagesStore.selectedIds.length > 0;
const selectionCount = () => messagesStore.selectedIds.length;

function compose() {
  router.push("/compose");
}

async function deleteSelected() {
  await messagesStore.deleteSelected();
}

async function toggleRead() {
  const accountId = accountsStore.activeAccountId;
  if (!accountId || !hasSelection()) return;
  try {
    await api.setMessageFlags(accountId, getSelectedIds(), ["seen"], true);
    await messagesStore.fetchMessages();
    await foldersStore.fetchFolders();
  } catch (e) {
    console.error("Flag change failed:", e);
  }
}

async function toggleStar() {
  const accountId = accountsStore.activeAccountId;
  if (!accountId || !hasSelection()) return;
  try {
    await api.setMessageFlags(accountId, getSelectedIds(), ["flagged"], true);
    await messagesStore.fetchMessages();
  } catch (e) {
    console.error("Flag change failed:", e);
  }
}

async function archiveSelected() {
  const accountId = accountsStore.activeAccountId;
  if (!accountId || !hasSelection()) return;
  const folder = foldersStore.folders.find((f) => f.folder_type === "archive");
  if (!folder) return;
  try {
    await api.moveMessages(accountId, getSelectedIds(), folder.path);
    messagesStore.clearSelection();
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
  if (!accountId || !hasSelection()) return;
  const folder = foldersStore.folders.find((f) => f.folder_type === "junk");
  if (!folder) return;
  try {
    await api.moveMessages(accountId, getSelectedIds(), folder.path);
    messagesStore.clearSelection();
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
  <div class="toolbar">
    <button class="compose-btn" title="Compose new email" @click="compose">
      <span class="icon">&#x270F;</span> Compose
    </button>
    <div class="toolbar-separator"></div>
    <button class="icon-btn" title="Archive" :disabled="!hasSelection()" @click="archiveSelected">
      <span class="icon">&#x1F4E6;</span>
    </button>
    <button class="icon-btn" title="Report spam" :disabled="!hasSelection()" @click="markSpam">
      <span class="icon">&#x26A0;</span>
    </button>
    <button class="icon-btn danger" title="Delete" :disabled="!hasSelection()" @click="deleteSelected">
      <span class="icon">&#x1F5D1;</span>
    </button>
    <div class="toolbar-separator"></div>
    <button class="icon-btn" title="Mark as read" :disabled="!hasSelection()" @click="toggleRead">
      <span class="icon">&#x2709;</span>
    </button>
    <button class="icon-btn" title="Star" :disabled="!hasSelection()" @click="toggleStar">
      <span class="icon">&#x2606;</span>
    </button>
    <span v-if="selectionCount() > 1" class="selection-count">
      {{ selectionCount() }} selected
    </span>
  </div>
</template>

<style scoped>
.toolbar {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 6px 12px;
  background: var(--color-bg);
  border-bottom: 1px solid var(--color-border);
  flex-shrink: 0;
}

.compose-btn {
  display: flex;
  align-items: center;
  gap: 6px;
  background: var(--color-accent);
  color: white;
  font-weight: 600;
  font-size: 13px;
  border-radius: 18px;
  padding: 6px 18px;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.12);
  margin-right: 4px;
  transition: all 0.15s;
}

.compose-btn:hover {
  box-shadow: 0 2px 6px rgba(0, 0, 0, 0.2);
  transform: translateY(-1px);
}

.compose-btn .icon {
  font-size: 14px;
}

.icon-btn {
  width: 32px;
  height: 32px;
  border-radius: 50%;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: background 0.12s;
}

.icon-btn .icon {
  font-size: 15px;
  color: var(--color-text-secondary);
}

.icon-btn:hover:not(:disabled) {
  background: var(--color-bg-hover);
}

.icon-btn:hover:not(:disabled) .icon {
  color: var(--color-text);
}

.icon-btn:disabled {
  opacity: 0.25;
  cursor: default;
}

.icon-btn.danger:hover:not(:disabled) {
  background: rgba(243, 139, 168, 0.1);
}

.icon-btn.danger:hover:not(:disabled) .icon {
  color: var(--color-danger);
}

.toolbar-separator {
  width: 1px;
  height: 20px;
  background: var(--color-border);
  margin: 0 6px;
}

.selection-count {
  font-size: 11px;
  font-weight: 500;
  color: var(--color-accent);
  margin-left: 6px;
}
</style>
