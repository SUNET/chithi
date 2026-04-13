<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from "vue";
import { useRoute } from "vue-router";
import { listen } from "@tauri-apps/api/event";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { useActivityStore } from "@/stores/activity";
import { useOpsStore } from "@/stores/ops";
import { useUiStore } from "@/stores/ui";
import { useAccountsStore } from "@/stores/accounts";
import { useFoldersStore } from "@/stores/folders";
import { useCalendarStore } from "@/stores/calendar";
import * as api from "@/lib/tauri";

const route = useRoute();
const activityStore = useActivityStore();
const opsStore = useOpsStore();
const uiStore = useUiStore();
const accountsStore = useAccountsStore();
const foldersStore = useFoldersStore();
const calendarStore = useCalendarStore();
const lastSyncTime = ref<Date | null>(null);
const connectionStatus = ref<"connected" | "disconnected" | "reconnecting">("connected");
const syncError = ref<string | null>(null);

const unlistenFns: UnlistenFn[] = [];

onMounted(async () => {
  await opsStore.initEventListeners();

  unlistenFns.push(
    await listen("idle-disconnected", () => {
      connectionStatus.value = "disconnected";
    }),
  );
  unlistenFns.push(
    await listen("idle-reconnected", () => {
      connectionStatus.value = "connected";
      syncError.value = null;
    }),
  );
  unlistenFns.push(
    await listen<{ error: string }>("sync-error", (event) => {
      connectionStatus.value = "disconnected";
      syncError.value = event.payload.error;
    }),
  );
  unlistenFns.push(
    await listen("sync-complete", () => {
      connectionStatus.value = "connected";
      syncError.value = null;
    }),
  );
});

onUnmounted(() => {
  for (const unlisten of unlistenFns) {
    unlisten();
  }
});

const lastSyncLabel = computed(() => {
  if (!lastSyncTime.value) return "";
  const diff = Math.floor((Date.now() - lastSyncTime.value.getTime()) / 1000);
  if (diff < 60) return "Last sync just now";
  if (diff < 3600) return `Last sync ${Math.floor(diff / 60)} minutes ago`;
  return `Last sync ${Math.floor(diff / 3600)} hours ago`;
});

async function syncAll() {
  const isCalendar = route.name === "calendar";
  const isContacts = route.name === "contacts";

  for (const account of accountsStore.accounts) {
    if (!account.enabled) continue;
    try {
      if (isCalendar) {
        await api.syncCalendars(account.id, true);
      } else if (isContacts) {
        await api.syncContacts(account.id);
      } else {
        await api.triggerSync(
          account.id,
          foldersStore.activeFolderPath ?? undefined,
        );
      }
    } catch (e) {
      console.error("Sync failed:", e);
    }
  }

  if (isCalendar) {
    await calendarStore.fetchCalendars();
    await calendarStore.fetchEvents();
  }

  lastSyncTime.value = new Date();
}
</script>

<template>
  <div class="status-bar">
    <div class="status-left">
      <button class="sync-btn" title="Sync" @click="syncAll">
        <span class="sync-icon" :class="{ spinning: activityStore.hasActiveOperations }">
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M21 2v6h-6M3 12a9 9 0 0 1 15-6.7L21 8M3 22v-6h6M21 12a9 9 0 0 1-15 6.7L3 16" />
          </svg>
        </span>
        Sync
      </button>
      <span v-if="activityStore.hasActiveOperations" class="op-spinner"></span>
      <span class="status-dot" :class="connectionStatus" data-testid="sync-status"></span>
      <span v-if="syncError" class="sync-error-msg" data-testid="sync-error">{{ syncError }}</span>
      <button v-else-if="opsStore.hasFailures" class="sync-error-msg" data-testid="op-failure" @click="opsStore.clearFailures()" title="Click to dismiss">{{ opsStore.recentFailures[0]?.error }}</button>
      <span v-else-if="connectionStatus === 'disconnected'" class="disconnect-msg">Offline — reconnecting...</span>
      <span v-else class="account-info">{{ accountsStore.accounts.length }} account{{ accountsStore.accounts.length !== 1 ? 's' : '' }} connected</span>
    </div>
    <div class="status-right">
      <span v-if="lastSyncLabel" class="last-sync">{{ lastSyncLabel }}</span>
      <button
        class="ops-toggle-btn"
        :class="{ active: uiStore.operationsPanelOpen }"
        title="Operations"
        data-testid="ops-toggle-btn"
        @click="uiStore.toggleOperationsPanel()"
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="22 12 18 12 15 21 9 3 6 12 2 12" />
        </svg>
        <span v-if="activityStore.activeOperations.length > 0" class="ops-badge">{{ activityStore.activeOperations.length }}</span>
      </button>
      <button class="help-btn" title="Help">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="12" cy="12" r="10" />
          <path d="M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3M12 17h.01" />
        </svg>
      </button>
    </div>
  </div>
</template>

<style scoped>
.status-bar {
  height: 32px;
  background: var(--color-bg);
  border-top: 1px solid var(--color-border);
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 12px;
  font-size: 11px;
  color: var(--color-text-muted);
  flex-shrink: 0;
}

.status-left {
  display: flex;
  align-items: center;
  gap: 8px;
}

.status-right {
  display: flex;
  align-items: center;
  gap: 8px;
}

.sync-btn {
  display: flex;
  align-items: center;
  gap: 4px;
  font-size: 11px;
  color: var(--color-text-secondary);
  padding: 2px 6px;
  border-radius: 4px;
  transition: background 0.12s;
}

.sync-btn:hover {
  background: var(--color-bg-hover);
}

.sync-icon {
  display: flex;
  align-items: center;
}

.sync-icon.spinning {
  animation: spin 1s linear infinite;
}

@keyframes spin {
  from { transform: rotate(0deg); }
  to { transform: rotate(360deg); }
}

.status-dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--color-status-dot);
  flex-shrink: 0;
}

.status-dot.disconnected {
  background: var(--color-danger);
}

.status-dot.reconnecting {
  background: var(--color-warning);
}

.sync-error-msg {
  color: var(--color-danger-text);
  font-size: 11px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  /* Reset <button> browser defaults so it looks like inline text */
  background: none;
  border: none;
  padding: 0;
  cursor: pointer;
  font-family: inherit;
}

.disconnect-msg {
  color: var(--color-warning);
  font-size: 11px;
}

.op-spinner {
  width: 10px;
  height: 10px;
  border: 1.5px solid var(--color-border);
  border-top-color: var(--color-accent);
  border-radius: 50%;
  animation: spin 0.8s linear infinite;
  flex-shrink: 0;
}

.account-info {
  white-space: nowrap;
}

.last-sync {
  white-space: nowrap;
  color: var(--color-text-muted);
}

.ops-toggle-btn {
  display: flex;
  align-items: center;
  justify-content: center;
  position: relative;
  width: 24px;
  height: 20px;
  border-radius: 4px;
  color: var(--color-text-muted);
  transition: all 0.12s;
}

.ops-toggle-btn:hover,
.ops-toggle-btn.active {
  color: var(--color-text);
  background: var(--color-bg-hover);
}

.ops-badge {
  position: absolute;
  top: -2px;
  right: -2px;
  min-width: 14px;
  height: 14px;
  border-radius: 7px;
  background: var(--color-accent);
  color: #fff;
  font-size: 9px;
  font-weight: 600;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 0 3px;
}

.help-btn {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  border-radius: 50%;
  color: var(--color-text-muted);
  transition: all 0.12s;
}

.help-btn:hover {
  color: var(--color-text);
  background: var(--color-bg-hover);
}
</style>
