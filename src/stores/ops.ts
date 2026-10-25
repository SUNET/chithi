import { defineStore } from "pinia";
import { ref, computed, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import type { FailedOp } from "@/lib/types";

const FAILURE_RETENTION_MS = 60_000;
const MAX_RECENT_FAILURES = 20;

export const useOpsStore = defineStore("ops", () => {
  const failedOps = ref<FailedOp[]>([]);
  const initialized = ref(false);

  const hasFailures = computed(() => failedOps.value.length > 0);

  const recentFailures = computed(() =>
    [...failedOps.value]
      .sort((a, b) => b.timestamp - a.timestamp)
      .slice(0, MAX_RECENT_FAILURES),
  );

  function clearFailures() {
    failedOps.value = [];
  }

  let disposed = false;
  let stopOpFailed: (() => void) | null = null;
  const timerIds: ReturnType<typeof setTimeout>[] = [];

  async function initEventListeners() {
    if (initialized.value) return;

    try {
      stopOpFailed = await listen<Omit<FailedOp, "timestamp">>("op-failed", (event) => {
        if (disposed) return;
        failedOps.value = [
          ...failedOps.value,
          {
            ...event.payload,
            timestamp: Date.now(),
          },
        ];
        timerIds.push(
          setTimeout(() => {
            failedOps.value = failedOps.value.filter(
              (operation) =>
                Date.now() - operation.timestamp < FAILURE_RETENTION_MS,
            );
          }, FAILURE_RETENTION_MS),
        );
      });
      initialized.value = true;
    } catch (error) {
      console.error("Failed to initialize ops event listeners:", error);
    }
  }

  onScopeDispose(() => {
    disposed = true;
    stopOpFailed?.();
    for (const timerId of timerIds) {
      clearTimeout(timerId);
    }
    timerIds.length = 0;
  });

  return {
    hasFailures,
    recentFailures,
    clearFailures,
    initEventListeners,
  };
});
