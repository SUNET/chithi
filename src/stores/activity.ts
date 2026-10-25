import { defineStore } from "pinia";
import { ref, computed, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import type { Event, UnlistenFn } from "@tauri-apps/api/event";
import { showToast, dismissToast } from "@/lib/toast";

const COMPLETED_OPERATION_RETENTION_MS = 60_000;
const FAILED_OPERATION_RETENTION_MS = 5 * 60_000;
const SEND_FAILURE_TOAST_DURATION_MS = 10_000;

interface SyncStartedPayload {
  account_id: string;
  account_name: string;
}

interface SyncProgressPayload {
  account_id: string;
  folder: string;
  synced: number;
  total_folders: number;
  current_folder: number;
}

interface SyncCompletePayload {
  account_id: string;
  total_synced: number;
}

interface AccountErrorPayload {
  account_id: string;
  error: string;
}

interface OperationFailurePayload extends AccountErrorPayload {
  op_type: string;
}

interface SendPayload {
  account_id: string;
  subject: string;
  outbox_id: number;
}

interface SendFailurePayload extends SendPayload {
  error: string;
}

function mailSyncOperationId(accountId: string): string {
  return `sync-${accountId}`;
}

function calendarSyncOperationId(accountId: string): string {
  return `cal-sync-${accountId}`;
}

function sendOperationId(outboxId: number): string {
  return `send-${outboxId}`;
}

function wait(ms: number): Promise<void> {
  return ms > 0
    ? new Promise<void>((resolve) => setTimeout(resolve, ms))
    : Promise.resolve();
}

async function initializeWithTimeout(
  initialize: (signal: AbortSignal) => Promise<void>,
  timeoutMs: number,
): Promise<void> {
  const controller = new AbortController();
  if (timeoutMs <= 0) {
    await initialize(controller.signal);
    return;
  }

  let timeout: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([
      initialize(controller.signal),
      new Promise<never>((_, reject) => {
        timeout = setTimeout(() => {
          const error = new Error("Activity listener initialization timed out");
          controller.abort(error);
          reject(error);
        }, timeoutMs);
      }),
    ]);
  } finally {
    if (timeout !== undefined) clearTimeout(timeout);
  }
}

export async function initializeActivityListenersWithRetry(
  initialize: (signal: AbortSignal) => Promise<void>,
  attempts = 2,
  retryDelayMs = 50,
  attemptTimeoutMs = 1000,
): Promise<unknown | null> {
  const attemptCount = Math.max(1, Math.floor(attempts));
  let lastError: unknown;
  for (let attempt = 0; attempt < attemptCount; attempt += 1) {
    try {
      await initializeWithTimeout(initialize, attemptTimeoutMs);
      return null;
    } catch (error) {
      lastError = error;
      if (attempt + 1 < attemptCount) await wait(retryDelayMs);
    }
  }
  return lastError;
}

interface Operation {
  id: string;
  type: "sync" | "send" | "general";
  label: string;
  detail: string;
  status: "running" | "done" | "error";
  startedAt: number;
  error?: string;
}

