<script setup lang="ts">
import { useFoldersStore } from "@/stores/folders";
import { useAccountsStore } from "@/stores/accounts";
import type { Folder } from "@/lib/types";

const foldersStore = useFoldersStore();
const accountsStore = useAccountsStore();

function folderIcon(folder: Folder): string {
  switch (folder.folder_type) {
    case "inbox":
      return "\uD83D\uDCE5";
    case "sent":
      return "\uD83D\uDCE4";
    case "drafts":
      return "\uD83D\uDCDD";
    case "trash":
      return "\uD83D\uDDD1";
    case "junk":
      return "\u26A0";
    case "archive":
      return "\uD83D\uDCE6";
    default:
      return "\uD83D\uDCC1";
  }
}

function switchAccount(id: string) {
  accountsStore.setActiveAccount(id);
}
</script>

<template>
  <div class="folder-tree">
    <!-- Account selector when multiple accounts -->
    <div v-if="accountsStore.accounts.length > 1" class="account-list">
      <button
        v-for="account in accountsStore.accounts"
        :key="account.id"
        class="account-item"
        :class="{ active: accountsStore.activeAccountId === account.id }"
        @click="switchAccount(account.id)"
        :title="account.email"
      >
        <span class="account-initial">{{ account.display_name.charAt(0).toUpperCase() }}</span>
        <span class="account-name-short">{{ account.display_name }}</span>
      </button>
    </div>

    <div class="folder-header">
      <span class="account-label">{{ accountsStore.activeAccount()?.display_name ?? "No Account" }}</span>
    </div>
    <div class="folder-list">
      <button
        v-for="folder in foldersStore.folders"
        :key="folder.path"
        class="folder-item"
        :class="{ active: foldersStore.activeFolderPath === folder.path }"
        @click="foldersStore.setActiveFolder(folder.path)"
      >
        <span class="folder-icon">{{ folderIcon(folder) }}</span>
        <span class="folder-name">{{ folder.name }}</span>
        <span v-if="folder.unread_count > 0" class="unread-badge">{{ folder.unread_count }}</span>
      </button>
    </div>
  </div>
</template>

<style scoped>
.folder-tree {
  height: 100%;
  display: flex;
  flex-direction: column;
  background: var(--color-bg-secondary);
  border-right: 1px solid var(--color-border);
}

.account-list {
  padding: 4px;
  border-bottom: 1px solid var(--color-border);
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.account-item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 8px;
  border-radius: 4px;
  font-size: 12px;
  text-align: left;
  color: var(--color-text-secondary);
}

.account-item:hover {
  background: var(--color-bg-hover);
}

.account-item.active {
  background: var(--color-bg-active);
  color: var(--color-text);
}

.account-initial {
  width: 20px;
  height: 20px;
  border-radius: 50%;
  background: var(--color-accent);
  color: var(--color-bg);
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 11px;
  font-weight: 600;
  flex-shrink: 0;
}

.account-name-short {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.folder-header {
  padding: 8px 12px;
  border-bottom: 1px solid var(--color-border);
  font-weight: 600;
  font-size: 12px;
  color: var(--color-text-secondary);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.folder-list {
  flex: 1;
  overflow-y: auto;
  padding: 4px;
}

.folder-item {
  display: flex;
  align-items: center;
  width: 100%;
  padding: 6px 8px;
  border-radius: 6px;
  gap: 8px;
  text-align: left;
  transition: background 0.15s;
}

.folder-item:hover {
  background: var(--color-bg-hover);
}

.folder-item.active {
  background: var(--color-bg-active);
}

.folder-icon {
  font-size: 14px;
  flex-shrink: 0;
}

.folder-name {
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.unread-badge {
  background: var(--color-accent);
  color: var(--color-bg);
  font-size: 11px;
  font-weight: 600;
  padding: 1px 6px;
  border-radius: 10px;
  flex-shrink: 0;
}
</style>
