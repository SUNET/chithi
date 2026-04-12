<script setup lang="ts">
import { ref, onMounted, watch } from "vue";
import { useFoldersStore } from "@/stores/folders";
import { useAccountsStore } from "@/stores/accounts";
import { useMessagesStore } from "@/stores/messages";
import type { Folder } from "@/lib/types";
import * as api from "@/lib/tauri";
import { dragMessageIds, dragSourceAccountId, isDragging } from "@/lib/drag-state";
import { showToast, dismissToast } from "@/lib/toast";

const foldersStore = useFoldersStore();
const accountsStore = useAccountsStore();
const messagesStore = useMessagesStore();

const contextMenu = ref<{ x: number; y: number; folder: Folder; accountId: string } | null>(null);
const accountMenu = ref<{ x: number; y: number; accountId: string } | null>(null);
const syncing = ref<string | null>(null);
const collapsedAccounts = ref<string[]>([]);
const dropTarget = ref<string | null>(null);

// Folder expand/collapse state, persisted per account in localStorage
const collapsedFolders = ref<Record<string, string[]>>(loadCollapsedFolders());

function loadCollapsedFolders(): Record<string, string[]> {
  try {
    const stored = localStorage.getItem("chithi-collapsed-folders");
    if (!stored) return {};
    const parsed: unknown = JSON.parse(stored);
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return {};
    // Validate all values are string arrays
    for (const v of Object.values(parsed as Record<string, unknown>)) {
      if (!Array.isArray(v) || !v.every((item) => typeof item === "string")) return {};
    }
    return parsed as Record<string, string[]>;
  } catch {
    return {};
  }
}

function saveCollapsedFolders() {
  localStorage.setItem("chithi-collapsed-folders", JSON.stringify(collapsedFolders.value));
}

function isFolderCollapsed(accountId: string, folderPath: string): boolean {
  return collapsedFolders.value[accountId]?.includes(folderPath) ?? false;
}

function toggleFolderCollapse(accountId: string, folderPath: string) {
  const current = collapsedFolders.value[accountId] ?? [];
  if (current.includes(folderPath)) {
    collapsedFolders.value = {
      ...collapsedFolders.value,
      [accountId]: current.filter(p => p !== folderPath),
    };
  } else {
    collapsedFolders.value = {
      ...collapsedFolders.value,
      [accountId]: [...current, folderPath],
    };
  }
  saveCollapsedFolders();
}

interface FlatFolder {
  folder: Folder;
  depth: number;
  hasChildren: boolean;
}

function flattenTree(folders: Folder[], depth: number, accountId: string): FlatFolder[] {
  const result: FlatFolder[] = [];
  for (const folder of folders) {
    result.push({ folder, depth, hasChildren: folder.children.length > 0 });
    if (folder.children.length > 0 && !isFolderCollapsed(accountId, folder.path)) {
      result.push(...flattenTree(folder.children, depth + 1, accountId));
    }
  }
  return result;
}

const showNewFolderModal = ref(false);
const newFolderName = ref("");
const newFolderParent = ref("");
const newFolderSaving = ref(false);
const newFolderError = ref<string | null>(null);
const deletingFolder = ref<{ accountId: string; folder: Folder } | null>(null);

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

function onFolderContextMenu(event: MouseEvent, folder: Folder, accountId: string) {
  event.preventDefault();
  contextMenu.value = { x: event.clientX, y: event.clientY, folder, accountId };
}

function closeContextMenu() {
  contextMenu.value = null;
  accountMenu.value = null;
}

function onAccountContextMenu(event: MouseEvent, accountId: string) {
  event.preventDefault();
  contextMenu.value = null;
  accountMenu.value = { x: event.clientX, y: event.clientY, accountId };
}

function openNewFolderFromAccount() {
  const accountId = accountMenu.value?.accountId;
  newFolderParent.value = accountId ? `${accountId}|` : "";
  newFolderName.value = "";
  newFolderError.value = null;
  closeContextMenu();
  showNewFolderModal.value = true;
}

async function syncThisFolder() {
  if (!contextMenu.value) return;
  const { folder, accountId } = contextMenu.value;
  closeContextMenu();

  syncing.value = folder.path;
  try {
    await api.syncFolder(accountId, folder.path);
  } catch (e) {
    console.error("Folder sync failed:", e);
  } finally {
    syncing.value = null;
  }
}


