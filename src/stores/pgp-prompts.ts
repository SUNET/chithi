import { defineStore } from "pinia";
import { ref, computed } from "vue";
import { listen } from "@tauri-apps/api/event";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";

// Drives PassphraseDialog.vue and PinDialog.vue. Subscribes once on app
// boot to the backend `pgp-secret-needed` event and queues each request
// FIFO so back-to-back sign/decrypt calls don't lose prompts. The head of
// the queue is what the active dialog renders.

export type PgpSecretKind = "passphrase" | "pin";

export interface PgpSecretPrompt {
  requestId: string;
  kind: PgpSecretKind;
  /** Fingerprint (passphrase) or card ident (pin). */
  target: string;
  /** Human-readable reason: "Decrypt message from Alice", etc. */
  reason: string;
}

export const usePgpPromptsStore = defineStore("pgp-prompts", () => {
  // Queue of pending prompts. Append on event, shift on resolve/cancel.
  const queue = ref<PgpSecretPrompt[]>([]);
  let unlisten: UnlistenFn | null = null;

  const currentPrompt = computed<PgpSecretPrompt | null>(() =>
    queue.value.length > 0 ? queue.value[0] : null,
  );

  /** Idempotent — call from App.vue's onMounted. */
  async function start() {
    if (unlisten) return;
    unlisten = await listen<PgpSecretPrompt>("pgp-secret-needed", (event) => {
      // Backend payload uses camelCase already (serde rename_all =
      // "camelCase" on SecretPromptPayload). Trust the shape.
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
    const idx = queue.value.findIndex((p) => p.requestId === requestId);
    if (idx >= 0) queue.value.splice(idx, 1);
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
