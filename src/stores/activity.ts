import { defineStore } from "pinia";
import { ref, computed, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { showToast, dismissToast } from "@/lib/toast";

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

  function startOperation(
    id: string,
    type: Operation["type"],
    label: string,
    detail: string = "",
  ): string {
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
      setTimeout(() => {
        operations.value.delete(id);
        operations.value = new Map(operations.value);
      }, 60_000);
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
      setTimeout(() => {
        operations.value.delete(id);
        operations.value = new Map(operations.value);
      }, 5 * 60_000);
    }
  }

  const unlistenFns: UnlistenFn[] = [];

  async function initEventListeners() {
    if (initialized.value) return;
    initialized.value = true;

    // --- Mail sync events ---
    unlistenFns.push(
      await listen<{ account_id: string; account_name: string }>(
        "sync-started",
        (event) => {
          startOperation(
            `sync-${event.payload.account_id}`,
            "sync",
            `Syncing ${event.payload.account_name}`,
            "Syncing...",
          );
        },
      ),
    );

    unlistenFns.push(
      await listen<{
        account_id: string;
        folder: string;
        synced: number;
        total_folders: number;
        current_folder: number;
      }>("sync-progress", (event) => {
        const p = event.payload;
        updateOperation(
          `sync-${p.account_id}`,
          `${p.folder} (${p.current_folder}/${p.total_folders})${p.synced > 0 ? ` - ${p.synced} new` : ""}`,
        );
      }),
    );

    unlistenFns.push(
      await listen<{ account_id: string; total_synced: number }>(
        "sync-complete",
        (event) => {
          const p = event.payload;
          completeOperation(
            `sync-${p.account_id}`,
            p.total_synced > 0
              ? `Done - ${p.total_synced} new messages`
              : "Up to date",
          );
        },
      ),
    );

    unlistenFns.push(
      await listen<{ account_id: string; error: string }>(
        "sync-error",
        (event) => {
          failOperation(`sync-${event.payload.account_id}`, event.payload.error);
        },
      ),
    );

    // --- Calendar sync events ---
    unlistenFns.push(
      await listen<string>("calendar-changed", (event) => {
        completeOperation(
          `cal-sync-${event.payload}`,
          "Calendars updated",
        );
      }),
    );

    // --- Contacts sync events ---
    unlistenFns.push(
      await listen<string>("contacts-changed", (event) => {
        completeOperation(
          `contacts-sync-${event.payload}`,
          "Contacts updated",
        );
      }),
    );

    // --- Background operation failures ---
    unlistenFns.push(
      await listen<{ account_id: string; op_type: string; error: string }>(
        "op-failed",
        (event) => {
          const p = event.payload;
          // Create and immediately fail an operation entry so it shows up in the
          // operations panel (failOperation is a no-op for unknown ids).
          const opId = `op-${p.account_id}-${Date.now()}`;
          startOperation(opId, "general", `${p.op_type} failed`, p.error);
          failOperation(opId, `${p.op_type}: ${p.error}`);
        },
      ),
    );

    // Maps an operation id → toast id so we can dismiss the persistent
    // "Sending..." toast when the send completes or fails.
    const sendToastIds = new Map<string, number>();

    // --- Send events ---
    unlistenFns.push(
      await listen<{ account_id: string; subject: string }>(
        "send-started",
        (event) => {
          const p = event.payload;
          const opId = `send-${p.account_id}-${Date.now()}`;
          startOperation(opId, "send", `Sending "${p.subject}"`, "Syncing...");
          const toastId = showToast(`Sending "${p.subject}"...`, "info", 0); // persistent until complete/failed
          sendToastIds.set(opId, toastId);
        },
      ),
    );

    unlistenFns.push(
      await listen<{ account_id: string; subject: string }>(
        "send-complete",
        (event) => {
          const p = event.payload;
          // Complete all running send operations for this account
          for (const [id, op] of operations.value) {
            if (op.type === "send" && op.status === "running" && id.startsWith(`send-${p.account_id}`)) {
              completeOperation(id, "Sent");
              const toastId = sendToastIds.get(id);
              if (toastId !== undefined) {
                dismissToast(toastId);
                sendToastIds.delete(id);
              }
            }
          }
          showToast(`"${p.subject}" sent`, "success");
        },
      ),
    );

    unlistenFns.push(
      await listen<{ account_id: string; subject: string; error: string }>(
        "send-failed",
        (event) => {
          const p = event.payload;
          // Fail all running send operations for this account
          for (const [id, op] of operations.value) {
            if (op.type === "send" && op.status === "running" && id.startsWith(`send-${p.account_id}`)) {
              failOperation(id, p.error);
              const toastId = sendToastIds.get(id);
              if (toastId !== undefined) {
                dismissToast(toastId);
                sendToastIds.delete(id);
              }
            }
          }
          showToast(`Send failed: ${p.error}`, "error", 10000);
        },
      ),
    );
  }

  onScopeDispose(() => {
    for (const unlisten of unlistenFns) {
      unlisten();
    }
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
