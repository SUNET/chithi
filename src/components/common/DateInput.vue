<script setup lang="ts">
// Theme-aware date input used in place of `<input type="date">`. WebKitGTK's
// native date picker ignores our CSS variables, can only be dismissed with
// Esc (not by clicking outside), and uses a hostile light background on the
// dark theme. This component renders a trigger button that matches the rest
// of the app's surfaces and opens an in-DOM calendar popup.
//
// The v-model contract is the same as the native input: a "YYYY-MM-DD"
// string (blank when no date is selected), so callers and existing
// `localInputToUTC` logic don't need to change.

import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { useUiStore } from "@/stores/ui";

const props = defineProps<{
  modelValue: string;
  min?: string;
  testid?: string;
}>();
const emit = defineEmits<{
  "update:modelValue": [value: string];
}>();

const uiStore = useUiStore();
const rootEl = ref<HTMLElement | null>(null);
const open = ref(false);
const viewYear = ref(new Date().getFullYear());
const viewMonth = ref(new Date().getMonth());

function parseYMD(s: string): Date | null {
  const m = s?.match(/^(\d{4})-(\d{2})-(\d{2})$/);
  if (!m) return null;
  return new Date(parseInt(m[1], 10), parseInt(m[2], 10) - 1, parseInt(m[3], 10));
}

function toYMD(d: Date): string {
  const y = d.getFullYear();
  const mo = String(d.getMonth() + 1).padStart(2, "0");
  const da = String(d.getDate()).padStart(2, "0");
  return `${y}-${mo}-${da}`;
}

function openPicker() {
  const d = parseYMD(props.modelValue) ?? new Date();
  viewYear.value = d.getFullYear();
  viewMonth.value = d.getMonth();
  open.value = true;
}

function closePicker() {
  open.value = false;
}

// Keep the calendar's viewed month in sync when the parent replaces the
// value while the popup is open (e.g. watchers that push endDate forward).
watch(
  () => props.modelValue,
  (v) => {
    if (!open.value) return;
    const d = parseYMD(v);
    if (d) {
      viewYear.value = d.getFullYear();
      viewMonth.value = d.getMonth();
    }
  },
);

const weeks = computed<Date[][]>(() => {
  const first = new Date(viewYear.value, viewMonth.value, 1);
  const offset = (first.getDay() - uiStore.weekStartDay + 7) % 7;
  const start = new Date(viewYear.value, viewMonth.value, 1 - offset);
  const result: Date[][] = [];
  for (let w = 0; w < 6; w++) {
    const row: Date[] = [];
    for (let d = 0; d < 7; d++) {
      const day = new Date(start);
      day.setDate(start.getDate() + w * 7 + d);
      row.push(day);
    }
    result.push(row);
  }
  return result;
});

const dayNames = computed(() => {
  const base = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
  const rotated = [...base];
  for (let i = 0; i < uiStore.weekStartDay; i++) rotated.push(rotated.shift()!);
  return rotated;
});

const monthLabel = computed(() => {
  const d = new Date(viewYear.value, viewMonth.value, 1);
  return d.toLocaleDateString(undefined, { month: "long", year: "numeric" });
});

function goPrev() {
  if (viewMonth.value === 0) {
    viewMonth.value = 11;
    viewYear.value--;
  } else {
    viewMonth.value--;
  }
}

function goNext() {
  if (viewMonth.value === 11) {
    viewMonth.value = 0;
    viewYear.value++;
  } else {
    viewMonth.value++;
  }
}

function isToday(d: Date): boolean {
  const now = new Date();
  return (
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate()
  );
}

function isSelected(d: Date): boolean {
  return toYMD(d) === props.modelValue;
}

function isCurrentMonth(d: Date): boolean {
  return d.getMonth() === viewMonth.value;
}

function isDisabled(d: Date): boolean {
  return props.min ? toYMD(d) < props.min : false;
}

function selectDay(d: Date) {
  if (isDisabled(d)) return;
  emit("update:modelValue", toYMD(d));
  closePicker();
}

function onDocMouseDown(e: MouseEvent) {
  if (!open.value) return;
  if (rootEl.value && !rootEl.value.contains(e.target as Node)) {
    closePicker();
  }
}

function onDocKeydown(e: KeyboardEvent) {
  if (open.value && e.key === "Escape") {
    e.stopPropagation();
    closePicker();
  }
}

onMounted(() => {
  document.addEventListener("mousedown", onDocMouseDown);
  document.addEventListener("keydown", onDocKeydown);
});

onUnmounted(() => {
  document.removeEventListener("mousedown", onDocMouseDown);
  document.removeEventListener("keydown", onDocKeydown);
});
</script>