function openNewFolderModal() {
  if (!contextMenu.value) return;
  const { folder, accountId } = contextMenu.value;
  // Default parent to the right-clicked folder's path
  newFolderParent.value = `${accountId}|${folder.path}`;
  newFolderName.value = "";
  newFolderError.value = null;
  closeContextMenu();
  showNewFolderModal.value = true;
}

function buildParentOptions(): { label: string; value: string }[] {
  const options: { label: string; value: string }[] = [];

  function addFolders(folders: Folder[], accountId: string, accountEmail: string, prefix: string) {
    for (const folder of folders) {
      const label = prefix ? `${prefix}/${folder.name}` : folder.name;
      options.push({ label: `${label} on ${accountEmail}`, value: `${accountId}|${folder.path}` });
      addFolders(folder.children, accountId, accountEmail, label);
    }
  }

  for (const acc of accountsStore.accounts) {
    options.push({ label: `${acc.display_name} (root)`, value: `${acc.id}|` });
    addFolders(foldersStore.getAccountFolders(acc.id), acc.id, acc.email, "");
  }
  return options;
}

async function createNewFolder() {
  if (!newFolderName.value.trim()) {
    newFolderError.value = "Folder name is required";
    return;
  }
  const [accountId, parentPath] = newFolderParent.value.split("|", 2);
  if (!accountId) {
    newFolderError.value = "Select a location";
    return;
  }
  // Build full path: parent/name or just name if root
  const folderPath = parentPath
    ? `${parentPath}/${newFolderName.value.trim()}`
    : newFolderName.value.trim();

  newFolderSaving.value = true;
  newFolderError.value = null;
  const createToastId = showToast(`Creating folder...`, "info", 0);
  try {
    await api.createFolder(accountId, folderPath);
    showNewFolderModal.value = false;
    await api.triggerSync(accountId);
    await foldersStore.fetchAllAccountFolders();
  } catch (e) {
    newFolderError.value = String(e);
  } finally {
    newFolderSaving.value = false;
    dismissToast(createToastId);
  }
}

let dragExpandTimer: ReturnType<typeof setTimeout> | null = null;

function onFolderMouseEnter(accountId: string, folderPath: string) {
  if (!isDragging.value) return;
  // Allow drops on any account (cross-account moves are supported)
  if (dragSourceAccountId.value === accountId &&
      foldersStore.activeFolderPath === folderPath &&
      accountsStore.activeAccountId === accountId) return;
  dropTarget.value = `${accountId}:${folderPath}`;
  if (isFolderCollapsed(accountId, folderPath)) {
    dragExpandTimer = setTimeout(() => {
      toggleFolderCollapse(accountId, folderPath);
    }, 600);
  }
}

function onFolderMouseLeave(accountId: string, folderPath: string) {
  if (dropTarget.value === `${accountId}:${folderPath}`) {
    dropTarget.value = null;
  }
  if (dragExpandTimer) {
    clearTimeout(dragExpandTimer);
    dragExpandTimer = null;
  }
}

async function onFolderMouseUp(accountId: string, folderPath: string) {
  if (!isDragging.value || dragMessageIds.value.length === 0) return;
  dropTarget.value = null;
  const sourceAccountId = dragSourceAccountId.value;
  if (!sourceAccountId) return;
  if (sourceAccountId === accountId &&
      foldersStore.activeFolderPath === folderPath &&
      accountsStore.activeAccountId === accountId) return;

  const messageIds = [...dragMessageIds.value];
  const isCrossAccount = sourceAccountId !== accountId;
  const label = isCrossAccount ? "Moving (cross-account)" : "Moving";
  const moveToastId = showToast(`${label} ${messageIds.length} message(s)...`, "info", 0);
  try {
    if (isCrossAccount) {
      await api.moveMessagesCrossAccount(sourceAccountId, messageIds, accountId, folderPath);
      // Sync the destination account so the moved messages appear immediately
      api.triggerSync(accountId, folderPath).catch((e) =>
        console.error("Post-move sync failed:", e),
      );
    } else {
      await api.moveMessages(accountId, messageIds, folderPath);
    }
    messagesStore.clearSelection();
    messagesStore.activeMessage = null;
    messagesStore.activeMessageId = null;
  } catch (e) {
    console.error("Drag-and-drop move failed:", e);
    const message = e instanceof Error ? e.message : String(e);
    showToast(`Move failed: ${message}`, "error", 5000);
  } finally {
    dismissToast(moveToastId);
  }
}

