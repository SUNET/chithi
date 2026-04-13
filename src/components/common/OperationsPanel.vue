<script setup lang="ts">
import { useActivityStore } from "@/stores/activity";
import { useUiStore } from "@/stores/ui";

const activityStore = useActivityStore();
const uiStore = useUiStore();

function statusIcon(status: string): string {
  switch (status) {
    case "running":
      return "spinner";
    case "done":
      return "check";
    case "error":
      return "error";
    default:
      return "info";
  }
}
</script>

<template>
  <Transition name="panel-slide">
    <div v-if="uiStore.operationsPanelOpen" class="operations-panel">
      <div class="panel-header">
        <span class="panel-title">Operations</span>
        <button class="panel-close" @click="uiStore.toggleOperationsPanel()" title="Close">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" />
          </svg>
        </button>
      </div>
      <div class="panel-body">
        <div
          v-if="activityStore.recentOperations.length === 0"
          class="panel-empty"
        >
          No recent operations
        </div>
        <div
          v-for="op in activityStore.recentOperations"
          :key="op.id"
          class="op-row"
          :class="op.status"
        >
          <span class="op-icon" :class="statusIcon(op.status)">
            <!-- Spinner -->
            <svg v-if="op.status === 'running'" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="spin">
              <path d="M21 12a9 9 0 1 1-6.22-8.56" />
            </svg>
            <!-- Check -->
            <svg v-else-if="op.status === 'done'" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <polyline points="20 6 9 17 4 12" />
            </svg>
            <!-- Error -->
            <svg v-else width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <circle cx="12" cy="12" r="10" /><line x1="15" y1="9" x2="9" y2="15" /><line x1="9" y1="9" x2="15" y2="15" />
            </svg>
          </span>
          <div class="op-content">
            <span class="op-label">{{ op.label }}</span>
            <span class="op-detail">{{ op.detail }}</span>
          </div>
          <span class="op-type-badge">{{ op.type }}</span>
        </div>
      </div>
    </div>
  </Transition>
</template>

<style scoped>
.operations-panel {
  background: var(--color-bg);
  border-top: 1px solid var(--color-border);
  max-height: 40vh;
  display: flex;
  flex-direction: column;
  overflow: hidden;
  flex-shrink: 0;
}

.panel-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 6px 12px;
  border-bottom: 1px solid var(--color-border);
  flex-shrink: 0;
}

.panel-title {
  font-size: 12px;
  font-weight: 600;
  color: var(--color-text);
}

.panel-close {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  border-radius: 4px;
  color: var(--color-text-muted);
  transition: all 0.12s;
}

.panel-close:hover {
  color: var(--color-text);
  background: var(--color-bg-hover);
}

.panel-body {
  overflow-y: auto;
  padding: 4px 0;
}

.panel-empty {
  padding: 16px 12px;
  text-align: center;
  font-size: 12px;
  color: var(--color-text-muted);
}

.op-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 12px;
  font-size: 12px;
  transition: background 0.1s;
}

.op-row:hover {
  background: var(--color-bg-hover);
}

.op-icon {
  flex-shrink: 0;
  display: flex;
  align-items: center;
}

.op-icon.check {
  color: #059669;
}

.op-icon.error {
  color: var(--color-danger);
}

.op-icon.spinner {
  color: var(--color-accent);
}

.op-icon .spin {
  animation: spin 1s linear infinite;
}

@keyframes spin {
  from { transform: rotate(0deg); }
  to { transform: rotate(360deg); }
}

.op-content {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 1px;
}

.op-label {
  color: var(--color-text);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.op-detail {
  color: var(--color-text-muted);
  font-size: 11px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.op-row.error .op-detail {
  color: var(--color-danger-text, var(--color-danger));
}

.op-type-badge {
  flex-shrink: 0;
  font-size: 10px;
  padding: 1px 6px;
  border-radius: 4px;
  background: var(--color-bg-hover);
  color: var(--color-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

/* Slide-up transition */
.panel-slide-enter-active,
.panel-slide-leave-active {
  transition: max-height 0.2s ease, opacity 0.2s ease;
}

.panel-slide-enter-from,
.panel-slide-leave-to {
  max-height: 0;
  opacity: 0;
}

.panel-slide-enter-to,
.panel-slide-leave-from {
  max-height: 40vh;
  opacity: 1;
}
</style>
