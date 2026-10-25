<script setup lang="ts">
import { ref, computed, watch, onMounted, onUnmounted } from "vue";
import { useUiStore } from "@/stores/ui";
import * as api from "@/lib/tauri";
import { showToast } from "@/lib/toast";

const uiStore = useUiStore();

const cleaned = ref<string | null>(null);
const cleaning = ref(false);
const cleanError = ref(false);

const original = computed(() => uiStore.linkPopupUrl);
const wasModified = computed(
  () => cleaned.value !== null && original.value !== null && cleaned.value !== original.value,
);
// Keep schemes aligned with the backend allowlist.
const OPENABLE_SCHEMES = ["http://", "https://", "mailto:", "tel:"];
const openableScheme = computed(() => {
  const prefix = (original.value ?? "").slice(0, 8).toLowerCase();
  return OPENABLE_SCHEMES.some((scheme) => prefix.startsWith(scheme));
});

// An unchanged result means no tracking parameters were found.
watch(
  () => uiStore.linkPopupUrl,
  async (url) => {
    cleaned.value = null;
    cleanError.value = false;
    if (!url) return;
    cleaning.value = true;
    try {
      cleaned.value = await api.cleanUrl(url);
    } catch {
      // Preserve a usable URL while surfacing the cleaning failure.
      cleaned.value = url;
      cleanError.value = true;
    } finally {
      cleaning.value = false;
    }
  },
  { immediate: true },
);

async function onCopy() {
  if (!original.value) return;
  try {
    await navigator.clipboard.writeText(original.value);
    showToast("Link copied to clipboard", "success");
  } catch (error) {
    showToast("Copy failed: " + String(error), "error");
  }
  uiStore.closeLinkPopup();
}

async function onOpen() {
  if (!original.value) return;
  try {
    // The backend re-sanitizes the original before opening it.
    await api.openLink(original.value);
  } catch (error) {
    showToast("Open failed: " + String(error), "error");
  }
  uiStore.closeLinkPopup();
}

function onCancel() {
  uiStore.closeLinkPopup();
}

function onKeydown(event: KeyboardEvent) {
  if (!uiStore.linkPopupUrl) return;
  if (event.key === "Escape") {
    event.preventDefault();
    onCancel();
  }
}

onMounted(() => window.addEventListener("keydown", onKeydown));
onUnmounted(() => window.removeEventListener("keydown", onKeydown));
</script>

<template>
  <div
    v-if="uiStore.linkPopupUrl"
    class="link-popup-backdrop"
    data-testid="link-popup"
    @click.self="onCancel"
  >
    <div class="link-popup" role="dialog" aria-labelledby="link-popup-title">
      <div id="link-popup-title" class="link-popup-title">Open link</div>

      <div class="link-popup-row">
        <span class="link-popup-label">URL</span>
        <span class="link-popup-url" data-testid="link-popup-original">{{ original }}</span>
      </div>

      <div v-if="wasModified" class="link-popup-row clean">
        <span class="link-popup-label">Open as</span>
        <span class="link-popup-url clean" data-testid="link-popup-cleaned">{{ cleaned }}</span>
      </div>
      <div v-else-if="cleaning" class="link-popup-row hint">
        <span class="link-popup-hint">Checking for tracking parameters...</span>
      </div>
      <div v-else-if="cleanError" class="link-popup-row hint">
        <span class="link-popup-hint error" data-testid="link-popup-clean-error">
          Could not check for tracking parameters.
        </span>
      </div>
      <div v-else class="link-popup-row hint">
        <span class="link-popup-hint">No tracking parameters detected.</span>
      </div>

      <div v-if="!openableScheme" class="link-popup-row hint">
        <span class="link-popup-hint warn" data-testid="link-popup-unopenable">
          chithi can only open http(s), mailto:, and tel: links. Copy this one and paste it where you need it.
        </span>
      </div>

      <div class="link-popup-actions">
        <button
          type="button"
          class="link-popup-btn"
          data-testid="link-popup-cancel"
          @click="onCancel"
        >Cancel</button>
        <button
          type="button"
          class="link-popup-btn"
          data-testid="link-popup-copy"
          @click="onCopy"
        >Copy</button>
        <button
          type="button"
          class="link-popup-btn primary"
          :disabled="!openableScheme"
          :title="openableScheme ? '' : 'Only http(s) URLs can be opened from chithi'"
          data-testid="link-popup-open"
          @click="onOpen"
        >Open</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.link-popup-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.4);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 1000;
  padding: 16px;
}

.link-popup {
  background: var(--color-bg);
  color: var(--color-text);
  border: 1px solid var(--color-border);
  border-radius: 8px;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.25);
  width: 100%;
  max-width: 520px;
  padding: 16px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.link-popup-title {
  font-size: 14px;
  font-weight: 600;
}

.link-popup-row {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.link-popup-label {
  font-size: 10px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
  color: var(--color-text-muted);
}

.link-popup-url {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
  word-break: break-all;
  background: var(--color-bg-secondary, var(--color-bg-hover));
  border: 1px solid var(--color-border);
  border-radius: 4px;
  padding: 6px 8px;
}

.link-popup-url.clean {
  border-color: var(--color-accent);
}

.link-popup-hint {
  font-size: 11px;
  color: var(--color-text-muted);
}

.link-popup-hint.error {
  color: var(--color-danger-text, var(--color-danger));
}

.link-popup-hint.warn {
  color: var(--color-warning, var(--color-text));
}

.link-popup-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 4px;
}

.link-popup-btn {
  font-size: 12px;
  padding: 6px 12px;
  border-radius: 4px;
  border: 1px solid var(--color-border);
  background: var(--color-bg);
  color: var(--color-text);
  cursor: pointer;
}

.link-popup-btn:hover {
  background: var(--color-bg-hover);
}

.link-popup-btn.primary {
  background: var(--color-accent);
  color: #fff;
  border-color: var(--color-accent);
}

.link-popup-btn.primary:hover {
  filter: brightness(1.05);
}

.link-popup-btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
