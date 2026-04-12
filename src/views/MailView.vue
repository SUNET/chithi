<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { listen } from "@tauri-apps/api/event";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useMessagesStore } from "@/stores/messages";
import { useUiStore } from "@/stores/ui";
import * as api from "@/lib/tauri";
import { openReaderWindow } from "@/lib/reader-window";
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

// Bottom mode: reader pane stacked below the list (single message)
const showBottomReader = computed(() =>
  uiStore.messageViewMode === "bottom" && uiStore.readerVisible,
);

// Tab mode state: open message tabs. activeTabId === null means the
// pinned "Messages" list tab.
interface MessageTab {
  messageId: string;
  subject: string;
}
const messageTabs = ref<MessageTab[]>([]);
const activeTabId = ref<string | null>(null);

function activateMessageTab(messageId: string) {
  activeTabId.value = messageId;
  // Avoid re-fetching the body when MessageList has already loaded it
  // (single-click → loadMessage → double-click → activateMessageTab).
  if (messagesStore.activeMessageId !== messageId) {
    messagesStore.loadMessage(messageId);
  }
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

// Guarded helper used by the reader's close event; activeTabId is non-null
// whenever the reader is visible, but vue-tsc can't narrow across template
// branches so we check explicitly.
function closeActiveTab() {
  if (activeTabId.value !== null) {
    closeTab(activeTabId.value);
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

// Double-click behavior depends on view mode:
//  - right:  ensure the inline reader pane is visible
//  - bottom: open the message in a new standalone window
//  - tab:    open the message in a new tab (or focus existing)
function onOpenMessage(messageId: string) {
  if (uiStore.messageViewMode === "right") {
    uiStore.showReader();
    return;
  }
  if (uiStore.messageViewMode === "bottom") {
    const accountId = accountsStore.activeAccountId;
    if (!accountId) return;
    const subject = messagesStore.subjectForMessage(messageId) ?? undefined;
    openReaderWindow(accountId, messageId, subject);
    return;
  }
  // Tab mode
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

      <!-- Bottom mode: message list on top, single-message reader below -->
      <template v-else-if="uiStore.messageViewMode === 'bottom'">
        <div class="stacked-content" data-testid="bottom-mode-content">
          <div class="message-list-pane expanded">
            <MessageList @open-message="onOpenMessage" />
          </div>
          <div v-if="showBottomReader" class="bottom-reader-pane" data-testid="bottom-reader-pane">
            <MessageReader @close="uiStore.hideReader()" />
          </div>
        </div>
      </template>

      <!-- Tab mode: tab bar on top, list or reader content below -->
      <template v-else>
        <div class="stacked-content" data-testid="tab-mode-content">
          <div class="tab-bar" role="tablist" data-testid="tab-bar">
            <div
              class="tab list-tab"
              :class="{ active: activeTabId === null }"
              data-testid="tab-messages"
            >
              <button
                type="button"
                class="tab-activate"
                role="tab"
                :aria-selected="activeTabId === null"
                @click="activateListTab"
                data-testid="tab-messages-btn"
              >
                <span class="tab-label">Messages</span>
              </button>
            </div>
            <div
              v-for="tab in messageTabs"
              :key="tab.messageId"
              class="tab message-tab"
              :class="{ active: activeTabId === tab.messageId }"
              :data-testid="`tab-message-${tab.messageId}`"
            >
              <button
                type="button"
                class="tab-activate"
                role="tab"
                :aria-selected="activeTabId === tab.messageId"
                @click="activateMessageTab(tab.messageId)"
                :data-testid="`tab-activate-${tab.messageId}`"
              >
                <span class="tab-label">{{ tab.subject }}</span>
              </button>
              <button
                type="button"
                class="tab-close"
                :aria-label="`Close tab: ${tab.subject}`"
                @click="closeTab(tab.messageId)"
                :data-testid="`tab-close-${tab.messageId}`"
              >
                &times;
              </button>
            </div>
          </div>
          <div class="tab-content-pane" data-testid="tab-content-pane">
            <MessageList v-if="activeTabId === null" @open-message="onOpenMessage" />
            <MessageReader v-else @close="closeActiveTab" />
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

.tab {
  display: flex;
  align-items: center;
  height: 28px;
  background: transparent;
  border-radius: 4px;
  min-width: 120px;
  max-width: 240px;
}

.tab.list-tab {
  min-width: 100px;
  max-width: 160px;
}

.tab:hover:not(.active) {
  background: var(--color-bg-hover);
}

.tab.active {
  background: var(--color-reader-bg);
}

.tab-activate {
  flex: 1;
  min-width: 0;
  padding: 0 10px;
  font-family: var(--font-sans);
  font-weight: 500;
  font-size: 12px;
  color: var(--color-text-muted);
  background: transparent;
  border: none;
  cursor: pointer;
  text-align: left;
  display: flex;
  align-items: center;
  height: 100%;
}

.tab.active .tab-activate,
.tab:hover .tab-activate {
  color: var(--color-text);
}

.tab-label {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  flex: 1;
  min-width: 0;
}

.tab-close {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 16px;
  height: 16px;
  margin-right: 6px;
  font-size: 14px;
  line-height: 1;
  border: none;
  border-radius: 2px;
  background: transparent;
  color: var(--color-text-muted);
  opacity: 0;
  flex-shrink: 0;
  cursor: pointer;
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

.bottom-reader-pane {
  flex: 1;
  min-height: 200px;
  overflow: auto;
  border-top: 1px solid var(--color-border);
}
</style>
