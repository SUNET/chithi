import { defineStore } from "pinia";
import { ref, computed, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import type { FailedOp, OfflineQueueChange } from "@/lib/types";

/**
 * Centralized store for tracking background operation failures and
 * offline queue status. Listens for `op-failed` and `offline-queue-changed`
 * events emitted by the Rust backend worker.
 *
 * Components can use `hasFailures` and `recentFailures` to show
 * error indicators in the status bar or toast notifications.
 */
export const useOpsStore = defineStore("ops", () => {
  const failedOps = ref<FailedOp[]>([]);
  const deadOps = ref<OfflineQueueChange[]>([]);
  const initialized = ref(false);

  const hasFailures = computed(() => failedOps.value.length > 0);
  const hasDeadOps = computed(() => deadOps.value.length > 0);

  /** Most recent failures (newest first, max 20). */
  const recentFailures = computed(() =>
    [...failedOps.value].sort((a, b) => b.timestamp - a.timestamp).slice(0, 20),
  );

  function clearFailures() {
    failedOps.value = [];
  }

  function clearDeadOps() {
    deadOps.value = [];
  }

  let disposed = false;
  let stopOpFailed: null | (() => void) = null;
  let stopOfflineChanged: null | (() => void) = null;

  async function initEventListeners() {
    if (initialized.value) return;

    try {
      // Listen for background operation failures
      stopOpFailed = await listen<{
        account_id: string;
        op_type: string;
        error: string;
      }>("op-failed", (event) => {
        if (disposed) return;
        const p = event.payload;
        failedOps.value = [
          ...failedOps.value,
          {
            account_id: p.account_id,
            op_type: p.op_type,
            error: p.error,
            timestamp: Date.now(),
          },
        ];
        // Auto-clear old failures after 60 seconds
        setTimeout(() => {
          failedOps.value = failedOps.value.filter(
            (op) => Date.now() - op.timestamp < 60_000,
          );
        }, 60_000);
      });

      // Listen for dead offline operations (exceeded max retries)
      stopOfflineChanged = await listen<OfflineQueueChange>(
        "offline-queue-changed",
        (event) => {
          if (disposed) return;
          deadOps.value = [...deadOps.value, event.payload];
        },
      );

      initialized.value = true;
    } catch (err) {
      console.error("Failed to initialize ops event listeners:", err);
      // Do not set initialized = true so a retry remains possible
    }
  }

  onScopeDispose(() => {
    disposed = true;
    stopOpFailed?.();
    stopOfflineChanged?.();
  });

  return {
    failedOps,
    deadOps,
    hasFailures,
    hasDeadOps,
    recentFailures,
    clearFailures,
    clearDeadOps,
    initEventListeners,
  };
});
