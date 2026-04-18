<script setup lang="ts">
// A format-aware time input used in place of `<input type="time">` where
// the display needs to follow the user's time-format preference (#57).
//
// WebKitGTK — and Chromium generally — ignore the `lang` attribute on
// native time inputs; the picker and inline display are locked to the
// OS/browser locale. This component keeps a text-input surface, renders
// the value in the preferred format (12h / 24h / auto), and accepts
// either form on input (e.g. "14:30", "2:30 PM", "2 pm", "13"). The
// v-model value is always a 24-hour "HH:MM" string so existing callers
// and `localInputToUTC` logic don't need to change.

import { computed, ref, watch } from "vue";
import { useUiStore } from "@/stores/ui";

const props = defineProps<{
  modelValue: string; // canonical 24h "HH:MM"
  min?: string; // 24h "HH:MM"
  testid?: string;
}>();
const emit = defineEmits<{
  "update:modelValue": [value: string];
}>();

const uiStore = useUiStore();
const displayValue = ref(toDisplay(props.modelValue));
const invalid = ref(false);

watch(
  () => props.modelValue,
  (v) => {
    displayValue.value = toDisplay(v);
    invalid.value = false;
  },
);

watch(
  () => uiStore.timeFormat,
  () => {
    displayValue.value = toDisplay(props.modelValue);
  },
);

function toDisplay(hhmm: string): string {
  if (!hhmm) return "";
  const parts = hhmm.split(":");
  if (parts.length < 2) return hhmm;
  const h = parseInt(parts[0], 10);
  const m = parseInt(parts[1], 10);
  if (isNaN(h) || isNaN(m)) return hhmm;
  const d = new Date();
  d.setHours(h, m, 0, 0);
  return d.toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
    hour12: uiStore.hour12,
  });
}

// Accept: "14", "14:30", "2:30", "2:30 PM", "2 pm", "02:30am".
function parse(raw: string): string | null {
  const match = raw.trim().match(/^(\d{1,2})(?::(\d{1,2}))?\s*(am|pm)?$/i);
  if (!match) return null;
  let hour = parseInt(match[1], 10);
  const minute = match[2] ? parseInt(match[2], 10) : 0;
  const period = match[3]?.toLowerCase();
  if (period === "pm" && hour < 12) hour += 12;
  if (period === "am" && hour === 12) hour = 0;
  if (hour < 0 || hour > 23 || minute < 0 || minute > 59) return null;
  return `${String(hour).padStart(2, "0")}:${String(minute).padStart(2, "0")}`;
}

function belowMin(value: string): boolean {
  if (!props.min) return false;
  return value < props.min;
}

function onInput(e: Event) {
  displayValue.value = (e.target as HTMLInputElement).value;
  // Clear the invalid flag as soon as the user is typing again; the real
  // check happens on blur. Leaving the red outline on while editing is
  // distracting.
  invalid.value = false;
}

function onBlur() {
  if (displayValue.value.trim() === "") {
    // Empty value: keep whatever the parent had; nothing to emit.
    displayValue.value = toDisplay(props.modelValue);
    invalid.value = false;
    return;
  }
  const parsed = parse(displayValue.value);
  if (parsed === null || belowMin(parsed)) {
    // Restore the last known-good display so the field doesn't get stuck
    // in an invalid state — matches the behavior documented at the top
    // of the file and in the PR description. Flash the red outline
    // briefly so the user sees why the value didn't take.
    invalid.value = true;
    displayValue.value = toDisplay(props.modelValue);
    return;
  }
  invalid.value = false;
  emit("update:modelValue", parsed);
  displayValue.value = toDisplay(parsed);
}

const placeholder = computed(() => {
  if (uiStore.hour12 === false) return "HH:MM";
  if (uiStore.hour12 === true) return "h:mm AM";
  return "";
});
</script>

<template>
  <input
    type="text"
    class="time-input-text"
    :class="{ invalid }"
    :value="displayValue"
    :placeholder="placeholder"
    :data-testid="testid"
    inputmode="numeric"
    autocomplete="off"
    spellcheck="false"
    @input="onInput"
    @blur="onBlur"
    @keydown.enter.prevent="onBlur"
  />
</template>

<style scoped>
/* The parent form styles sizing by setting these custom properties on the
   enclosing element (see EventForm .form-group, EventDetail .edit-group).
   The defaults here cover standalone usage / the compact form in
   RecurrenceEditor. */
.time-input-text {
  width: 100%;
  box-sizing: border-box;
  height: var(--input-height, 28px);
  padding: var(--input-padding, 4px 8px);
  border: var(--input-border, 1px solid var(--color-border));
  border-radius: 4px;
  background: var(--input-bg, var(--color-bg));
  color: var(--color-text);
  font-size: var(--input-font-size, 13px);
  font-variant-numeric: tabular-nums;
}

.time-input-text.invalid {
  border-color: var(--color-danger, #c0392b);
  outline: 1px solid var(--color-danger, #c0392b);
}
</style>