<template>
  <div ref="rootEl" class="date-input-wrap">
    <button
      type="button"
      class="date-input-trigger"
      :data-testid="testid"
      @click="open ? closePicker() : openPicker()"
    >
      <span class="date-value">{{ modelValue || "Select date" }}</span>
      <svg
        class="cal-icon"
        width="14"
        height="14"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
        stroke-linecap="round"
        stroke-linejoin="round"
        aria-hidden="true"
      >
        <rect x="3" y="4" width="18" height="18" rx="2" ry="2" />
        <line x1="16" y1="2" x2="16" y2="6" />
        <line x1="8" y1="2" x2="8" y2="6" />
        <line x1="3" y1="10" x2="21" y2="10" />
      </svg>
    </button>
    <div v-if="open" class="date-picker-popup" role="dialog" aria-label="Choose a date">
      <div class="date-picker-header">
        <button
          type="button"
          class="nav-btn"
          aria-label="Previous month"
          data-testid="date-picker-prev"
          @click="goPrev"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="15 18 9 12 15 6" />
          </svg>
        </button>
        <span class="month-label">{{ monthLabel }}</span>
        <button
          type="button"
          class="nav-btn"
          aria-label="Next month"
          data-testid="date-picker-next"
          @click="goNext"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="9 18 15 12 9 6" />
          </svg>
        </button>
      </div>
      <div class="day-names">
        <span v-for="d in dayNames" :key="d" class="day-name">{{ d }}</span>
      </div>
      <div class="day-grid">
        <template v-for="(week, wi) in weeks" :key="wi">
          <button
            v-for="day in week"
            :key="day.toISOString()"
            type="button"
            class="day-cell"
            :class="{
              today: isToday(day),
              selected: isSelected(day),
              'other-month': !isCurrentMonth(day),
              disabled: isDisabled(day),
            }"
            :disabled="isDisabled(day)"
            :data-testid="`date-picker-day-${toYMD(day)}`"
            @click="selectDay(day)"
          >
            {{ day.getDate() }}
          </button>
        </template>
      </div>
    </div>
  </div>
</template>

<style scoped>
.date-input-wrap {
  position: relative;
  display: inline-block;
  width: 100%;
}

/* Sizing tokens are inherited from the nearest enclosing parent (see
   EventForm .form-group / EventDetail .edit-group / RecurrenceEditor
   .recurrence-row). Defaults cover compact/standalone use. */
.date-input-trigger {
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
  font-variant-numeric: tabular-nums;
  transition: background 0.1s, border-color 0.1s;
}

.date-input-trigger:hover {
  background: var(--color-bg-hover);
}

.date-input-trigger:focus-visible {
  outline: none;
  border-color: var(--color-accent);
}

.date-value {
  flex: 1;
  text-align: left;
}

.cal-icon {
  color: var(--color-text-muted);
  flex-shrink: 0;
}

.date-picker-popup {
  position: absolute;
  top: calc(100% + 4px);
  left: 0;
  z-index: 1000;
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: 6px;
  padding: 10px;
  min-width: 240px;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.25);
}

.date-picker-header {
  display: flex;
  align-items: center;
  gap: 4px;
  margin-bottom: 8px;
}

.month-label {
  flex: 1;
  text-align: center;
  font-weight: 600;
  font-size: 13px;
  color: var(--color-text);
}

.nav-btn {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  border-radius: 4px;
  background: transparent;
  color: var(--color-text);
  cursor: pointer;
  transition: background 0.1s;
}

.nav-btn:hover {
  background: var(--color-bg-hover);
}

.day-names {
  display: grid;
  grid-template-columns: repeat(7, 1fr);
  gap: 2px;
  margin-bottom: 4px;
}

.day-name {
  text-align: center;
  font-size: 10px;
  font-weight: 600;
  color: var(--color-text-muted);
  letter-spacing: 0.5px;
  padding: 2px 0;
}

.day-grid {
  display: grid;
  grid-template-columns: repeat(7, 1fr);
  gap: 2px;
}

.day-cell {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 28px;
  border-radius: 4px;
  background: transparent;
  color: var(--color-text);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  cursor: pointer;
  transition: background 0.1s;
}

.day-cell:hover:not(.disabled):not(.selected) {
  background: var(--color-bg-hover);
}

.day-cell.today {
  font-weight: 700;
  box-shadow: inset 0 0 0 1px var(--color-accent);
}

.day-cell.selected {
  background: var(--color-accent);
  color: #fff;
}

.day-cell.other-month {
  color: var(--color-text-muted);
  opacity: 0.5;
}

.day-cell.disabled {
  opacity: 0.3;
  cursor: not-allowed;
}
</style>
