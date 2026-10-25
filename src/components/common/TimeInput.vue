<script setup lang="ts">
// Replaces native time inputs that ignore the app's 12/24-hour preference.
// The model remains a canonical 24-hour HH:MM string.

import { computed, ref, watch } from "vue";
import { useUiStore } from "@/stores/ui";

const props = defineProps<{
  min?: string;
  testid?: string;
}>();
const model = defineModel<string>({ required: true });

const uiStore = useUiStore();
const displayValue = ref(toDisplay(model.value));
const invalid = ref(false);

watch(
  model,
  (value) => {
    displayValue.value = toDisplay(value);
    invalid.value = false;
  },
);

watch(
  () => uiStore.timeFormat,
  () => {
    displayValue.value = toDisplay(model.value);
  },
);

function toDisplay(hhmm: string): string {
  if (!hhmm) return "";
  const parts = hhmm.split(":");
  if (parts.length < 2) return hhmm;
  const h = Number.parseInt(parts[0], 10);
  const m = Number.parseInt(parts[1], 10);
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
  let hour = Number.parseInt(match[1], 10);
  const minute = match[2] ? Number.parseInt(match[2], 10) : 0;
  const period = match[3]?.toLowerCase();
  if (period === "pm" && hour < 12) hour += 12;
  if (period === "am" && hour === 12) hour = 0;
  if (hour > 23 || minute > 59) return null;
  return `${String(hour).padStart(2, "0")}:${String(minute).padStart(2, "0")}`;
}

function onBlur() {
  if (displayValue.value.trim() === "") {
    displayValue.value = toDisplay(model.value);
    invalid.value = false;
    return;
  }
  const parsed = parse(displayValue.value);
  if (parsed === null || (props.min !== undefined && parsed < props.min)) {
    // Signal rejection while restoring the last valid value.
    invalid.value = true;
    displayValue.value = toDisplay(model.value);
    return;
  }
  invalid.value = false;
  model.value = parsed;
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
    v-model="displayValue"
    :placeholder="placeholder"
    :data-testid="testid"
    inputmode="numeric"
    autocomplete="off"
    spellcheck="false"
    @input="invalid = false"
    @blur="onBlur"
    @keydown.enter.prevent="onBlur"
  />
</template>

<style scoped>
/* Parent forms override these sizing tokens. */
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
