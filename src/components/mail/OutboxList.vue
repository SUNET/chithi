<script setup lang="ts">
import { onMounted, onUnmounted, ref, watch } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useAccountsStore } from "@/stores/accounts";
import * as api from "@/lib/tauri";
import type { OutboxRow } from "@/lib/types";
import { showToast } from "@/lib/toast";

const accountsStore = useAccountsStore();
const rows = ref<OutboxRow[]>([]);
const loading = ref(false);
const error = ref<string | null>(null);

async function reload() {
  const accountId = accountsStore.activeAccountId;
  if (!accountId) {
    rows.value = [];
    return;
  }
  loading.value = true;
  error.value = null;
  try {
    rows.value = await api.listOutbox(accountId);
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

async function retry(row: OutboxRow) {
  if (
    row.delivery_outcome_unknown &&
    !confirm(
      `Delivery of "${row.subject ?? "(no subject)"}" may already have occurred. Retrying can duplicate this message. Retry anyway?`,
    )
  ) {
    return;
  }
  try {
    await api.retryOutboxOp(row.id);
    showToast(`Queued "${row.subject ?? "(no subject)"}" for retry`, "info");
    await reload();
  } catch (e) {
    showToast(`Retry failed: ${e instanceof Error ? e.message : String(e)}`, "error", 5000);
  }
}

async function discard(row: OutboxRow) {
  const message = row.delivery_outcome_unknown
    ? `Discard "${row.subject ?? "(no subject)"}"? This only removes the local record and cannot cancel delivery.`
    : `Discard "${row.subject ?? "(no subject)"}"? This cannot be undone.`;
  if (!confirm(message)) {
    return;
  }
  try {
    await api.discardOutboxOp(row.id);
    rows.value = rows.value.filter((r) => r.id !== row.id);
  } catch (e) {
    showToast(`Discard failed: ${e instanceof Error ? e.message : String(e)}`, "error", 5000);
  }
}

const unlistenFns: UnlistenFn[] = [];

onMounted(async () => {
  await reload();
  // Refresh when the worker drains the outbox or marks something dead.
  unlistenFns.push(
    await listen<{ account_id: string }>("offline-queue-changed", (event) => {
      if (event.payload.account_id === accountsStore.activeAccountId) {
        reload();
      }
    }),
  );
  unlistenFns.push(
    await listen<{ account_id: string }>("send-started", (event) => {
      if (event.payload.account_id === accountsStore.activeAccountId) {
        reload();
      }
    }),
  );
  unlistenFns.push(
    await listen<{ account_id: string }>("send-complete", (event) => {
      if (event.payload.account_id === accountsStore.activeAccountId) {
        reload();
      }
    }),
  );
  unlistenFns.push(
    await listen<{ account_id: string }>("send-failed", (event) => {
      if (event.payload.account_id === accountsStore.activeAccountId) {
        reload();
      }
    }),
  );
  unlistenFns.push(
    await listen<{ account_id: string }>("send-unknown", (event) => {
      if (event.payload.account_id === accountsStore.activeAccountId) {
        reload();
      }
    }),
  );
});

onUnmounted(() => {
  for (const u of unlistenFns) {
    u();
  }
});

watch(
  () => accountsStore.activeAccountId,
  () => {
    reload();
  },
);

function recipients(row: OutboxRow): string {
  const all = [...row.to, ...row.cc, ...row.bcc];
  if (all.length === 0) return "(no recipients)";
  if (all.length <= 3) return all.join(", ");
  return `${all.slice(0, 3).join(", ")} +${all.length - 3} more`;
}

function statusLabel(row: OutboxRow): string {
  if (row.status === "sending") return "Sending...";
  if (row.status === "dead" && row.delivery_outcome_unknown) {
    return "Delivery status unknown";
  }
  if (row.status === "dead") return `Failed (${row.retry_count} attempts)`;
  if (row.retry_count > 0) return `Queued for retry (${row.retry_count} so far)`;
  return "Queued";
}
</script>

<template>
  <div class="outbox-list" data-testid="outbox-list">
    <div class="outbox-header">
      <h2>Outbox</h2>
      <button
        type="button"
        class="outbox-refresh"
        data-testid="outbox-refresh-btn"
        :disabled="loading"
        @click="reload"
      >
        Refresh
      </button>
    </div>

    <div v-if="loading && rows.length === 0" class="outbox-empty">Loading...</div>
    <div v-else-if="error" class="outbox-error" data-testid="outbox-error">{{ error }}</div>
    <div v-else-if="rows.length === 0" class="outbox-empty" data-testid="outbox-empty">
      No queued or failed sends.
    </div>

    <ul v-else class="outbox-items">
      <li
        v-for="row in rows"
        :key="row.id"
        class="outbox-item"
        :class="{
          dead: row.status === 'dead',
          sending: row.status === 'sending',
          unknown: row.delivery_outcome_unknown,
        }"
        :data-testid="`outbox-item-${row.id}`"
      >
        <div class="outbox-item-main">
          <div class="outbox-subject">{{ row.subject || "(no subject)" }}</div>
          <div class="outbox-recipients">{{ recipients(row) }}</div>
          <div class="outbox-status">
            <span class="outbox-status-label">{{ statusLabel(row) }}</span>
            <span v-if="row.error_message" class="outbox-error-msg" :title="row.error_message">
              {{ row.error_message }}
            </span>
          </div>
        </div>
        <div class="outbox-actions">
          <button
            type="button"
            class="outbox-btn outbox-btn-retry"
            :data-testid="`outbox-retry-${row.id}`"
            :disabled="row.status !== 'dead'"
            @click="retry(row)"
          >
            Retry
          </button>
          <button
            type="button"
            class="outbox-btn outbox-btn-discard"
            :data-testid="`outbox-discard-${row.id}`"
            :disabled="row.status === 'sending'"
            @click="discard(row)"
          >
            Discard
          </button>
        </div>
      </li>
    </ul>
  </div>
</template>

<style scoped>
.outbox-list {
  flex: 1;
  display: flex;
  flex-direction: column;
  overflow: hidden;
  padding: 12px 16px;
}

.outbox-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 12px;
}

