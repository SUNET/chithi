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
const listenerError = ref<string | null>(null);
const actingRowId = ref<number | null>(null);
let reloadGeneration = 0;
let disposed = false;

function isCurrentReload(generation: number, accountId: string): boolean {
  return (
    !disposed &&
    generation === reloadGeneration &&
    accountsStore.activeAccountId === accountId
  );
}

async function reload() {
  const generation = ++reloadGeneration;
  if (disposed) return;

  const accountId = accountsStore.activeAccountId;
  if (!accountId) {
    rows.value = [];
    error.value = null;
    loading.value = false;
    return;
  }
  loading.value = true;
  error.value = null;
  try {
    const nextRows = await api.listOutbox(accountId);
    if (isCurrentReload(generation, accountId)) {
      rows.value = nextRows;
    }
  } catch (e) {
    if (isCurrentReload(generation, accountId)) {
      error.value = e instanceof Error ? e.message : String(e);
    }
  } finally {
    if (isCurrentReload(generation, accountId)) {
      loading.value = false;
    }
  }
}

function activeAccountFor(row: OutboxRow): string | null {
  const accountId = accountsStore.activeAccountId;
  if (!accountId || row.account_id !== accountId) {
    showToast("The Outbox account changed. Refresh and try again.", "error", 5000);
    void reload();
    return null;
  }
  return accountId;
}

async function retry(row: OutboxRow) {
  if (actingRowId.value !== null) return;
  const accountId = activeAccountFor(row);
  if (!accountId) return;

  if (
    row.delivery_outcome_unknown &&
    !confirm(
      `Delivery of "${row.subject ?? "(no subject)"}" may already have occurred. Retrying can duplicate this message. Retry anyway?`,
    )
  ) {
    return;
  }
  actingRowId.value = row.id;
  try {
    await api.retryOutboxOp(accountId, row.id);
    showToast(`Queued "${row.subject ?? "(no subject)"}" for retry`, "info");
  } catch (e) {
    showToast(`Retry failed: ${e instanceof Error ? e.message : String(e)}`, "error", 5000);
  } finally {
    await reload();
    if (!disposed && actingRowId.value === row.id) {
      actingRowId.value = null;
    }
  }
}

async function discard(row: OutboxRow) {
  if (actingRowId.value !== null) return;
  const accountId = activeAccountFor(row);
  if (!accountId) return;

  const message = row.delivery_outcome_unknown
    ? `Discard "${row.subject ?? "(no subject)"}"? This only removes the local record and cannot cancel delivery.`
    : `Discard "${row.subject ?? "(no subject)"}"? This cannot be undone.`;
  if (!confirm(message)) {
    return;
  }
  actingRowId.value = row.id;
  try {
    await api.discardOutboxOp(accountId, row.id);
  } catch (e) {
    showToast(`Discard failed: ${e instanceof Error ? e.message : String(e)}`, "error", 5000);
  } finally {
    await reload();
    if (!disposed && actingRowId.value === row.id) {
      actingRowId.value = null;
    }
  }
}

const unlistenFns: UnlistenFn[] = [];
const pendingListenerCleanups = new Set<() => void>();
const refreshEvents = [
  "offline-queue-changed",
  "send-started",
  "send-complete",
  "send-failed",
  "send-unknown",
] as const;

