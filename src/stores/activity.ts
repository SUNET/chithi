import { defineStore } from "pinia";
import { ref, computed } from "vue";
import { listen } from "@tauri-apps/api/event";

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
      // Auto-remove after 5 seconds
      setTimeout(() => {
        operations.value.delete(id);
        operations.value = new Map(operations.value);
      }, 5000);
    }
  }

  function failOperation(id: string, error: string) {
    const op = operations.value.get(id);
    if (op) {
      op.status = "error";
      op.error = error;
      op.detail = error;
      operations.value = new Map(operations.value);
      // Auto-remove errors after 15 seconds
      setTimeout(() => {
        operations.value.delete(id);
        operations.value = new Map(operations.value);
      }, 15000);
    }
  }

  async function initEventListeners() {
    if (initialized.value) return;
    initialized.value = true;

    await listen<{ account_id: string; account_name: string }>(
      "sync-started",
      (event) => {
        startOperation(
          `sync-${event.payload.account_id}`,
          "sync",
          `Syncing ${event.payload.account_name}`,
          "Connecting...",
        );
      },
    );

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
    });

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
    );

    await listen<{ account_id: string; error: string }>(
      "sync-error",
      (event) => {
        failOperation(`sync-${event.payload.account_id}`, event.payload.error);
      },
    );
  }

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
