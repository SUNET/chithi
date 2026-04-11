<script setup lang="ts">
import { computed } from "vue";
import { useMessagesStore } from "@/stores/messages";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { openComposeWindow } from "@/lib/compose-window";
import * as api from "@/lib/tauri";

const messagesStore = useMessagesStore();
const accountsStore = useAccountsStore();
const foldersStore = useFoldersStore();

const isJunkFolder = computed(() => {
  const active = foldersStore.activeFolderPath;
  if (!active) return false;
  const folder = foldersStore.folders.find((f) => f.path === active);
  return folder?.folder_type === "junk";
});

const hasSelection = computed(() => messagesStore.selectedIds.length > 0);

async function markNotSpam() {
  const accountId = accountsStore.activeAccountId;
  if (!accountId || !hasSelection.value) return;
  const inboxFolder = foldersStore.folders.find((f) => f.folder_type === "inbox");
  if (!inboxFolder) return;
  const ids = messagesStore.resolveSelectedIds();
  try {
    await api.moveMessages(accountId, ids, inboxFolder.path);
    messagesStore.clearSelection();
    messagesStore.activeMessage = null;
    messagesStore.activeMessageId = null;
  } catch (e) {
    console.error("Not Spam failed:", e);
  }
}
</script>

<template>
  <div class="toolbar">
    <button class="compose-btn" title="Compose new email" data-testid="btn-compose" @click="openComposeWindow({ accountId: accountsStore.activeAccountId ?? undefined })">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M12 20h9M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" />
      </svg>
      Compose
    </button>

    <button
      v-if="isJunkFolder && hasSelection"
      class="not-spam-btn"
      title="Move to Inbox"
      data-testid="btn-not-spam"
      @click="markNotSpam"
    >
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M22 11.08V12a10 10 0 1 1-5.93-9.14" />
        <polyline points="22 4 12 14.01 9 11.01" />
      </svg>
      Not Spam
    </button>

    <span v-if="messagesStore.selectedIds.length > 1" class="selection-count">
      {{ messagesStore.selectedIds.length }} selected
    </span>
  </div>
</template>

<style scoped>
.toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
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
  font-weight: 500;
  font-size: 13px;
  border-radius: 18px;
  padding: 6px 16px;
  transition: all 0.15s;
}

.compose-btn:hover {
  background: var(--color-accent-hover);
}

.not-spam-btn {
  display: flex;
  align-items: center;
  gap: 6px;
  background: var(--color-bg-hover);
  color: var(--color-text);
  font-weight: 500;
  font-size: 13px;
  border-radius: 18px;
  padding: 6px 16px;
  transition: all 0.15s;
}

.not-spam-btn:hover {
  background: var(--color-status-dot);
  color: white;
}

.selection-count {
  font-size: 11px;
  font-weight: 500;
  color: var(--color-text-muted);
}
</style>
