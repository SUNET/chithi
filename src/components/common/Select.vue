<script setup lang="ts">
// Theme-aware dropdown used in place of native <select>. WebKitGTK renders
// the expanded popup with its own chrome (ignoring our CSS tokens), so the
// dropdown looks out of place in both the dark and light themes. This
// component replaces the native picker with an in-DOM popup that uses the
// same CSS variables as DateInput / TimeInput.
//
// The v-model contract stays a single primitive value (string / number /
// boolean) and the component accepts an `options` array of { value, label }.
// Options can be disabled individually.

import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";

export interface SelectOption<T> {
  value: T;
  label: string;
  disabled?: boolean;
}

// Using `any` on the prop type so each call site can constrain the value
// type via its template binding without the parent having to type-narrow
// the whole component generic (Vue 3.5 supports generics on defineProps
// but they're still fiddly for v-model with primitives).
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type SelectValue = any;

const props = defineProps<{
  modelValue: SelectValue;
  options: SelectOption<SelectValue>[];
  placeholder?: string;
  testid?: string;
  ariaLabel?: string;
}>();
const emit = defineEmits<{
  "update:modelValue": [value: SelectValue];
}>();

const rootEl = ref<HTMLElement | null>(null);
const listEl = ref<HTMLElement | null>(null);
const open = ref(false);
const highlight = ref(-1);

const selectedOption = computed(() =>
  props.options.find((o) => o.value === props.modelValue),
);

const selectedLabel = computed(
  () => selectedOption.value?.label ?? props.placeholder ?? "",
);

function openMenu() {
  const idx = props.options.findIndex(
    (o) => o.value === props.modelValue && !o.disabled,
  );
  highlight.value = idx >= 0 ? idx : firstEnabledIndex();
  open.value = true;
  nextTick(() => scrollHighlightedIntoView());
}

function closeMenu() {
  open.value = false;
}

function toggleMenu() {
  if (open.value) closeMenu();
  else openMenu();
}

function firstEnabledIndex(): number {
  for (let i = 0; i < props.options.length; i++) {
    if (!props.options[i].disabled) return i;
  }
  return -1;
}

function lastEnabledIndex(): number {
  for (let i = props.options.length - 1; i >= 0; i--) {
    if (!props.options[i].disabled) return i;
  }
  return -1;
}

function moveHighlight(delta: 1 | -1) {
  if (props.options.length === 0) return;
  let i = highlight.value;
  for (let step = 0; step < props.options.length; step++) {
    i = (i + delta + props.options.length) % props.options.length;
    if (!props.options[i].disabled) {
      highlight.value = i;
      scrollHighlightedIntoView();
      return;
    }
  }
}

function scrollHighlightedIntoView() {
  if (!listEl.value || highlight.value < 0) return;
  const el = listEl.value.children[highlight.value] as HTMLElement | undefined;
  el?.scrollIntoView({ block: "nearest" });
}

function commit(idx: number) {
  const opt = props.options[idx];
  if (!opt || opt.disabled) return;
  emit("update:modelValue", opt.value);
  closeMenu();
}

function onTriggerKeydown(e: KeyboardEvent) {
  if (!open.value) {
    if (e.key === "Enter" || e.key === " " || e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      openMenu();
    }
    return;
  }
  if (e.key === "ArrowDown") {
    e.preventDefault();
    moveHighlight(1);
  } else if (e.key === "ArrowUp") {
    e.preventDefault();
    moveHighlight(-1);
  } else if (e.key === "Home") {
    e.preventDefault();
    highlight.value = firstEnabledIndex();
    scrollHighlightedIntoView();
  } else if (e.key === "End") {
    e.preventDefault();
    highlight.value = lastEnabledIndex();
    scrollHighlightedIntoView();
  } else if (e.key === "Enter" || e.key === " ") {
    e.preventDefault();
    if (highlight.value >= 0) commit(highlight.value);
  } else if (e.key === "Escape") {
    e.preventDefault();
    e.stopPropagation();
    closeMenu();
  } else if (e.key === "Tab") {
    closeMenu();
  }
}

function onOptionMouseEnter(idx: number) {
  if (!props.options[idx].disabled) highlight.value = idx;
}

