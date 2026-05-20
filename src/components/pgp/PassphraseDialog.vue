<script setup lang="ts">
import { ref, onMounted, onUnmounted, nextTick } from "vue";
import type { PgpSecretPrompt } from "@/stores/pgp-prompts";
import { usePgpPromptsStore } from "@/stores/pgp-prompts";

const props = defineProps<{ prompt: PgpSecretPrompt }>();
const prompts = usePgpPromptsStore();

// `secret` is the bound input value. We clear it the moment the user
// submits AND we directly reassign the DOM element's `.value` to "" so the
// input's internal buffer doesn't keep the string around — Vue's v-model
// only tracks the JS ref, not the underlying input field.
const secret = ref("");
const inputEl = ref<HTMLInputElement | null>(null);
const busy = ref(false);
const errorMsg = ref<string | null>(null);

function clearSecret() {
  secret.value = "";
  if (inputEl.value) inputEl.value.value = "";
}

onMounted(async () => {
  await nextTick();
  inputEl.value?.focus();
});

onUnmounted(() => {
  // Belt-and-braces: if the dialog tears down for any other reason
  // (route change, parent v-if flip), wipe the ref and the DOM buffer.
  clearSecret();
});

async function submit() {
  if (busy.value) return;
  if (!secret.value) {
    errorMsg.value = "Enter the passphrase.";
    return;
  }
  errorMsg.value = null;
  busy.value = true;
  // Snapshot into a local, clear the ref+input, then ship the local to
  // the backend. The local goes out of scope at the end of this function.
  // The backend always caches; the cache is dropped when the app exits or
  // when a signing failure evicts it via `evict_cached_secret`.
  const local = secret.value;
  clearSecret();
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
  clearSecret();
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
    <div class="dialog" role="dialog" aria-modal="true" aria-labelledby="pgp-pass-title">
      <h3 id="pgp-pass-title">Unlock OpenPGP key</h3>
      <p class="reason">{{ prompt.reason }}</p>
      <code class="target">{{ prompt.target.toUpperCase() }}</code>
      <label class="field">
        <span>Passphrase</span>
        <input
          ref="inputEl"
          v-model="secret"
          type="password"
          autocomplete="off"
          spellcheck="false"
          :disabled="busy"
          data-testid="pgp-passphrase-input"
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
          data-testid="pgp-passphrase-submit"
          @click="submit"
        >
          Unlock
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
  width: min(440px, 90vw);
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
  word-break: break-all;
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
  font-size: 13px;
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
