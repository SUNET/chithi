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

const hasSelection = () => messagesStore.activeMessageId !== null;

function compose() {
  router.push("/compose");
}

async function deleteSelected() {
  const accountId = accountsStore.activeAccountId;
  const msgId = messagesStore.activeMessageId;
  if (!accountId || !msgId) return;

  try {
    await api.deleteMessages(accountId, [msgId]);
    messagesStore.activeMessage = null;
    messagesStore.activeMessageId = null;
    await messagesStore.fetchMessages();
    await foldersStore.fetchFolders();
  } catch (e) {
    console.error("Delete failed:", e);
  }
}

async function toggleRead() {
  const accountId = accountsStore.activeAccountId;
  const msgId = messagesStore.activeMessageId;
  if (!accountId || !msgId) return;

  const msg = messagesStore.messages.find((m) => m.id === msgId);
  if (!msg) return;

  const isSeen = msg.flags.includes("seen");
  try {
    await api.setMessageFlags(accountId, [msgId], ["seen"], !isSeen);
    await messagesStore.fetchMessages();
    await foldersStore.fetchFolders();
  } catch (e) {
    console.error("Flag change failed:", e);
  }
}

async function toggleStar() {
  const accountId = accountsStore.activeAccountId;
  const msgId = messagesStore.activeMessageId;
  if (!accountId || !msgId) return;

  const msg = messagesStore.messages.find((m) => m.id === msgId);
  if (!msg) return;

  const isFlagged = msg.flags.includes("flagged");
  try {
    await api.setMessageFlags(accountId, [msgId], ["flagged"], !isFlagged);
    await messagesStore.fetchMessages();
  } catch (e) {
    console.error("Flag change failed:", e);
  }
}

async function archiveSelected() {
  const accountId = accountsStore.activeAccountId;
  const msgId = messagesStore.activeMessageId;
  if (!accountId || !msgId) return;

  // Find archive folder
  const archiveFolder = foldersStore.folders.find(
    (f) => f.folder_type === "archive",
  );
  if (!archiveFolder) return;

  try {
    await api.moveMessages(accountId, [msgId], archiveFolder.path);
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
  const msgId = messagesStore.activeMessageId;
  if (!accountId || !msgId) return;

  const spamFolder = foldersStore.folders.find(
    (f) => f.folder_type === "junk",
  );
  if (!spamFolder) return;

  try {
    await api.moveMessages(accountId, [msgId], spamFolder.path);
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
    <button class="toolbar-btn compose-btn" title="Compose" @click="compose">
      Compose
    </button>
    <div class="toolbar-separator"></div>
    <button
      class="toolbar-btn"
      title="Archive"
      :disabled="!hasSelection()"
      @click="archiveSelected"
    >
      Archive
    </button>
    <button
      class="toolbar-btn"
      title="Spam"
      :disabled="!hasSelection()"
      @click="markSpam"
    >
      Spam
    </button>
    <button
      class="toolbar-btn"
      title="Delete"
      :disabled="!hasSelection()"
      @click="deleteSelected"
    >
      Delete
    </button>
    <div class="toolbar-separator"></div>
    <button
      class="toolbar-btn"
      title="Toggle read/unread"
      :disabled="!hasSelection()"
      @click="toggleRead"
    >
      Read
    </button>
    <button
      class="toolbar-btn"
      title="Toggle star"
      :disabled="!hasSelection()"
      @click="toggleStar"
    >
      Star
    </button>
  </div>
</template>

<style scoped>
.toolbar {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 4px 8px;
  background: var(--color-bg-secondary);
  border-bottom: 1px solid var(--color-border);
  flex-shrink: 0;
}

.toolbar-btn {
  padding: 4px 10px;
  border-radius: 4px;
  font-size: 12px;
  color: var(--color-text-secondary);
  white-space: nowrap;
}

.toolbar-btn:hover:not(:disabled) {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.toolbar-btn:disabled {
  opacity: 0.4;
  cursor: default;
}

.compose-btn {
  background: var(--color-accent);
  color: var(--color-bg);
  font-weight: 600;
}

.compose-btn:hover {
  background: var(--color-accent-hover) !important;
  color: var(--color-bg) !important;
}

.toolbar-separator {
  width: 1px;
  height: 20px;
  background: var(--color-border);
  margin: 0 4px;
}
</style>
