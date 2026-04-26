<script setup lang="ts">
import { onMounted, onUnmounted } from "vue";
import { openUrl } from "@tauri-apps/plugin-opener";

const props = defineProps<{ open: boolean }>();
const emit = defineEmits<{ close: [] }>();

const SOURCE_URL = "https://github.com/SUNET/chithi";
const LICENSE_URL = "https://github.com/SUNET/chithi/blob/main/LICENSE";

const version = __APP_VERSION__;

async function openExternal(url: string) {
  try {
    await openUrl(url);
  } catch (e) {
    console.error("Failed to open URL:", url, e);
  }
}

function onKeyDown(e: KeyboardEvent) {
  if (!props.open || e.key !== "Escape") return;
  // Vue mounts children before parents, so this listener registers BEFORE
  // any ancestor menubar's keydown listener. Use stopImmediatePropagation
  // so the parent's Esc-bound action (e.g. close-window in compose) does
  // not fire when the user is just dismissing this modal.
  e.stopImmediatePropagation();
  e.preventDefault();
  emit("close");
}

onMounted(() => window.addEventListener("keydown", onKeyDown));
onUnmounted(() => window.removeEventListener("keydown", onKeyDown));
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      class="about-overlay"
      data-testid="about-overlay"
      @click.self="emit('close')"
    >
      <div class="about-modal" role="dialog" aria-modal="true" aria-labelledby="about-title">
        <header class="about-header">
          <h2 id="about-title">Chithi</h2>
          <button
            class="about-close"
            data-testid="about-close"
            aria-label="Close"
            @click="emit('close')"
          >&times;</button>
        </header>

        <div class="about-body">
          <p class="about-tagline">A personal mail and calendar client.</p>
          <dl class="about-meta">
            <dt>Version</dt>
            <dd data-testid="about-version">{{ version }}</dd>
            <dt>Source</dt>
            <dd>
              <a
                href="#"
                data-testid="about-source-link"
                @click.prevent="openExternal(SOURCE_URL)"
              >github.com/SUNET/chithi</a>
            </dd>
            <dt>License</dt>
            <dd>
              <a
                href="#"
                data-testid="about-license-link"
                @click.prevent="openExternal(LICENSE_URL)"
              >GPL-3.0</a>
            </dd>
          </dl>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.about-overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.45);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 10000;
}

.about-modal {
  width: 380px;
  max-width: calc(100vw - 32px);
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 8px;
  box-shadow: 0 8px 32px rgba(0, 0, 0, 0.3);
  color: var(--color-text);
}

.about-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 14px 18px;
  border-bottom: 1px solid var(--color-border);
}

.about-header h2 {
  margin: 0;
  font-size: 16px;
  font-weight: 600;
}

.about-close {
  background: none;
  border: none;
  color: var(--color-text-muted);
  font-size: 22px;
  line-height: 1;
  cursor: pointer;
  padding: 0 6px;
}

.about-close:hover {
  color: var(--color-text);
}

.about-body {
  padding: 18px;
}

.about-tagline {
  margin: 0 0 14px;
  font-size: 13px;
  color: var(--color-text-secondary);
}

.about-meta {
  display: grid;
  grid-template-columns: 84px 1fr;
  row-gap: 6px;
  column-gap: 12px;
  margin: 0;
  font-size: 13px;
}

.about-meta dt {
  color: var(--color-text-muted);
  font-weight: 500;
}

.about-meta dd {
  margin: 0;
}

.about-meta a {
  color: var(--color-accent);
  text-decoration: none;
  cursor: pointer;
}

.about-meta a:hover {
  text-decoration: underline;
}
</style>
