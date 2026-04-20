<script setup lang="ts">
import { onMounted, onBeforeUnmount } from "vue";
import { useUiStore } from "@/stores/ui";

const uiStore = useUiStore();

function close() {
  uiStore.closeCompose();
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === "Escape") {
    close();
  }
}

onMounted(() => {
  window.addEventListener("keydown", onKeydown);
});

onBeforeUnmount(() => {
  window.removeEventListener("keydown", onKeydown);
});
</script>

<template>
  <div class="compose-sheet-root" role="dialog" aria-label="New message">
    <div class="scrim" @click="close" />
    <section class="sheet">
      <div class="grabber" aria-hidden="true" />
      <header class="sheet-header">
        <button class="sheet-action muted" @click="close">Cancel</button>
        <div class="sheet-title">New Message</div>
        <button class="sheet-action send" disabled>Send</button>
      </header>

      <div class="sheet-body">
        <!-- Scaffold only — full Compose sheet rebuild is tracked for
             the mobile iteration pass (§8 of PATCHES-MOBILE.md). The
             desktop path still uses openComposeWindow(). -->
        <p class="placeholder">
          Mobile compose sheet placeholder — full implementation tracked in
          PATCHES-MOBILE §8.
        </p>
      </div>
    </section>
  </div>
</template>

<style scoped>
.compose-sheet-root {
  position: fixed;
  inset: 0;
  z-index: 60;
  display: flex;
  flex-direction: column;
  justify-content: flex-end;
}

.scrim {
  position: absolute;
  inset: 0;
  background: rgba(20, 14, 6, 0.4);
}

.sheet {
  position: relative;
  margin-top: auto;
  height: calc(100vh - 64px);
  background: var(--color-bg);
  border-top-left-radius: var(--radius-sheet);
  border-top-right-radius: var(--radius-sheet);
  box-shadow: var(--shadow-sheet);
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.grabber {
  width: 38px;
  height: 5px;
  border-radius: 100px;
  background: var(--color-border);
  margin: 8px auto 4px;
}

.sheet-header {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 12px 10px;
  border-bottom: 1px solid var(--color-divider, #e9e0cd);
}

.sheet-title {
  flex: 1;
  text-align: center;
  font-size: 15px;
  font-weight: 600;
  color: var(--color-text);
}

.sheet-action {
  border: 0;
  background: transparent;
  font-family: inherit;
  font-size: 15px;
  padding: 8px 6px;
  cursor: pointer;
}

.sheet-action.muted {
  color: var(--color-text-muted);
}

.sheet-action.send {
  background: var(--color-accent);
  color: #fff;
  padding: 8px 14px;
  border-radius: 100px;
  font-weight: 600;
}

.sheet-action.send:disabled {
  opacity: 0.55;
  cursor: not-allowed;
}

.sheet-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 16px;
}

.placeholder {
  color: var(--color-text-muted);
  font-size: 14px;
  line-height: 1.5;
}
</style>
