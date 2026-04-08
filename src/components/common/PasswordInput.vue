<script setup lang="ts">
import { ref } from "vue";

defineProps<{
  modelValue: string;
  placeholder?: string;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: string];
}>();

const isVisible = ref(false);

function toggleVisibility() {
  isVisible.value = !isVisible.value;
}

function onBlur() {
  isVisible.value = false;
}

function onInput(event: Event) {
  emit("update:modelValue", (event.target as HTMLInputElement).value);
}
</script>

<template>
  <div class="password-input">
    <input
      :type="isVisible ? 'text' : 'password'"
      :value="modelValue"
      :placeholder="placeholder || '••••••••'"
      autocomplete="off"
      data-form-type="other"
      data-lpignore="true"
      autocorrect="off"
      autocapitalize="off"
      spellcheck="false"
      @input="onInput"
      @blur="onBlur"
    />
    <button
      type="button"
      class="eye-btn"
      tabindex="-1"
      @click="toggleVisibility"
    >
      <svg v-if="isVisible" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/>
        <circle cx="12" cy="12" r="3"/>
      </svg>
      <svg v-else width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19m-6.72-1.07a3 3 0 1 1-4.24-4.24"/>
        <line x1="1" y1="1" x2="23" y2="23"/>
      </svg>
    </button>
  </div>
</template>

<style scoped>
.password-input {
  position: relative;
}

.password-input input {
  width: 100%;
  height: 40px;
  padding: 0 40px 0 12px;
  border: 0.8px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg-secondary);
  font-family: var(--font-sans);
  font-size: 16px;
  box-sizing: border-box;
  color: var(--color-text);
}

.password-input input:focus {
  outline: none;
  border-color: var(--color-accent);
  box-shadow: 0 0 0 2px var(--color-accent-light);
}

.eye-btn {
  position: absolute;
  right: 8px;
  top: 50%;
  transform: translateY(-50%);
  background: none;
  border: none;
  cursor: pointer;
  padding: 4px;
  display: flex;
  align-items: center;
  justify-content: center;
  border-radius: 4px;
}

.eye-btn:hover {
  background: var(--color-bg-hover);
}

.eye-btn svg {
  color: var(--color-text-muted);
}
</style>
