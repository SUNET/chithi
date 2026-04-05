<script setup lang="ts">
import { useMessagesStore } from "@/stores/messages";
import { openComposeWindow } from "@/lib/compose-window";

const messagesStore = useMessagesStore();
</script>

<template>
  <div class="toolbar">
    <button class="compose-btn" title="Compose new email" @click="openComposeWindow()">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M12 20h9M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" />
      </svg>
      Compose
    </button>
    <span v-if="messagesStore.selectedIds.length > 1" class="selection-count">
      {{ messagesStore.selectedIds.length }} selected
    </span>
  </div>
</template>

<style scoped>
.toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 12px;
  background: var(--color-bg);
  border-bottom: 1px solid var(--color-border);
  flex-shrink: 0;
}

.compose-btn {
  display: flex;
  align-items: center;
  gap: 6px;
  background: var(--color-accent);
  color: white;
  font-weight: 500;
  font-size: 13px;
  border-radius: 18px;
  padding: 6px 16px;
  transition: all 0.15s;
}

.compose-btn:hover {
  background: var(--color-accent-hover);
}

.selection-count {
  font-size: 11px;
  font-weight: 500;
  color: var(--color-text-muted);
}
</style>