function onOptionClick(idx: number) {
  commit(idx);
}

function onDocMouseDown(e: MouseEvent) {
  if (!open.value) return;
  if (rootEl.value && !rootEl.value.contains(e.target as Node)) {
    closeMenu();
  }
}

// When parent updates the model while open, move the highlight to match.
watch(
  () => props.modelValue,
  () => {
    if (!open.value) return;
    const idx = props.options.findIndex((o) => o.value === props.modelValue);
    if (idx >= 0) highlight.value = idx;
  },
);

onMounted(() => {
  document.addEventListener("mousedown", onDocMouseDown);
});
onUnmounted(() => {
  document.removeEventListener("mousedown", onDocMouseDown);
});
</script>

<template>
  <div ref="rootEl" class="select-wrap">
    <button
      type="button"
      class="select-trigger"
      :aria-haspopup="'listbox'"
      :aria-expanded="open"
      :aria-label="ariaLabel"
      :data-testid="testid"
      @click="toggleMenu"
      @keydown="onTriggerKeydown"
    >
      <span class="select-label" :class="{ 'is-placeholder': !selectedOption }">{{ selectedLabel }}</span>
      <svg
        class="select-caret"
        width="12"
        height="12"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
        stroke-linecap="round"
        stroke-linejoin="round"
        aria-hidden="true"
      >
        <polyline points="6 9 12 15 18 9" />
      </svg>
    </button>
    <ul
      v-if="open"
      ref="listEl"
      class="select-menu"
      role="listbox"
      :aria-activedescendant="highlight >= 0 ? `${testid ?? 'select'}-opt-${highlight}` : undefined"
    >
      <li
        v-for="(opt, idx) in options"
        :id="`${testid ?? 'select'}-opt-${idx}`"
        :key="String(opt.value)"
        role="option"
        class="select-option"
        :class="{
          selected: opt.value === modelValue,
          highlighted: idx === highlight,
          disabled: opt.disabled,
        }"
        :aria-selected="opt.value === modelValue"
        :aria-disabled="opt.disabled"
        @mousedown.prevent="onOptionClick(idx)"
        @mouseenter="onOptionMouseEnter(idx)"
      >
        {{ opt.label }}
      </li>
    </ul>
  </div>
</template>

<style scoped>
.select-wrap {
  position: relative;
  display: inline-block;
  width: 100%;
}

/* Sizing tokens inherited from the nearest enclosing parent (see
   EventForm .form-group / EventDetail .edit-group / RecurrenceEditor
   .recurrence-row). Defaults cover compact / standalone use. */
.select-trigger {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  box-sizing: border-box;
  height: var(--input-height, 28px);
  padding: var(--input-padding, 4px 8px);
  background: var(--input-bg, var(--color-bg));
  border: var(--input-border, 1px solid var(--color-border));
  border-radius: 4px;
  color: var(--color-text);
  cursor: pointer;
  font-size: var(--input-font-size, 13px);
  text-align: left;
  transition: background 0.1s, border-color 0.1s;
}

.select-trigger:hover {
  background: var(--color-bg-hover);
}

.select-trigger:focus-visible {
  outline: none;
  border-color: var(--color-accent);
}

.select-label {
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.select-label.is-placeholder {
  color: var(--color-text-muted);
}

.select-caret {
  color: var(--color-text-muted);
  flex-shrink: 0;
}

.select-menu {
  position: absolute;
  top: calc(100% + 4px);
  left: 0;
  right: 0;
  z-index: 1000;
  max-height: 280px;
  overflow-y: auto;
  margin: 0;
  padding: 4px;
  list-style: none;
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 6px;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.25);
}

.select-option {
  padding: 6px 10px;
  border-radius: 4px;
  color: var(--color-text);
  font-size: var(--input-font-size, 13px);
  cursor: pointer;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.select-option.highlighted:not(.disabled) {
  background: var(--color-bg-hover);
}

.select-option.selected {
  color: var(--color-accent);
  font-weight: 600;
}

.select-option.selected.highlighted {
  background: var(--color-bg-hover);
}

.select-option.disabled {
  color: var(--color-text-muted);
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