async function markFolderRead() {
  if (!contextMenu.value) return;
  const { folder, accountId } = contextMenu.value;
  closeContextMenu();

  try {
    // Get all messages in the folder (large page to capture all)
    const result = await api.getMessages(accountId, folder.path, 0, 10000, "date", false);
    const unreadIds = result.messages
      .filter((m: { flags: string[] }) => !m.flags.includes("seen"))
      .map((m: { id: string }) => m.id);

    if (unreadIds.length > 0) {
      await api.setMessageFlags(accountId, unreadIds, ["seen"], true);
    }
  } catch (e) {
    console.error("Mark folder read failed:", e);
  }
}

function confirmDeleteFolder() {
  if (!contextMenu.value) return;
  const { folder, accountId } = contextMenu.value;
  closeContextMenu();
  deletingFolder.value = { accountId, folder };
}

async function doDeleteFolder() {
  if (!deletingFolder.value) return;
  const { accountId, folder } = deletingFolder.value;
  deletingFolder.value = null;

  const deleteToastId = showToast(`Deleting "${folder.name}"...`, "info", 0);
  try {
    await api.deleteFolder(accountId, folder.path);
    await foldersStore.fetchAllAccountFolders();
    if (
      accountsStore.activeAccountId === accountId &&
      foldersStore.activeFolderPath === folder.path
    ) {
      const folders = foldersStore.getAccountFolders(accountId);
      const inbox = folders.find((f: Folder) => f.folder_type === "inbox");
      foldersStore.setActiveFolder(inbox?.path ?? (folders[0]?.path ?? ""));
    }
    showToast(`Deleted "${folder.name}"`, "success");
  } catch (e) {
    console.error("Delete folder failed:", e);
    const message = e instanceof Error ? e.message : String(e);
    showToast(`Failed to delete "${folder.name}": ${message}`, "error", 5000);
  } finally {
    dismissToast(deleteToastId);
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
        :data-testid="`account-${account.id}`"
        @click="toggleAccountCollapse(account.id)"
        @contextmenu="onAccountContextMenu($event, account.id)"
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
          v-for="item in flattenTree(foldersStore.getAccountFolders(account.id), 0, account.id)"
          :key="account.id + '/' + item.folder.path"
          class="folder-item"
          :class="{
            active: accountsStore.activeAccountId === account.id && foldersStore.activeFolderPath === item.folder.path,
            syncing: syncing === item.folder.path,
            'drop-target': dropTarget === `${account.id}:${item.folder.path}`,
          }"
          :data-testid="`folder-${account.id}-${item.folder.path}`"
          :style="{ paddingLeft: (12 + item.depth * 16) + 'px' }"
          @click.stop="selectFolder(account.id, item.folder.path)"
          @contextmenu="onFolderContextMenu($event, item.folder, account.id)"
          @mouseenter="onFolderMouseEnter(account.id, item.folder.path)"
          @mouseleave="onFolderMouseLeave(account.id, item.folder.path)"
          @mouseup="onFolderMouseUp(account.id, item.folder.path)"
        >
          <span
            v-if="item.hasChildren"
            class="folder-toggle"
            role="button"
            tabindex="0"
            :aria-expanded="!isFolderCollapsed(account.id, item.folder.path)"
            @click.stop="toggleFolderCollapse(account.id, item.folder.path)"
            @keydown.enter.stop="toggleFolderCollapse(account.id, item.folder.path)"
            @keydown.space.stop.prevent="toggleFolderCollapse(account.id, item.folder.path)"
          >
            <svg
              width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor"
              stroke-width="2" stroke-linecap="round" stroke-linejoin="round"
              :style="{ transform: isFolderCollapsed(account.id, item.folder.path) ? 'rotate(-90deg)' : '', transition: 'transform 0.15s' }"
            >
              <path d="M6 9l6 6 6-6" />
            </svg>
          </span>
          <span v-else class="folder-toggle-spacer"></span>

          <!-- Folder icons as SVG -->
          <svg v-if="folderIcon(item.folder) === 'inbox'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="22 12 16 12 14 15 10 15 8 12 2 12" />
            <path d="M5.45 5.11L2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z" />
          </svg>
          <svg v-else-if="folderIcon(item.folder) === 'sent'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M9 18l6-6-6-6" />
          </svg>
          <svg v-else-if="folderIcon(item.folder) === 'drafts'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 20h9M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" />
          </svg>
          <svg v-else-if="folderIcon(item.folder) === 'trash'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="3 6 5 6 21 6" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
          </svg>
          <svg v-else-if="folderIcon(item.folder) === 'spam'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="10" /><line x1="4.93" y1="4.93" x2="19.07" y2="19.07" />
          </svg>
          <svg v-else-if="folderIcon(item.folder) === 'archive'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="21 8 21 21 3 21 3 8" /><rect x="1" y="3" width="22" height="5" /><line x1="10" y1="12" x2="14" y2="12" />
          </svg>
          <svg v-else-if="folderIcon(item.folder) === 'starred'" class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2" />
          </svg>
          <svg v-else class="folder-svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
          </svg>

          <span class="folder-name">{{ item.folder.name }}</span>
          <span v-if="syncing === item.folder.path" class="sync-spinner"></span>
          <span v-else-if="item.folder.unread_count > 0" class="unread-badge">{{ item.folder.unread_count }}</span>
        </button>
      </div>
    </div>

    <!-- Right-click context menu -->
    <Teleport to="body">
      <div v-if="contextMenu" class="folder-menu-overlay" @click="closeContextMenu"></div>
      <div
        v-if="contextMenu"
        class="folder-context-menu"
        data-testid="folder-context-menu"
        :style="{ left: contextMenu.x + 'px', top: contextMenu.y + 'px' }"
      >
        <button class="ctx-item disabled">Open in New Tab</button>
        <button class="ctx-item disabled">Open in New Window</button>
        <button class="ctx-item disabled">Search Messages...</button>
        <div class="ctx-separator"></div>
        <button class="ctx-item" data-testid="ctx-new-folder" @click="openNewFolderModal">New Folder...</button>
        <div class="ctx-separator"></div>
        <button class="ctx-item" data-testid="ctx-mark-read" @click="markFolderRead">Mark Folder Read</button>
        <div class="ctx-separator"></div>
        <button class="ctx-item danger" data-testid="ctx-delete-folder" @click="confirmDeleteFolder">Delete Folder</button>
        <div class="ctx-separator"></div>
        <button class="ctx-item disabled">Properties</button>
        <button class="ctx-item" data-testid="ctx-sync-folder" @click="syncThisFolder">
          Sync "{{ contextMenu.folder.name }}"
        </button>
      </div>
    </Teleport>
    <!-- Account right-click context menu -->
    <Teleport to="body">
      <div v-if="accountMenu" class="folder-menu-overlay" @click="closeContextMenu"></div>
      <div
        v-if="accountMenu"
        class="folder-context-menu"
        :style="{ left: accountMenu.x + 'px', top: accountMenu.y + 'px' }"
      >
        <button class="ctx-item" @click="openNewFolderFromAccount">New Folder...</button>
      </div>
    </Teleport>

    <!-- New Folder Modal -->
    <Teleport to="body">
      <div v-if="showNewFolderModal" class="modal-overlay" @click.self="showNewFolderModal = false">
        <div class="new-folder-modal">
          <div class="nf-header">
            <h3>New Folder</h3>
            <button class="nf-close" @click="showNewFolderModal = false">&times;</button>
          </div>
          <div class="nf-body">
            <div v-if="newFolderError" class="nf-error">{{ newFolderError }}</div>
            <div class="nf-field">
              <label>Name:</label>
              <input
                v-model="newFolderName"
                type="text"
                class="nf-input"
                placeholder="Folder name"
                @keydown.enter="createNewFolder"
              />
            </div>
            <div class="nf-field">
              <label>Create as a subfolder of:</label>
              <select v-model="newFolderParent" class="nf-select">
                <option v-for="opt in buildParentOptions()" :key="opt.value" :value="opt.value">
                  {{ opt.label }}
                </option>
              </select>
            </div>
          </div>
          <div class="nf-footer">
            <button class="nf-btn-cancel" @click="showNewFolderModal = false">Cancel</button>
            <button class="nf-btn-create" :disabled="newFolderSaving" @click="createNewFolder">
              {{ newFolderSaving ? "Creating..." : "Create Folder" }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>

    <!-- Delete Folder Confirmation -->
    <Teleport to="body">
      <div v-if="deletingFolder" class="modal-overlay" @click.self="deletingFolder = null">
        <div class="new-folder-modal">
          <div class="nf-body" style="padding: 20px">
            <h3 style="margin: 0 0 8px">Delete Folder</h3>
            <p style="font-size: 13px; color: var(--color-text-secondary); line-height: 1.5; margin: 0 0 4px">
              Are you sure you want to delete "{{ deletingFolder.folder.name }}"?
            </p>
            <p style="font-size: 12px; color: var(--color-text-muted); margin: 0">
              All messages in this folder will be permanently deleted.
            </p>
          </div>
          <div class="nf-footer">
            <button class="nf-btn-cancel" @click="deletingFolder = null">Cancel</button>
            <button class="nf-btn-create" style="background: var(--color-danger, #dc2626)" @click="doDeleteFolder">Delete</button>
          </div>
        </div>
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

.folder-item.drop-target {
  background: var(--color-accent-light);
  outline: 1.5px solid var(--color-accent);
  outline-offset: -1.5px;
}

.folder-toggle {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 16px;
  height: 16px;
  flex-shrink: 0;
  color: var(--color-text-muted);
  cursor: pointer;
  border-radius: 3px;
}

.folder-toggle:hover {
  background: var(--color-bg-hover);
}

.folder-toggle-spacer {
  width: 16px;
  flex-shrink: 0;
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
.folder-menu-overlay {
  position: fixed;
  inset: 0;
  z-index: 9998;
}

.folder-context-menu {
  position: fixed;
  z-index: 9999;
  background: var(--color-bg);
  border: 0.8px solid var(--color-border);
  border-radius: 8px;
  padding: 4px 0;
  min-width: 200px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.12);
}

.folder-context-menu .ctx-item {
  display: block;
  width: 100%;
  padding: 7px 16px;
  text-align: left;
  font-size: 13px;
  color: var(--color-text);
  background: none;
  border: none;
  cursor: pointer;
}

.folder-context-menu .ctx-item:hover:not(.disabled) {
  background: var(--color-bg-hover);
}

.folder-context-menu .ctx-item.disabled {
  opacity: 0.4;
  cursor: default;
}

.folder-context-menu .ctx-item.danger {
  color: var(--color-danger, #dc2626);
}

.folder-context-menu .ctx-item.danger:hover {
  background: rgba(220, 53, 69, 0.08);
}

.folder-context-menu .ctx-separator {
  height: 1px;
  background: var(--color-border);
  margin: 4px 0;
}

.modal-overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.3);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 10000;
}

.new-folder-modal {
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 10px;
  box-shadow: 0 8px 32px rgba(0, 0, 0, 0.15);
  width: 380px;
}

.nf-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 16px;
  border-bottom: 1px solid var(--color-border);
}

