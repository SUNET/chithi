<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { listen } from "@tauri-apps/api/event";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useMessagesStore } from "@/stores/messages";
import { useUiStore } from "@/stores/ui";
import * as api from "@/lib/tauri";
import Toolbar from "@/components/mail/Toolbar.vue";
import FolderTree from "@/components/mail/FolderTree.vue";
import MessageList from "@/components/mail/MessageList.vue";
import MessageReader from "@/components/mail/MessageReader.vue";

const accountsStore = useAccountsStore();
const foldersStore = useFoldersStore();
const messagesStore = useMessagesStore();
const uiStore = useUiStore();

// Right mode: inline reader pane next to the message list
const showRightReader = computed(() =>
  uiStore.messageViewMode === "right" && uiStore.readerVisible,
);

// Bottom mode: show tab bar + reader under the list once a tab is opened
const showBottomReader = computed(() =>
  uiStore.messageViewMode === "bottom" && messageTabs.value.length > 0,
);

// Shared state for bottom and tab modes: open message tabs and the active one.
// activeTabId === null means the "Messages" list tab (used in tab mode).
interface MessageTab {
  messageId: string;
  subject: string;
}
const messageTabs = ref<MessageTab[]>([]);
const activeTabId = ref<string | null>(null);

function activateMessageTab(messageId: string) {
  activeTabId.value = messageId;
  messagesStore.loadMessage(messageId);
}

function activateListTab() {
  activeTabId.value = null;
}

function closeTab(messageId: string) {
  const idx = messageTabs.value.findIndex((t) => t.messageId === messageId);
  if (idx === -1) return;
  messageTabs.value.splice(idx, 1);
  if (activeTabId.value === messageId) {
    if (messageTabs.value.length > 0) {
      const next = messageTabs.value[Math.max(0, idx - 1)];
      activateMessageTab(next.messageId);
    } else {
      activeTabId.value = null;
    }
  }
}

// Resizing state
const resizingPane = ref<"folder" | "list" | null>(null);
const startX = ref(0);
const startWidth = ref(0);

function startResize(pane: "folder" | "list", event: MouseEvent) {
  resizingPane.value = pane;
  startX.value = event.clientX;
  startWidth.value =
    pane === "folder"
      ? uiStore.folderPaneWidth
      : uiStore.messageListWidth;
  document.addEventListener("mousemove", onResize);
  document.addEventListener("mouseup", stopResize);
  document.body.style.cursor = "col-resize";
  document.body.style.userSelect = "none";
}

function onResize(event: MouseEvent) {
  const delta = event.clientX - startX.value;
  const newWidth = Math.max(120, Math.min(600, startWidth.value + delta));
  if (resizingPane.value === "folder") {
    uiStore.folderPaneWidth = newWidth;
  } else if (resizingPane.value === "list") {
    uiStore.messageListWidth = newWidth;
  }
}

function stopResize() {
  resizingPane.value = null;
  document.removeEventListener("mousemove", onResize);
  document.removeEventListener("mouseup", stopResize);
  document.body.style.cursor = "";
  document.body.style.userSelect = "";
}

// Double-click opens the message — inline reader in right mode,
// or a new tab in bottom/tab modes.
function onOpenMessage(messageId: string) {
  if (uiStore.messageViewMode === "right") {
    uiStore.showReader();
    return;
  }
  const existing = messageTabs.value.find((t) => t.messageId === messageId);
  if (!existing) {
    messageTabs.value.push({
      messageId,
      subject: messagesStore.subjectForMessage(messageId) ?? "(no subject)",
    });
  }
  activateMessageTab(messageId);
}

// Background prefetch after sync
async function startBackgroundPrefetch() {
  for (const account of accountsStore.accounts) {
    if (!account.enabled) continue;
    try {
      let fetched = 1;
      while (fetched > 0) {
        fetched = await api.prefetchBodies(account.id);
      }
    } catch (e) {
      console.error("Background prefetch error:", e);
    }
  }
}

// Desktop notifications for new mail
let notificationsAllowed = false;

async function initNotifications() {
  let granted = await isPermissionGranted();
  if (!granted) {
    const permission = await requestPermission();
    granted = permission === "granted";
  }
  notificationsAllowed = granted;
}

