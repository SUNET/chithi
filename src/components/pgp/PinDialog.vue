<script setup lang="ts">
import { ref, onMounted, onUnmounted, nextTick } from "vue";
import type { PgpSecretPrompt } from "@/stores/pgp-prompts";
import { usePgpPromptsStore } from "@/stores/pgp-prompts";

const props = defineProps<{ prompt: PgpSecretPrompt }>();
const prompts = usePgpPromptsStore();

// Mirror of PassphraseDialog with a numeric-keypad inputmode and
// PIN-length validation. Same clearing discipline: secret ref AND DOM
// input buffer wiped on submit, cancel, and unmount.
const pin = ref("");
const inputEl = ref<HTMLInputElement | null>(null);
const busy = ref(false);
const errorMsg = ref<string | null>(null);

function clearPin() {
  pin.value = "";
  if (inputEl.value) inputEl.value.value = "";
}

onMounted(async () => {
  await nextTick();
  inputEl.value?.focus();
});

onUnmounted(() => {
  clearPin();
});

async function submit() {
  if (busy.value) return;
  if (pin.value.length < 4) {
    errorMsg.value = "PIN must be at least 4 digits.";
    return;
  }
  errorMsg.value = null;
  busy.value = true;
  // Backend caches unconditionally; the cache is dropped on app exit or
  // when a signing failure evicts the entry via `evict_cached_secret`.
  const local = pin.value;
  clearPin();
  try {
    await prompts.provide(props.prompt.requestId, local);
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : String(e);
  } finally {
    busy.value = false;
  }
}

async function cancel() {
  if (busy.value) return;
  busy.value = true;
  clearPin();
  try {
    await prompts.cancel(props.prompt.requestId);
  } finally {
    busy.value = false;
  }
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === "Escape") {
    e.preventDefault();
    cancel();
  } else if (e.key === "Enter") {
    e.preventDefault();
    submit();
  }
}
</script>

<template>
  <div class="overlay" @click.self="cancel">
    <div class="dialog" role="dialog" aria-modal="true" aria-labelledby="pgp-pin-title">
      <h3 id="pgp-pin-title">Smartcard PIN</h3>
      <p class="reason">{{ prompt.reason }}</p>
      <code class="target">{{ prompt.target }}</code>
      <label class="field">
        <span>User PIN</span>
        <input
          ref="inputEl"
          v-model="pin"
          type="password"
          inputmode="numeric"
          autocomplete="off"
          spellcheck="false"
          :disabled="busy"
          data-testid="pgp-pin-input"
          @keydown="onKeydown"
        />
      </label>
      <p v-if="errorMsg" class="error">{{ errorMsg }}</p>
      <div class="actions">
        <span class="spacer"></span>
        <button class="btn" :disabled="busy" @click="cancel">Cancel</button>
        <button
          class="btn btn-primary"
          :disabled="busy"
          data-testid="pgp-pin-submit"
          @click="submit"
        >
          Continue
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.4);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 1100;
}
.dialog {
  background: var(--color-bg);
  padding: 20px;
  border-radius: var(--radius);
  width: min(380px, 90vw);
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.dialog h3 {
  margin: 0;
}
.reason {
  margin: 0;
  font-size: 13px;
  color: var(--color-text);
}
.target {
  font-family: var(--font-mono, monospace);
  font-size: 11px;
  color: var(--color-text-muted);
}
.field {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 12px;
}
.field input {
  padding: 8px;
  border-radius: var(--radius);
  border: 0.8px solid var(--color-border);
  background: var(--color-bg-secondary, var(--color-bg));
  color: var(--color-text);
  font-family: var(--font-mono, monospace);
  font-size: 14px;
  letter-spacing: 0.3em;
}
.error {
  margin: 0;
  color: var(--color-danger, #fb2c36);
  font-size: 12px;
}
.actions {
  display: flex;
  gap: 8px;
}
.spacer {
  flex: 1;
}
.btn {
  padding: 6px 12px;
  border-radius: var(--radius);
  border: 0.8px solid var(--color-border);
  background: var(--color-bg);
  color: var(--color-text);
  font-size: 13px;
  cursor: pointer;
}
.btn:hover:not(:disabled) {
  background: var(--color-bg-hover);
}
.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.btn-primary {
  background: var(--color-accent);
  color: #fff;
  border-color: var(--color-accent);
}
</style>
