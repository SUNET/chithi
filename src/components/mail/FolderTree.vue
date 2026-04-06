<script setup lang="ts">
import { ref, onMounted, watch } from "vue";
import { useFoldersStore } from "@/stores/folders";
import { useAccountsStore } from "@/stores/accounts";
import { useMessagesStore } from "@/stores/messages";
import type { Folder } from "@/lib/types";
import * as api from "@/lib/tauri";

const foldersStore = useFoldersStore();
const accountsStore = useAccountsStore();
const messagesStore = useMessagesStore();

const contextMenu = ref<{ x: number; y: number; folder: Folder } | null>(null);
const syncing = ref<string | null>(null);
const collapsedAccounts = ref<string[]>([]);

// Predefined avatar colors for accounts
const avatarColors = ["#3366cc", "#2e7d32", "#9c27b0", "#e65100", "#00838f"];

function getAvatarColor(index: number): string {
  return avatarColors[index % avatarColors.length];
}

function getInitials(name: string): string {
  const words = name.split(/\s+/);
  if (words.length >= 2) {
    return (words[0][0] + words[1][0]).toUpperCase();
  }
  return name.slice(0, 2).toUpperCase();
}

function folderIcon(folder: Folder): string {
  switch (folder.folder_type) {
    case "inbox": return "inbox";
    case "sent": return "sent";
    case "drafts": return "drafts";
    case "trash": return "trash";
    case "junk": return "spam";
    case "archive": return "archive";
    default: return "folder";
  }
}

function toggleAccountCollapse(accountId: string) {
  const idx = collapsedAccounts.value.indexOf(accountId);
  if (idx !== -1) {
    collapsedAccounts.value = collapsedAccounts.value.filter(id => id !== accountId);
  } else {
    collapsedAccounts.value = [...collapsedAccounts.value, accountId];
  }
}

function selectFolder(accountId: string, folderPath: string) {
  // Set the folder path BEFORE switching accounts so the watcher
  // in the folders store doesn't reset it to Inbox.
  foldersStore.setActiveFolder(folderPath);
  if (accountsStore.activeAccountId !== accountId) {
    accountsStore.setActiveAccount(accountId);
  }
}

onMounted(() => {
  foldersStore.fetchAllAccountFolders();
});

// Re-fetch all folders when accounts list changes (e.g. after initial load)
watch(
  () => accountsStore.accounts.length,
  () => {
    foldersStore.fetchAllAccountFolders();
  },
);

function onFolderContextMenu(event: MouseEvent, folder: Folder) {
  event.preventDefault();
  contextMenu.value = { x: event.clientX, y: event.clientY, folder };
}

function closeContextMenu() {
  contextMenu.value = null;
}

async function syncThisFolder() {
  const accountId = accountsStore.activeAccountId;
  const folder = contextMenu.value?.folder;
  if (!accountId || !folder) return;
  closeContextMenu();

  syncing.value = folder.path;
  try {
    await api.syncFolder(accountId, folder.path);
    await foldersStore.fetchFolders();
    if (foldersStore.activeFolderPath === folder.path) {
      await messagesStore.fetchMessages();
    }
  } catch (e) {
    console.error("Folder sync failed:", e);
  } finally {
    syncing.value = null;
  }
}
</script>

