import { defineStore } from "pinia";
import { ref, computed } from "vue";
import { listen } from "@tauri-apps/api/event";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";

// FIFO ordering prevents concurrent signing/decryption prompts from being lost.

type PgpSecretKind = "passphrase" | "pin";

export interface PgpSecretPrompt {
  requestId: string;
  kind: PgpSecretKind;
  /** Fingerprint (passphrase) or card ident (pin). */
  target: string;
  reason: string;
}

export const usePgpPromptsStore = defineStore("pgp-prompts", () => {
  const queue = ref<PgpSecretPrompt[]>([]);
  let unlisten: UnlistenFn | null = null;

  const currentPrompt = computed<PgpSecretPrompt | null>(() =>
    queue.value[0] ?? null,
  );

  /** Idempotent. */
  async function start() {
    if (unlisten) return;
    unlisten = await listen<PgpSecretPrompt>("pgp-secret-needed", (event) => {
      queue.value.push(event.payload);
    });
  }

  function stop() {
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
  }

  async function provide(requestId: string, value: string) {
    try {
      await invoke("pgp_provide_secret", { requestId, value });
    } finally {
      removeFromQueue(requestId);
    }
  }

  async function cancel(requestId: string) {
    try {
      await invoke("pgp_cancel_secret", { requestId });
    } finally {
      removeFromQueue(requestId);
    }
  }

  function removeFromQueue(requestId: string) {
    const index = queue.value.findIndex(
      (prompt) => prompt.requestId === requestId,
    );
    if (index >= 0) queue.value.splice(index, 1);
  }

  return {
    queue,
    currentPrompt,
    start,
    stop,
    provide,
    cancel,
  };
});