function notifyNewMail(accountId: string, count: number) {
  if (!notificationsAllowed || count === 0) return;
  // Don't notify if the window is focused — the user is already looking at it
  if (document.hasFocus()) return;

  const account = accountsStore.accounts.find(a => a.id === accountId);
  const accountName = account?.display_name || account?.email || "Account";
  const body = count === 1
    ? `1 new message in ${accountName}`
    : `${count} new messages in ${accountName}`;

  sendNotification({ title: "Chithi", body });
}

// Periodic sync every 2 minutes
let syncIntervalId: ReturnType<typeof setInterval> | null = null;

async function periodicSync() {
  for (const account of accountsStore.accounts) {
    if (!account.enabled) continue;
    try {
      await api.triggerSync(
        account.id,
        foldersStore.activeFolderPath ?? undefined,
      );
    } catch (e) {
      console.error("Periodic sync error:", e);
    }
  }
}

onMounted(async () => {
  await accountsStore.fetchAccounts();
  if (accountsStore.activeAccountId) {
    await foldersStore.fetchFolders();
  }

  initNotifications();

  await listen("sync-complete", async (event) => {
    const payload = event.payload as { account_id: string; total_synced: number };
    console.log("sync-complete:", payload);
    notifyNewMail(payload.account_id, payload.total_synced);
    // Always refresh folders to update counts
    await foldersStore.fetchFolders();
    // Refresh message list if viewing the synced account
    if (
      accountsStore.activeAccountId === payload.account_id &&
      foldersStore.activeFolderPath
    ) {
      await messagesStore.fetchMessages();
    }
    startBackgroundPrefetch();
    // Sync calendars in background after mail sync (keeps O365/Google events up to date)
    api.syncCalendars(payload.account_id).catch(() => {});
  });

  let lastRefresh = 0;
  await listen("sync-progress", async (event) => {
    const now = Date.now();
    const payload = event.payload as {
      account_id: string;
      synced: number;
      folder: string;
    };
    // Refresh when new messages synced in the folder we're viewing
    if (
      payload.synced > 0 &&
      now - lastRefresh > 2000 &&
      accountsStore.activeAccountId === payload.account_id
    ) {
      lastRefresh = now;
      await foldersStore.fetchFolders();
      if (foldersStore.activeFolderPath) {
        await messagesStore.fetchMessages();
      }
    }
  });

  // Start periodic sync every 2 minutes
  syncIntervalId = setInterval(periodicSync, 2 * 60 * 1000);

  // Start IMAP IDLE for push notifications
  api.startIdle().catch((e) => console.error("Failed to start IDLE:", e));

  // Listen for IDLE events
  await listen("idle-new-mail", async (event) => {
    const accountId = event.payload as string;
    console.log("IDLE new mail for account", accountId);
    try {
      await api.triggerSync(accountId, foldersStore.activeFolderPath ?? undefined);
    } catch (e) {
      console.error("IDLE sync trigger failed:", e);
    }
  });

  await listen("idle-disconnected", (event) => {
    console.warn("IDLE disconnected for account", event.payload);
  });

  await listen("idle-reconnected", (event) => {
    console.log("IDLE reconnected for account", event.payload);
  });
});

onUnmounted(() => {
  if (syncIntervalId) clearInterval(syncIntervalId);
  api.stopIdle().catch(() => {});
});
</script>

