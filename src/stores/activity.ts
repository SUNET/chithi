import { defineStore } from "pinia";
import { ref, computed, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import type { Event, UnlistenFn } from "@tauri-apps/api/event";
import { showToast, dismissToast } from "@/lib/toast";

async function wait(ms: number): Promise<void> {
  if (ms <= 0) return;
  await new Promise<void>((resolve) => setTimeout(resolve, ms));
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

export interface Operation {
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
    Array.from(operations.value.values()).filter((op) => op.status === "running"),
  );

  const recentOperations = computed(() => {
    const all = Array.from(operations.value.values());
    all.sort((a, b) => b.startedAt - a.startedAt);
    return all.slice(0, 10);
  });

  const hasActiveOperations = computed(() => activeOperations.value.length > 0);

  // Pending removal timers keyed by operation id. A second op that reuses an
  // id (e.g. a repeated sync for the same account) must cancel the earlier
  // timer, or the stale timer would wipe out the new "running" entry mid-run.
  const pendingRemovals = new Map<string, ReturnType<typeof setTimeout>>();

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
      const op = operations.value.get(id);
      // Guard against a new "running" op having taken the same id since we
      // were scheduled. We only auto-remove terminal entries.
      if (op && (op.status === "done" || op.status === "error")) {
        operations.value.delete(id);
        operations.value = new Map(operations.value);
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
    // A fresh run with the same id supersedes any pending removal.
    cancelPendingRemoval(id);
    operations.value.set(id, {
      id,
      type,
      label,
      detail,
      status: "running",
      startedAt: Date.now(),
    });
    // Trigger reactivity
    operations.value = new Map(operations.value);
    return id;
  }

  function updateOperation(id: string, detail: string) {
    const op = operations.value.get(id);
    if (op) {
      op.detail = detail;
      operations.value = new Map(operations.value);
    }
  }

  function completeOperation(id: string, detail?: string) {
    const op = operations.value.get(id);
    if (op) {
      op.status = "done";
      if (detail) op.detail = detail;
      operations.value = new Map(operations.value);
      // Auto-remove after 60 seconds (visible in operations panel)
      scheduleRemoval(id, 60_000);
    }
  }

  function failOperation(id: string, error: string) {
    const op = operations.value.get(id);
    if (op) {
      op.status = "error";
      op.error = error;
      op.detail = error;
      operations.value = new Map(operations.value);
      // Auto-remove errors after 5 minutes
      scheduleRemoval(id, 5 * 60_000);
    }
  }

  const unlistenFns: UnlistenFn[] = [];
  const sendToastIds = new Map<string, number>();
  const pendingRegistrationCleanups = new Set<() => void>();
  let initializationAttempt: { id: number; promise: Promise<void> } | null = null;
  let nextInitializationAttemptId = 0;
  let disposed = false;

  function dismissSendToast(opId: string) {
    const toastId = sendToastIds.get(opId);
    if (toastId !== undefined) {
      dismissToast(toastId);
      sendToastIds.delete(opId);
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
      const reason = signal?.reason ?? new Error("Activity listener initialization cancelled");
      cleanupAttempt();
      rejectCancellation(reason);
    };
    if (signal?.aborted) {
      cancelAttempt();
    } else {
      signal?.addEventListener("abort", cancelAttempt, { once: true });
    }

    function whenCommitted<T>(handler: (event: Event<T>) => void) {
      return (event: Event<T>) => {
        if (!committed || disposed) return;
        handler(event);
      };
    }

    // --- Mail sync events ---
    registrations.push(
      listen<{ account_id: string; account_name: string }>(
        "sync-started",
        whenCommitted((event) => {
          startOperation(
            `sync-${event.payload.account_id}`,
            "sync",
            `Syncing ${event.payload.account_name}`,
            "Syncing...",
          );
        }),
      ),
    );

    registrations.push(
      listen<{
        account_id: string;
        folder: string;
        synced: number;
        total_folders: number;
        current_folder: number;
      }>(
        "sync-progress",
        whenCommitted((event) => {
          const p = event.payload;
          updateOperation(
            `sync-${p.account_id}`,
            `${p.folder} (${p.current_folder}/${p.total_folders})${p.synced > 0 ? ` - ${p.synced} new` : ""}`,
          );
        }),
      ),
    );

    registrations.push(
      listen<{ account_id: string; total_synced: number }>(
        "sync-complete",
        whenCommitted((event) => {
          const p = event.payload;
          completeOperation(
            `sync-${p.account_id}`,
            p.total_synced > 0
              ? `Done - ${p.total_synced} new messages`
              : "Up to date",
          );
        }),
      ),
    );

    registrations.push(
      listen<{ account_id: string; error: string }>(
        "sync-error",
        whenCommitted((event) => {
          failOperation(`sync-${event.payload.account_id}`, event.payload.error);
        }),
      ),
    );

    // --- Calendar sync events ---
    // Dedicated start/complete/error events, mirroring the mail
    // `sync-started`/`sync-complete`/`sync-error` triad. We deliberately do
    // NOT listen for `calendar-changed`: that event also fires from invite
    // responses, push processing, and other non-sync mutations, so coupling
    // it to the spinner would complete the indicator prematurely.
    registrations.push(
      listen<string>(
        "calendar-sync-started",
        whenCommitted((event) => {
          startOperation(
            `cal-sync-${event.payload}`,
            "sync",
            "Syncing calendars",
            "Syncing...",
          );
        }),
      ),
    );
    registrations.push(
      listen<string>(
        "calendar-sync-complete",
        whenCommitted((event) => {
          completeOperation(`cal-sync-${event.payload}`, "Calendars updated");
        }),
      ),
    );
    registrations.push(
      listen<{ account_id: string; error: string }>(
        "calendar-sync-error",
        whenCommitted((event) => {
          failOperation(
            `cal-sync-${event.payload.account_id}`,
            event.payload.error,
          );
        }),
      ),
    );

    // --- Contacts sync events ---
    registrations.push(
      listen<string>(
        "contacts-changed",
        whenCommitted((event) => {
          completeOperation(
            `contacts-sync-${event.payload}`,
            "Contacts updated",
          );
        }),
      ),
    );

    // --- Background operation failures ---
    registrations.push(
      listen<{ account_id: string; op_type: string; error: string }>(
        "op-failed",
        whenCommitted((event) => {
          const p = event.payload;
          // Create and immediately fail an operation entry so it shows up in the
          // operations panel (failOperation is a no-op for unknown ids).
          const opId = `op-${p.account_id}-${Date.now()}`;
          startOperation(opId, "general", `${p.op_type} failed`, p.error);
          failOperation(opId, `${p.op_type}: ${p.error}`);
        }),
      ),
    );

    // --- Send events ---
    registrations.push(
      listen<{ account_id: string; subject: string; outbox_id: number }>(
        "send-started",
        whenCommitted((event) => {
          const p = event.payload;
          const opId = `send-${p.outbox_id}`;
          startOperation(opId, "send", `Sending "${p.subject}"`, "Syncing...");
          const toastId = showToast(`Sending "${p.subject}"...`, "info", 0); // persistent until complete/failed
          sendToastIds.set(opId, toastId);
        }),
      ),
    );

    registrations.push(
      listen<{ account_id: string; subject: string; outbox_id: number }>(
        "send-complete",
        whenCommitted((event) => {
          const p = event.payload;
          const opId = `send-${p.outbox_id}`;
          completeOperation(opId, "Sent");
          dismissSendToast(opId);
          showToast(`"${p.subject}" sent`, "success");
        }),
      ),
    );

    registrations.push(
      listen<{
        account_id: string;
        subject: string;
        outbox_id: number;
        error: string;
      }>(
        "send-failed",
        whenCommitted((event) => {
          const p = event.payload;
          const opId = `send-${p.outbox_id}`;
          failOperation(opId, p.error);
          dismissSendToast(opId);
          showToast(`Send failed: ${p.error}`, "error", 10000);
        }),
      ),
    );

    registrations.push(
      listen<{
        account_id: string;
        subject: string;
        outbox_id: number;
        error: string;
      }>(
        "send-unknown",
        whenCommitted((event) => {
          const p = event.payload;
          const opId = `send-${p.outbox_id}`;
          const detail = `Delivery status unknown: ${p.error}`;
          failOperation(opId, detail);
          dismissSendToast(opId);
          showToast(
            `Delivery status unknown for "${p.subject}": ${p.error}`,
            "error",
            10000,
          );
        }),
      ),
    );

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
    // Cancel any pending removal timers so they don't fire on a disposed
    // store and mutate state (or leak the handle).
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
    startOperation,
    updateOperation,
    completeOperation,
    failOperation,
    initEventListeners,
  };
});
