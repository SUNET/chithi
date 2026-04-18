<script setup lang="ts">
defineProps<{
  ariaLabel?: string;
  disabled?: boolean;
}>();
</script>

<template>
  <button
    type="button"
    class="mobile-icon-button"
    :aria-label="ariaLabel"
    :disabled="disabled"
  >
    <slot />
  </button>
</template>

<style scoped>
.mobile-icon-button {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: var(--touch-min);
  height: var(--touch-min);
  border: 0;
  background: transparent;
  color: var(--color-text);
  border-radius: 10px;
  flex-shrink: 0;
  cursor: pointer;
}

.mobile-icon-button:disabled {
  opacity: 0.4;
  cursor: not-allowed;
}

.mobile-icon-button:active:not(:disabled) {
  background: var(--color-bg-hover);
}

.mobile-icon-button :deep(svg) {
  width: 22px;
  height: 22px;
  stroke-width: 1.8;
}
</style>
