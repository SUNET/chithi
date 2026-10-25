<script setup lang="ts">
defineOptions({ inheritAttrs: false });
defineProps<{ open: boolean; title: string; modalClass?: string }>();
const emit = defineEmits<{ close: [] }>();
</script>

<template>
  <Teleport to="body">
    <div v-if="open" class="modal-overlay" @click.self="emit('close')">
      <div class="modal" :class="modalClass" v-bind="$attrs">
        <div class="modal-header">
          <h3>{{ title }}</h3>
          <button class="modal-close" @click="emit('close')">&times;</button>
        </div>
        <div class="modal-body">
          <slot />
        </div>
        <div v-if="$slots.footer" class="modal-footer">
          <slot name="footer" />
        </div>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.modal-overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.2);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 1000;
}

.modal {
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 12px;
  width: 480px;
  max-height: 85vh;
  overflow-y: auto;
  box-shadow: 0 8px 32px rgba(0, 0, 0, 0.12);
}

.modal-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 16px 20px;
  border-bottom: 1px solid var(--color-border);
}

.modal-header h3 {
  font-size: 16px;
  font-weight: 600;
}

.modal-close {
  font-size: 20px;
  color: var(--color-text-muted);
  width: 28px;
  height: 28px;
  border-radius: 6px;
  display: grid;
  place-items: center;
}

.modal-close:hover {
  background: var(--color-bg-hover);
  color: var(--color-text);
}

.modal-body {
  padding: 20px;
}

.modal-footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 12px 20px;
  border-top: 1px solid var(--color-border);
}

@media (max-width: 720px) {
  .modal-overlay {
    align-items: flex-end;
    background: rgba(20, 14, 6, 0.4);
  }

  .modal {
    width: 100%;
    max-width: 100%;
    height: calc(100vh - 48px);
    max-height: calc(100vh - 48px);
    border-radius: var(--radius-sheet, 16px) var(--radius-sheet, 16px) 0 0;
    box-shadow: var(--shadow-sheet, 0 -12px 30px rgba(30, 20, 10, 0.18));
    position: relative;
  }

  .modal::before {
    content: "";
    display: block;
    width: 38px;
    height: 5px;
    border-radius: 100px;
    background: var(--color-border);
    margin: 8px auto 4px;
  }

  .modal-header {
    justify-content: center;
    padding: 4px 16px 10px;
  }

  .modal-header h3 {
    font-size: 15px;
    flex: 1;
    text-align: center;
  }

  .modal-close {
    position: absolute;
    top: 16px;
    right: 12px;
  }
}
</style>