<template>
  <div class="mail-view">
    <Toolbar />
    <div class="mail-panes">
      <!-- Folder pane -->
      <div class="folder-pane" :style="{ width: uiStore.folderPaneWidth + 'px' }">
        <FolderTree />
      </div>
      <div class="resize-handle" @mousedown="startResize('folder', $event)"></div>

      <!-- Right mode: message list + reader side by side -->
      <template v-if="uiStore.messageViewMode === 'right'">
        <div
          class="message-list-pane"
          :style="{ width: showRightReader ? uiStore.messageListWidth + 'px' : undefined }"
          :class="{ expanded: !showRightReader }"
        >
          <MessageList @open-message="onOpenMessage" />
        </div>
        <div
          v-if="showRightReader"
          class="resize-handle"
          @mousedown="startResize('list', $event)"
        ></div>
        <div v-if="showRightReader" class="reader-pane">
          <MessageReader @close="uiStore.hideReader()" />
        </div>
      </template>

      <!-- Bottom mode: list on top, tab bar + reader below when any tabs open -->
      <template v-else-if="uiStore.messageViewMode === 'bottom'">
        <div class="stacked-content">
          <div class="message-list-pane expanded">
            <MessageList @open-message="onOpenMessage" />
          </div>
          <template v-if="showBottomReader">
            <div class="tab-bar">
              <button
                v-for="tab in messageTabs"
                :key="tab.messageId"
                class="tab"
                :class="{ active: activeTabId === tab.messageId }"
                @click="activateMessageTab(tab.messageId)"
              >
                <span class="tab-label">{{ tab.subject }}</span>
                <span class="tab-close" @click.stop="closeTab(tab.messageId)">&times;</span>
              </button>
            </div>
            <div class="tab-reader-pane">
              <MessageReader @close="closeTab(activeTabId!)" />
            </div>
          </template>
        </div>
      </template>

      <!-- Tab mode: tab bar on top, list or reader content below -->
      <template v-else>
        <div class="stacked-content">
          <div class="tab-bar">
            <button
              class="tab list-tab"
              :class="{ active: activeTabId === null }"
              @click="activateListTab"
            >
              <span class="tab-label">Messages</span>
            </button>
            <button
              v-for="tab in messageTabs"
              :key="tab.messageId"
              class="tab"
              :class="{ active: activeTabId === tab.messageId }"
              @click="activateMessageTab(tab.messageId)"
            >
              <span class="tab-label">{{ tab.subject }}</span>
              <span class="tab-close" @click.stop="closeTab(tab.messageId)">&times;</span>
            </button>
          </div>
          <div class="tab-content-pane">
            <MessageList v-if="activeTabId === null" @open-message="onOpenMessage" />
            <MessageReader v-else @close="closeTab(activeTabId)" />
          </div>
        </div>
      </template>
    </div>
  </div>
</template>

<style scoped>
.mail-view {
  display: flex;
  flex-direction: column;
  height: 100%;
  width: 100%;
}

.mail-panes {
  display: flex;
  flex: 1;
  min-height: 0;
}

.folder-pane {
  flex-shrink: 0;
  min-width: 120px;
}

.message-list-pane {
  flex-shrink: 0;
  min-width: 200px;
}

.message-list-pane.expanded {
  flex: 1;
  width: auto !important;
}

.reader-pane {
  flex: 1;
  min-width: 200px;
}

.resize-handle {
  width: 4px;
  cursor: col-resize;
  background: transparent;
  flex-shrink: 0;
  transition: background 0.15s;
}

.resize-handle:hover {
  background: var(--color-accent);
}

.stacked-content {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
}

.stacked-content .message-list-pane {
  flex: 1;
  min-height: 150px;
}

.tab-content-pane {
  flex: 1;
  min-height: 0;
  overflow: hidden;
  display: flex;
}

.tab-content-pane > * {
  flex: 1;
  min-height: 0;
  overflow: auto;
}

.tab-bar {
  display: flex;
  align-items: center;
  background: var(--color-bg-secondary);
  border-bottom: 1px solid var(--color-border);
  padding: 2px 4px;
  height: 36px;
  flex-shrink: 0;
  overflow-x: auto;
  gap: 2px;
}

/* In bottom mode the tab bar appears between the list and the reader,
   so it needs a top border to visually separate it from the list above. */
.stacked-content > .message-list-pane + .tab-bar {
  border-top: 1px solid var(--color-border);
}

.tab {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 28px;
  padding: 0 10px;
  font-family: var(--font-sans);
  font-weight: 500;
  font-size: 12px;
  color: var(--color-text-muted);
  background: transparent;
  border-radius: 4px;
  min-width: 120px;
  max-width: 240px;
  white-space: nowrap;
  cursor: pointer;
}

.tab.list-tab {
  min-width: 100px;
  max-width: 160px;
}

.tab:hover:not(.active) {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.tab.active {
  background: var(--color-reader-bg);
  color: var(--color-text);
}

.tab-label {
  overflow: hidden;
  text-overflow: ellipsis;
  flex: 1;
}

.tab-close {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 16px;
  height: 16px;
  font-size: 14px;
  line-height: 1;
  border-radius: 2px;
  color: var(--color-text-muted);
  opacity: 0;
  flex-shrink: 0;
  transition: opacity 0.1s, background 0.1s;
}

.tab:hover .tab-close,
.tab.active .tab-close {
  opacity: 1;
}

.tab-close:hover {
  background: var(--color-bg-tertiary);
  color: var(--color-text);
}

.tab-reader-pane {
  flex: 1;
  min-height: 200px;
  overflow: auto;
}
</style>