export const useActivityStore = defineStore("activity", () => {
  const operations = ref<Map<string, Operation>>(new Map());
  const initialized = ref(false);

  const activeOperations = computed(() =>
    Array.from(operations.value.values()).filter(
      (operation) => operation.status === "running",
    ),
  );

  const recentOperations = computed(() => {
    const sorted = Array.from(operations.value.values());
    sorted.sort((a, b) => b.startedAt - a.startedAt);
    return sorted.slice(0, 10);
  });

  const hasActiveOperations = computed(() => activeOperations.value.length > 0);

  // Reused IDs must cancel stale timers that could remove a newer operation.
  const pendingRemovals = new Map<string, ReturnType<typeof setTimeout>>();

  function publishOperations(): void {
    operations.value = new Map(operations.value);
  }

  function mutateOperation(
    id: string,
    mutate: (operation: Operation) => void,
  ): boolean {
    const operation = operations.value.get(id);
    if (!operation) return false;
    mutate(operation);
    publishOperations();
    return true;
  }

  function cancelPendingRemoval(id: string) {
    const handle = pendingRemovals.get(id);
    if (handle !== undefined) {
      clearTimeout(handle);
      pendingRemovals.delete(id);
    }
  }

  function scheduleRemoval(id: string, ms: number) {
    cancelPendingRemoval(id);
    const handle = setTimeout(() => {
      pendingRemovals.delete(id);
      const operation = operations.value.get(id);
      if (
        operation &&
        (operation.status === "done" || operation.status === "error")
      ) {
        operations.value.delete(id);
        publishOperations();
      }
    }, ms);
    pendingRemovals.set(id, handle);
  }

  function startOperation(
    id: string,
    type: Operation["type"],
    label: string,
    detail: string = "",
  ): string {
    cancelPendingRemoval(id);
    operations.value.set(id, {
      id,
      type,
      label,
      detail,
      status: "running",
      startedAt: Date.now(),
    });
    publishOperations();
    return id;
  }

  function updateOperation(id: string, detail: string) {
    mutateOperation(id, (operation) => {
      operation.detail = detail;
    });
  }

  function completeOperation(id: string, detail?: string) {
    const updated = mutateOperation(id, (operation) => {
      operation.status = "done";
      if (detail) operation.detail = detail;
    });
    if (updated) {
      scheduleRemoval(id, COMPLETED_OPERATION_RETENTION_MS);
    }
  }

  function failOperation(id: string, error: string) {
    const updated = mutateOperation(id, (operation) => {
      operation.status = "error";
      operation.error = error;
      operation.detail = error;
    });
    if (updated) {
      scheduleRemoval(id, FAILED_OPERATION_RETENTION_MS);
    }
  }

  const unlistenFns: UnlistenFn[] = [];
  const sendToastIds = new Map<string, number>();
  const pendingRegistrationCleanups = new Set<() => void>();
  let initializationAttempt: { id: number; promise: Promise<void> } | null = null;
  let nextInitializationAttemptId = 0;
  let disposed = false;

  function dismissSendToast(operationId: string) {
    const toastId = sendToastIds.get(operationId);
    if (toastId !== undefined) {
      dismissToast(toastId);
      sendToastIds.delete(operationId);
    }
  }

  async function registerEventListeners(signal?: AbortSignal) {
    const registrations: Promise<UnlistenFn>[] = [];
    const attemptListeners = new Set<UnlistenFn>();
    let acceptingListeners = true;
    let committed = false;

    const cleanupAttempt = () => {
      acceptingListeners = false;
      for (const unlisten of attemptListeners) unlisten();
      attemptListeners.clear();
      pendingRegistrationCleanups.delete(cleanupAttempt);
    };
    pendingRegistrationCleanups.add(cleanupAttempt);

    let rejectCancellation!: (reason: unknown) => void;
    const cancellation = new Promise<never>((_, reject) => {
      rejectCancellation = reject;
    });
    const cancelAttempt = () => {
      if (!acceptingListeners || committed) return;
      const reason =
        signal?.reason ??
        new Error("Activity listener initialization cancelled");
      cleanupAttempt();
      rejectCancellation(reason);
    };
    if (signal?.aborted) {
      cancelAttempt();
    } else {
      signal?.addEventListener("abort", cancelAttempt, { once: true });
    }

    function whenCommitted<T>(handler: (payload: T) => void) {
      return (event: Event<T>) => {
        if (!committed || disposed) return;
        handler(event.payload);
      };
    }

    function register<T>(name: string, handler: (payload: T) => void): void {
      registrations.push(listen<T>(name, whenCommitted(handler)));
    }

    register<SyncStartedPayload>("sync-started", (payload) => {
      startOperation(
        mailSyncOperationId(payload.account_id),
        "sync",
        `Syncing ${payload.account_name}`,
        "Syncing...",
      );
    });

    register<SyncProgressPayload>("sync-progress", (payload) => {
      const newMessages = payload.synced > 0 ? ` - ${payload.synced} new` : "";
      updateOperation(
        mailSyncOperationId(payload.account_id),
        `${payload.folder} (${payload.current_folder}/${payload.total_folders})${newMessages}`,
      );
    });

    register<SyncCompletePayload>("sync-complete", (payload) => {
      completeOperation(
        mailSyncOperationId(payload.account_id),
        payload.total_synced > 0
          ? `Done - ${payload.total_synced} new messages`
          : "Up to date",
      );
    });

    register<AccountErrorPayload>("sync-error", (payload) => {
      failOperation(mailSyncOperationId(payload.account_id), payload.error);
    });

    // calendar-changed includes non-sync mutations and must not drive the spinner.
    register<string>("calendar-sync-started", (accountId) => {
      startOperation(
        calendarSyncOperationId(accountId),
        "sync",
        "Syncing calendars",
        "Syncing...",
      );
    });

    register<string>("calendar-sync-complete", (accountId) => {
      completeOperation(calendarSyncOperationId(accountId), "Calendars updated");
    });

    register<AccountErrorPayload>("calendar-sync-error", (payload) => {
      failOperation(
        calendarSyncOperationId(payload.account_id),
        payload.error,
      );
    });

    register<OperationFailurePayload>("op-failed", (payload) => {
      // failOperation ignores unknown IDs, so create the visible entry first.
      const operationId = `op-${payload.account_id}-${Date.now()}`;
      startOperation(
        operationId,
        "general",
        `${payload.op_type} failed`,
        payload.error,
      );
      failOperation(operationId, `${payload.op_type}: ${payload.error}`);
    });

    register<SendPayload>("send-started", (payload) => {
      const operationId = sendOperationId(payload.outbox_id);
      startOperation(
        operationId,
        "send",
        `Sending "${payload.subject}"`,
        "Syncing...",
      );
      const toastId = showToast(`Sending "${payload.subject}"...`, "info", 0);
      sendToastIds.set(operationId, toastId);
    });

    register<SendPayload>("send-complete", (payload) => {
      const operationId = sendOperationId(payload.outbox_id);
      completeOperation(operationId, "Sent");
      dismissSendToast(operationId);
      showToast(`"${payload.subject}" sent`, "success");
    });

    register<SendFailurePayload>("send-failed", (payload) => {
      const operationId = sendOperationId(payload.outbox_id);
      failOperation(operationId, payload.error);
      dismissSendToast(operationId);
      showToast(
        `Send failed: ${payload.error}`,
        "error",
        SEND_FAILURE_TOAST_DURATION_MS,
      );
    });

    register<SendFailurePayload>("send-unknown", (payload) => {
      const operationId = sendOperationId(payload.outbox_id);
      const detail = `Delivery status unknown: ${payload.error}`;
      failOperation(operationId, detail);
      dismissSendToast(operationId);
      showToast(
        `Delivery status unknown for "${payload.subject}": ${payload.error}`,
        "error",
        SEND_FAILURE_TOAST_DURATION_MS,
      );
    });

    let listeners: UnlistenFn[];
    try {
      listeners = await Promise.race([
        Promise.all(
          registrations.map((registration) =>
            registration.then((unlisten) => {
              if (!acceptingListeners || disposed) {
                unlisten();
              } else {
                attemptListeners.add(unlisten);
              }
              return unlisten;
            }),
          ),
        ),
        cancellation,
      ]);
    } catch (error) {
      cleanupAttempt();
      throw error;
    } finally {
      signal?.removeEventListener("abort", cancelAttempt);
    }

    if (disposed) {
      cleanupAttempt();
      return;
    }

    pendingRegistrationCleanups.delete(cleanupAttempt);
    acceptingListeners = false;
    attemptListeners.clear();
    unlistenFns.push(...listeners);
    committed = true;
    initialized.value = true;
  }

  function initEventListeners(signal?: AbortSignal): Promise<void> {
    if (initialized.value) return Promise.resolve();
    if (initializationAttempt) return initializationAttempt.promise;

    const attemptId = ++nextInitializationAttemptId;
    const promise = registerEventListeners(signal).finally(() => {
      if (initializationAttempt?.id === attemptId) {
        initializationAttempt = null;
      }
    });
    initializationAttempt = { id: attemptId, promise };
    return promise;
  }

  onScopeDispose(() => {
    disposed = true;
    for (const unlisten of unlistenFns) {
      unlisten();
    }
    unlistenFns.length = 0;
    for (const cleanup of [...pendingRegistrationCleanups]) cleanup();
    for (const toastId of sendToastIds.values()) dismissToast(toastId);
    sendToastIds.clear();
    for (const handle of pendingRemovals.values()) {
      clearTimeout(handle);
    }
    pendingRemovals.clear();
  });

  return {
    operations,
    activeOperations,
    recentOperations,
    hasActiveOperations,
    initEventListeners,
  };
});
