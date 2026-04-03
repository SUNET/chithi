<script setup lang="ts">
import { useActivityStore } from "@/stores/activity";
import { useAccountsStore } from "@/stores/accounts";
import * as api from "@/lib/tauri";

const activityStore = useActivityStore();
const accountsStore = useAccountsStore();

async function syncAll() {
  for (const account of accountsStore.accounts) {
    if (account.enabled) {
      try {
        await api.triggerSync(account.id);
      } catch (e) {
        console.error("Sync failed:", e);
      }
    }
  }
}
</script>

<template>
  <div class="status-bar">
    <div class="status-left">
      <button class="status-btn" title="Sync all accounts" @click="syncAll">
        <span class="sync-icon" :class="{ spinning: activityStore.hasActiveOperations }">&#x21BB;</span>
        Sync
      </button>
    </div>
    <div class="status-center">
      <template v-if="activityStore.activeOperations.length > 0">
        <div
          v-for="op in activityStore.activeOperations"
          :key="op.id"
          class="status-operation"
        >
          <span class="op-spinner"></span>
          <span class="op-label">{{ op.label }}</span>
          <span class="op-detail">{{ op.detail }}</span>
        </div>
      </template>
      <template v-else>
        <div
          v-for="op in activityStore.recentOperations.slice(0, 1)"
          :key="op.id"
          class="status-operation"
          :class="{ error: op.status === 'error', done: op.status === 'done' }"
        >
          <span v-if="op.status === 'done'" class="op-check">&#x2713;</span>
          <span v-else-if="op.status === 'error'" class="op-error-icon">&#x2717;</span>
          <span class="op-label">{{ op.label }}</span>
          <span class="op-detail">{{ op.detail }}</span>
        </div>
      </template>
    </div>
    <div class="status-right">
      <span class="account-count">{{ accountsStore.accounts.length }} account{{ accountsStore.accounts.length !== 1 ? 's' : '' }}</span>
    </div>
  </div>
</template>

<style scoped>
.status-bar {
  height: 28px;
  background: var(--color-bg-secondary);
  border-top: 1px solid var(--color-border);
  display: flex;
  align-items: center;
  padding: 0 8px;
  font-size: 11px;
  color: var(--color-text-muted);
  flex-shrink: 0;
  gap: 12px;
}

.status-left {
  display: flex;
  align-items: center;
  gap: 4px;
}

.status-center {
  flex: 1;
  display: flex;
  align-items: center;
  gap: 8px;
  overflow: hidden;
}

.status-right {
  flex-shrink: 0;
}

.status-btn {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 2px 8px;
  border-radius: 4px;
  font-size: 11px;
  color: var(--color-text-secondary);
  transition: background 0.15s;
}

.status-btn:hover {
  background: var(--color-bg-hover);
}

.sync-icon {
  font-size: 14px;
  display: inline-block;
}

.sync-icon.spinning {
  animation: spin 1s linear infinite;
}

@keyframes spin {
  from { transform: rotate(0deg); }
  to { transform: rotate(360deg); }
}

.status-operation {
  display: flex;
  align-items: center;
  gap: 6px;
  overflow: hidden;
}

.status-operation.done {
  color: var(--color-success);
}

.status-operation.error {
  color: var(--color-danger);
}

.op-spinner {
  width: 10px;
  height: 10px;
  border: 2px solid var(--color-border);
  border-top-color: var(--color-accent);
  border-radius: 50%;
  animation: spin 0.8s linear infinite;
  flex-shrink: 0;
}

.op-check {
  color: var(--color-success);
  font-weight: bold;
}

.op-error-icon {
  color: var(--color-danger);
  font-weight: bold;
}

.op-label {
  font-weight: 500;
  white-space: nowrap;
}

.op-detail {
  color: var(--color-text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.account-count {
  white-space: nowrap;
}
</style>