.outbox-header h2 {
  font-size: 14px;
  font-weight: 600;
  color: var(--color-text);
  margin: 0;
}

.outbox-refresh {
  font-size: 12px;
  color: var(--color-text-muted);
  background: none;
  border: 1px solid var(--color-border);
  padding: 4px 10px;
  border-radius: 4px;
  cursor: pointer;
}

.outbox-refresh:disabled {
  opacity: 0.5;
  cursor: default;
}

.outbox-empty,
.outbox-error {
  font-size: 12px;
  color: var(--color-text-muted);
  padding: 16px;
  text-align: center;
}

.outbox-error {
  color: var(--color-danger-text);
}

.outbox-items {
  list-style: none;
  padding: 0;
  margin: 0;
  overflow-y: auto;
}

.outbox-item {
  display: flex;
  gap: 12px;
  padding: 10px 12px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  background: var(--color-bg);
  margin-bottom: 8px;
}

.outbox-item.dead {
  border-color: var(--color-danger);
}

.outbox-item.unknown {
  border-color: var(--color-warning);
}

.outbox-item.sending {
  opacity: 0.7;
}

.outbox-item-main {
  flex: 1;
  min-width: 0;
}

.outbox-subject {
  font-size: 13px;
  font-weight: 600;
  color: var(--color-text);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.outbox-recipients {
  font-size: 12px;
  color: var(--color-text-muted);
  margin-top: 2px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.outbox-status {
  font-size: 11px;
  margin-top: 4px;
  display: flex;
  gap: 8px;
  align-items: baseline;
  flex-wrap: wrap;
}

.outbox-status-label {
  color: var(--color-text-muted);
}

.outbox-item.dead .outbox-status-label {
  color: var(--color-danger-text);
}

.outbox-item.unknown .outbox-status-label,
.outbox-item.unknown .outbox-error-msg {
  color: var(--color-warning);
}

.outbox-error-msg {
  color: var(--color-danger-text);
  font-family: monospace;
  font-size: 10px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  max-width: 100%;
}

.outbox-actions {
  display: flex;
  flex-direction: column;
  gap: 4px;
  flex-shrink: 0;
}

.outbox-btn {
  font-size: 11px;
  padding: 4px 10px;
  border-radius: 4px;
  border: 1px solid var(--color-border);
  background: var(--color-bg);
  color: var(--color-text);
  cursor: pointer;
}

.outbox-btn:disabled {
  opacity: 0.5;
  cursor: default;
}

.outbox-btn-retry:hover:not(:disabled) {
  background: var(--color-bg-hover);
}

.outbox-btn-discard {
  color: var(--color-danger-text);
}

.outbox-btn-discard:hover:not(:disabled) {
  background: var(--color-danger);
  color: #fff;
  border-color: var(--color-danger);
}
</style>