onMounted(async () => {
  let listenersCommitted = false;
  let acceptingListeners = true;
  const setupListeners = new Set<UnlistenFn>();
  const cleanupSetup = () => {
    acceptingListeners = false;
    for (const unlisten of setupListeners) unlisten();
    setupListeners.clear();
    pendingListenerCleanups.delete(cleanupSetup);
  };
  pendingListenerCleanups.add(cleanupSetup);

  const registrations = refreshEvents.map((name) => {
    let registration: Promise<UnlistenFn>;
    try {
      registration = listen<{ account_id: string }>(name, (event) => {
        if (
          listenersCommitted &&
          !disposed &&
          event.payload.account_id === accountsStore.activeAccountId
        ) {
          void reload();
        }
      });
    } catch (setupError) {
      return Promise.reject(setupError);
    }
    return registration.then((unlisten) => {
      if (!acceptingListeners || disposed) {
        unlisten();
      } else {
        setupListeners.add(unlisten);
      }
      return unlisten;
    });
  });

  // Do not make initial data visibility depend on event subscription. A
  // second load after a successful commit reconciles transitions that occur
  // while listeners are still gated.
  const initialLoad = reload();
  const results = await Promise.allSettled(registrations);
  const listeners: UnlistenFn[] = [];
  let setupFailed = false;
  let setupFailure: unknown;
  for (const result of results) {
    if (result.status === "fulfilled") {
      listeners.push(result.value);
    } else if (!setupFailed) {
      setupFailed = true;
      setupFailure = result.reason;
    }
  }

  if (disposed || setupFailed) {
    cleanupSetup();
    if (!disposed && setupFailed) {
      const detail =
        setupFailure instanceof Error ? setupFailure.message : String(setupFailure);
      listenerError.value = `Automatic Outbox updates are unavailable: ${detail}. Use Refresh to update the list.`;
    }
    await initialLoad;
    return;
  }

  pendingListenerCleanups.delete(cleanupSetup);
  acceptingListeners = false;
  setupListeners.clear();
  unlistenFns.push(...listeners);
  listenersCommitted = true;
  await initialLoad;
  await reload();
});

onUnmounted(() => {
  disposed = true;
  reloadGeneration += 1;
  for (const u of unlistenFns) {
    u();
  }
  unlistenFns.length = 0;
  for (const cleanup of [...pendingListenerCleanups]) cleanup();
});

watch(
  () => accountsStore.activeAccountId,
  () => {
    rows.value = [];
    error.value = null;
    void reload();
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
  <div
    class="outbox-list"
    data-testid="outbox-list"
    role="region"
    aria-labelledby="outbox-heading"
    :aria-busy="loading"
  >
    <div class="outbox-header">
      <h2 id="outbox-heading">Outbox</h2>
      <button
        type="button"
        class="outbox-refresh"
        data-testid="outbox-refresh-btn"
        :disabled="loading || actingRowId !== null"
        @click="reload"
      >
        Refresh
      </button>
    </div>

    <div
      v-if="listenerError"
      class="outbox-listener-error"
      data-testid="outbox-listener-error"
      role="alert"
    >
      {{ listenerError }}
    </div>

    <div v-if="loading && rows.length === 0" class="outbox-empty" role="status">
      Loading...
    </div>
    <div v-else-if="error" class="outbox-error" data-testid="outbox-error" role="alert">
      {{ error }}
    </div>
    <div v-else-if="rows.length === 0" class="outbox-empty" data-testid="outbox-empty" role="status">
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
          <div
            class="outbox-status"
            :role="row.delivery_outcome_unknown ? 'alert' : 'status'"
            aria-atomic="true"
          >
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
            :aria-label="`Retry message: ${row.subject || '(no subject)'}`"
            :disabled="
              loading ||
              actingRowId !== null ||
              row.account_id !== accountsStore.activeAccountId ||
              row.status !== 'dead'
            "
            @click="retry(row)"
          >
            Retry
          </button>
          <button
            type="button"
            class="outbox-btn outbox-btn-discard"
            :data-testid="`outbox-discard-${row.id}`"
            :aria-label="`Discard message: ${row.subject || '(no subject)'}`"
            :disabled="
              loading ||
              actingRowId !== null ||
              row.account_id !== accountsStore.activeAccountId ||
              row.status === 'sending'
            "
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

.outbox-listener-error {
  color: var(--color-warning-text);
  font-size: 11px;
  margin-bottom: 8px;
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
  color: var(--color-warning-text);
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
