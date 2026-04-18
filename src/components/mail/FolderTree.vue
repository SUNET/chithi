<script setup lang="ts">
import { ref, onMounted, watch } from "vue";
import { useFoldersStore } from "@/stores/folders";
import { useAccountsStore } from "@/stores/accounts";
import { useMessagesStore } from "@/stores/messages";
import type { Folder } from "@/lib/types";
import * as api from "@/lib/tauri";
import { dragMessageIds, dragSourceAccountId, isDragging } from "@/lib/drag-state";
import { showToast, dismissToast } from "@/lib/toast";
import { acctColor } from "@/lib/account-colors";
import Select from "@/components/common/Select.vue";

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

function folderIcon(folder: Folder): string {
  switch (folder.folder_type) {
    case "inbox": return "inbox";
    case "sent": return "sent";
    case "drafts": return "drafts";
    case "trash": return "trash";
    case "junk": return "spam";
    case "archive": return "archive";
    case "outbox": return "outbox";
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

async function markAccountRead() {
  const accountId = accountMenu.value?.accountId;
  if (!accountId) return;
  closeContextMenu();

  try {
    await api.markAccountRead(accountId);
    await foldersStore.fetchAllAccountFolders();
    if (accountsStore.activeAccountId === accountId) {
      await messagesStore.fetchMessages();
    }
  } catch (e) {
    console.error("Mark account read failed:", e);
  }
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
      v-for="account in accountsStore.accounts"
      :key="account.id"
      class="account-section"
    >
      <button
        class="account-header"
        :data-testid="`account-${account.id}`"
        @click="toggleAccountCollapse(account.id)"
        @contextmenu="onAccountContextMenu($event, account.id)"
      >
        <span
          class="account-avatar"
          :style="{
            background: acctColor(account.id).soft,
            color: acctColor(account.id).fill,
            boxShadow: 'inset 0 0 0 1.5px ' + acctColor(account.id).fill,
          }"
        >
          {{ account.email.slice(0, 1).toUpperCase() }}
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

          <!-- Folder icons as SVG — filled, per-type color (PATCHES.md §2) -->
          <svg
            class="folder-svg"
            :class="'folder-svg--' + folderIcon(item.folder)"
            width="16" height="16" viewBox="0 0 24 24" aria-hidden="true"
          >
            <template v-if="folderIcon(item.folder) === 'inbox'">
              <path fill="currentColor" d="M3 13h4l1.5 2h7L17 13h4v5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-5Z"/>
              <path fill="currentColor" opacity=".55" d="M7 4h10l3 8h-5l-1.5 2h-3L9 12H4l3-8Z"/>
            </template>
            <template v-else-if="folderIcon(item.folder) === 'sent'">
              <path fill="currentColor" d="M21 3 3 11l7 2 2 7 9-17Z"/>
            </template>
            <template v-else-if="folderIcon(item.folder) === 'drafts'">
              <path fill="currentColor" opacity=".55" d="M5 3h10l4 4v14H5V3Z"/>
              <path fill="currentColor" d="m14 8 3 3-6.5 6.5H8V15l6-7Z"/>
            </template>
            <template v-else-if="folderIcon(item.folder) === 'trash'">
              <path fill="currentColor" d="M6 7h12l-1 13a2 2 0 0 1-2 2H9a2 2 0 0 1-2-2L6 7Z"/>
              <path fill="currentColor" d="M9 4h6l1 3H8l1-3Z"/>
            </template>
            <template v-else-if="folderIcon(item.folder) === 'spam'">
              <path fill="currentColor" d="M12 2 4 5v6c0 5 3.5 9 8 11 4.5-2 8-6 8-11V5l-8-3Z"/>
              <path fill="#fff" d="M11 7h2v6h-2V7Zm0 8h2v2h-2v-2Z"/>
            </template>
            <template v-else-if="folderIcon(item.folder) === 'archive'">
              <path fill="currentColor" opacity=".55" d="M3 4h18v4H3V4Z"/>
              <path fill="currentColor" d="M4 9h16v11H4V9Zm5 3h6v2H9v-2Z"/>
            </template>
            <template v-else-if="folderIcon(item.folder) === 'outbox'">
              <path fill="currentColor" d="M3 13h5l1 2h6l1-2h5v5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-5Z"/>
              <path fill="currentColor" opacity=".55" d="m12 3 5 6h-3v4h-4V9H7l5-6Z"/>
            </template>
            <template v-else-if="folderIcon(item.folder) === 'starred'">
              <path fill="currentColor" d="m12 2 3 7h7l-5.5 4.5L18 21l-6-4-6 4 1.5-7.5L2 9h7l3-7Z"/>
            </template>
            <template v-else>
              <path fill="currentColor" d="M3 6a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6Z"/>
            </template>
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
        <button class="ctx-item" data-testid="ctx-account-mark-read" @click="markAccountRead">Mark All Read</button>
        <button class="ctx-item" data-testid="ctx-account-new-folder" @click="openNewFolderFromAccount">New Folder...</button>
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
              <Select v-model="newFolderParent" :options="buildParentOptions()" class="nf-select" />
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
  /* color + background + boxShadow are set inline per-account via acctColor() */
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 11px;
  font-weight: 700;
  flex-shrink: 0;
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

/* Folder icons keep their type hue at all times — active state is
 * communicated by row background + text weight, not icon color (PATCHES.md §2). */
.folder-item.active .folder-svg { /* intentionally empty */ }

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

.folder-svg { flex-shrink: 0; }

.folder-svg--inbox    { color: #b54708; }
.folder-svg--drafts   { color: #9f5a00; }
.folder-svg--sent     { color: #7a5c0f; }
.folder-svg--spam     { color: #a03912; }
.folder-svg--trash    { color: #8a3a24; }
.folder-svg--archive  { color: #6b4226; }
.folder-svg--outbox   { color: #b8404d; }
.folder-svg--starred  { color: #c2410c; }
.folder-svg--folder   { color: #8a8079; }  /* user folders, neutral */

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