<template>
  <div class="folder-tree" @click="closeContextMenu">
    <div
      v-for="(account, idx) in accountsStore.accounts"
      :key="account.id"
      class="account-section"
    >
      <button
        class="account-header"
        @click="toggleAccountCollapse(account.id)"
      >
        <span class="account-avatar" :style="{ background: getAvatarColor(idx) }">
          {{ getInitials(account.display_name) }}
        </span>
        <span class="account-info">
          <span class="account-name">{{ account.display_name }}</span>
          <span class="account-email">{{ account.email }}</span>
        </span>
        <svg
          class="chevron"
          :class="{ collapsed: collapsedAccounts.includes(account.id) }"
          width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor"
          stroke-width="2" stroke-linecap="round" stroke-linejoin="round"
        >
          <path d="M6 9l6 6 6-6" />
        </svg>
      </button>

      <div v-if="!collapsedAccounts.includes(account.id)" class="folder-list">
        <button
          v-for="folder in foldersStore.getAccountFolders(account.id)"
          :key="account.id + '/' + folder.path"
          class="folder-item"
          :class="{
            active: accountsStore.activeAccountId === account.id && foldersStore.activeFolderPath === folder.path,
            syncing: syncing === folder.path,
          }"
          @click.stop="selectFolder(account.id, folder.path)"
          @contextmenu="onFolderContextMenu($event, folder)"
        >
          <!-- Folder icons as SVG -->
          <svg v-if="folderIcon(folder) === 'inbox'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="22 12 16 12 14 15 10 15 8 12 2 12" />
            <path d="M5.45 5.11L2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z" />
          </svg>
          <svg v-else-if="folderIcon(folder) === 'sent'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M9 18l6-6-6-6" />
          </svg>
          <svg v-else-if="folderIcon(folder) === 'drafts'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 20h9M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" />
          </svg>
          <svg v-else-if="folderIcon(folder) === 'trash'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
          </svg>
          <svg v-else-if="folderIcon(folder) === 'spam'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="10" /><line x1="4.93" y1="4.93" x2="19.07" y2="19.07" />
          </svg>
          <svg v-else-if="folderIcon(folder) === 'archive'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="21 8 21 21 3 21 3 8" /><rect x="1" y="3" width="22" height="5" /><line x1="10" y1="12" x2="14" y2="12" />
          </svg>
          <svg v-else-if="folderIcon(folder) === 'starred'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2" />
          </svg>
          <svg v-else class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
          </svg>

          <span class="folder-name">{{ folder.name }}</span>
          <span v-if="syncing === folder.path" class="sync-spinner"></span>
          <span v-else-if="folder.unread_count > 0" class="unread-badge">{{ folder.unread_count }}</span>
        </button>
      </div>
    </div>

    <!-- Right-click context menu -->
    <Teleport to="body">
      <div
        v-if="contextMenu"
        class="folder-context-menu"
        :style="{ left: contextMenu.x + 'px', top: contextMenu.y + 'px' }"
      >
        <button class="ctx-item" @click="syncThisFolder">
          Sync "{{ contextMenu.folder.name }}"
        </button>
      </div>
    </Teleport>
  </div>
</template>

<style scoped>
.folder-tree {
  height: 100%;
  display: flex;
  flex-direction: column;
  background: var(--color-bg-secondary);
  border-right: 0.8px solid var(--color-border);
  overflow-y: auto;
}

.account-section {
  border-bottom: 1px solid var(--color-border);
}

.account-header {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 10px 12px;
  text-align: left;
  transition: background 0.12s;
}

.account-header:hover {
  background: var(--color-bg-hover);
}

.account-avatar {
  width: 28px;
  height: 28px;
  border-radius: 50%;
  color: white;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 10px;
  font-weight: 600;
  flex-shrink: 0;
  letter-spacing: 0.5px;
}

.account-info {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
}

.account-name {
  font-size: 13px;
  font-weight: 500;
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.account-email {
  font-size: 11px;
  color: var(--color-text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.chevron {
  flex-shrink: 0;
  color: var(--color-text-muted);
  transition: transform 0.15s;
}

.chevron.collapsed {
  transform: rotate(-90deg);
}

.folder-list {
  padding: 2px 8px 8px;
}

.folder-item {
  display: flex;
  align-items: center;
  width: 100%;
  padding: 5px 8px 5px 12px;
  border-radius: 6px;
  gap: 8px;
  text-align: left;
  font-size: 13px;
  transition: background 0.12s;
}

.folder-item:hover {
  background: var(--color-bg-hover);
}

.folder-item.active {
  background: var(--color-accent-light);
  color: var(--color-accent);
}

.folder-item.active .folder-svg {
  color: var(--color-accent);
}

.folder-item.syncing {
  opacity: 0.6;
}

.folder-svg {
  flex-shrink: 0;
  color: var(--color-text-muted);
}

.folder-name {
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.unread-badge {
  background: var(--color-accent);
  color: white;
  font-size: 10px;
  font-weight: 600;
  padding: 1px 6px;
  border-radius: 10px;
  flex-shrink: 0;
  min-width: 18px;
  text-align: center;
}

.sync-spinner {
  width: 12px;
  height: 12px;
  border: 1.5px solid var(--color-border);
  border-top-color: var(--color-accent);
  border-radius: 50%;
  animation: spin 0.8s linear infinite;
  flex-shrink: 0;
}

@keyframes spin {
  to { transform: rotate(360deg); }
}
</style>

<style>
.folder-context-menu {
  position: fixed;
  z-index: 9999;
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 8px;
  padding: 4px 0;
  min-width: 160px;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.1);
}

.folder-context-menu .ctx-item {
  display: block;
  width: 100%;
  padding: 6px 16px;
  text-align: left;
  font-size: 12px;
  color: var(--color-text);
  background: none;
  border: none;
  cursor: pointer;
}

.folder-context-menu .ctx-item:hover {
  background: var(--color-bg-hover);
}
</style>