.nf-header h3 {
  margin: 0;
  font-size: 15px;
}

.nf-close {
  font-size: 20px;
  background: none;
  border: none;
  color: var(--color-text-muted);
  cursor: pointer;
}

.nf-body {
  padding: 16px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}

.nf-error {
  color: var(--color-danger-text, #dc2626);
  font-size: 12px;
  padding: 6px 8px;
  background: rgba(251, 44, 54, 0.06);
  border-radius: 4px;
}

.nf-field {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.nf-field label {
  font-size: 13px;
  font-weight: 500;
  color: var(--color-text);
}

.nf-input,
.nf-select {
  width: 100%;
  box-sizing: border-box;
  height: 36px;
  padding: 0 10px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  background: var(--color-bg);
  color: var(--color-text);
  font-size: 14px;
}

.nf-input:focus,
.nf-select:focus {
  outline: none;
  border-color: var(--color-accent);
}

.nf-footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 12px 16px;
  border-top: 1px solid var(--color-border);
}

.nf-btn-cancel {
  height: 34px;
  padding: 0 16px;
  background: var(--color-bg-hover);
  color: var(--color-text);
  border: none;
  border-radius: 6px;
  font-size: 13px;
  cursor: pointer;
}

.nf-btn-create {
  height: 34px;
  padding: 0 16px;
  background: var(--color-accent);
  color: white;
  border: none;
  border-radius: 6px;
  font-size: 13px;
  font-weight: 500;
  cursor: pointer;
}

.nf-btn-create:hover {
  background: var(--color-accent-hover);
}

.nf-btn-create:disabled {
  opacity: 0.5;
  cursor: default;
}
</style>
