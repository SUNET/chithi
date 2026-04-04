<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import { listen } from "@tauri-apps/api/event";
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

// Double-click opens reader pane (if hidden) or tab
function onOpenMessage(_messageId: string) {
  if (uiStore.messageViewMode === "right") {
    uiStore.showReader();
  }
  // Tab mode: reader always visible as a "tab" area below or could be a separate view
  // For now, just ensure reader is visible
  if (!uiStore.readerVisible) {
    uiStore.showReader();
  }
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

// Periodic sync every 2 minutes
let syncIntervalId: ReturnType<typeof setInterval> | null = null;

async function periodicSync() {
  for (const account of accountsStore.accounts) {
    if (!account.enabled) continue;
    try {
      await api.triggerSync(account.id);
    } catch (e) {
      console.error("Periodic sync error:", e);
    }
  }
}

onMounted(async () => {
  await accountsStore.fetchAccounts();
  if (accountsStore.activeAccountId) {
    await foldersStore.fetchFolders();
    // Backfill thread IDs for existing messages (runs once, fast if already done)
    api.backfillThreads(accountsStore.activeAccountId).catch((e) =>
      console.error("Thread backfill error:", e),
    );
  }

  await listen("sync-complete", async (event) => {
    const payload = event.payload as { account_id: string; total_synced: number };
    console.log("sync-complete:", payload);
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
});

onUnmounted(() => {
  if (syncIntervalId) clearInterval(syncIntervalId);
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

      <!-- Message list pane -->
      <div
        class="message-list-pane"
        :style="{ width: uiStore.readerVisible ? uiStore.messageListWidth + 'px' : undefined }"
        :class="{ expanded: !uiStore.readerVisible }"
      >
        <MessageList @open-message="onOpenMessage" />
      </div>

    <!-- Resize handle between list and reader -->
    <div
      v-if="uiStore.readerVisible"
      class="resize-handle"
      @mousedown="startResize('list', $event)"
    ></div>

      <!-- Reader pane -->
      <div v-if="uiStore.readerVisible" class="reader-pane">
        <MessageReader @close="uiStore.hideReader()" />
      </div>
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
</style>
