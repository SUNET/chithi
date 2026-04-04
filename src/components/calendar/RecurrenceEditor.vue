<script setup lang="ts">
import { ref, watch } from "vue";

const props = defineProps<{
  modelValue: string | null;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: string | null];
}>();

const enabled = ref(!!props.modelValue);
const freq = ref("WEEKLY");
const interval = ref(1);
const endType = ref<"never" | "count" | "until">("never");
const count = ref(10);
const untilDate = ref("");
const byDays = ref<string[]>([]);

const dayOptions = [
  { label: "Mon", value: "MO" },
  { label: "Tue", value: "TU" },
  { label: "Wed", value: "WE" },
  { label: "Thu", value: "TH" },
  { label: "Fri", value: "FR" },
  { label: "Sat", value: "SA" },
  { label: "Sun", value: "SU" },
];

// Parse existing RRULE
if (props.modelValue) {
  const parts: Record<string, string> = {};
  for (const p of props.modelValue.split(";")) {
    const [k, v] = p.split("=");
    if (k && v) parts[k] = v;
  }
  if (parts.FREQ) freq.value = parts.FREQ;
  if (parts.INTERVAL) interval.value = parseInt(parts.INTERVAL, 10);
  if (parts.COUNT) { endType.value = "count"; count.value = parseInt(parts.COUNT, 10); }
  if (parts.UNTIL) { endType.value = "until"; untilDate.value = parts.UNTIL.slice(0, 8).replace(/(\d{4})(\d{2})(\d{2})/, "$1-$2-$3"); }
  if (parts.BYDAY) byDays.value = parts.BYDAY.split(",");
}

function buildRRule(): string | null {
  if (!enabled.value) return null;
  let rule = `FREQ=${freq.value}`;
  if (interval.value > 1) rule += `;INTERVAL=${interval.value}`;
  if (freq.value === "WEEKLY" && byDays.value.length > 0) {
    rule += `;BYDAY=${byDays.value.join(",")}`;
  }
  if (endType.value === "count") rule += `;COUNT=${count.value}`;
  if (endType.value === "until" && untilDate.value) {
    rule += `;UNTIL=${untilDate.value.replace(/-/g, "")}T235959Z`;
  }
  return rule;
}

function update() {
  emit("update:modelValue", buildRRule());
}

watch([enabled, freq, interval, endType, count, untilDate, byDays], update, { deep: true });
</script>

<template>
  <div class="recurrence-editor">
    <label class="toggle-label">
      <input type="checkbox" v-model="enabled" />
      Repeat
    </label>

    <template v-if="enabled">
      <div class="recurrence-row">
        <label>Every</label>
        <input type="number" v-model.number="interval" min="1" max="99" class="num-input" />
        <select v-model="freq">
          <option value="DAILY">day(s)</option>
          <option value="WEEKLY">week(s)</option>
          <option value="MONTHLY">month(s)</option>
          <option value="YEARLY">year(s)</option>
        </select>
      </div>

      <div v-if="freq === 'WEEKLY'" class="day-picker">
        <label
          v-for="d in dayOptions"
          :key="d.value"
          class="day-chip"
          :class="{ active: byDays.includes(d.value) }"
        >
          <input
            type="checkbox"
            :value="d.value"
            v-model="byDays"
            class="hidden"
          />
          {{ d.label }}
        </label>
      </div>

      <div class="recurrence-row">
        <label>Ends</label>
        <select v-model="endType">
          <option value="never">Never</option>
          <option value="count">After N times</option>
          <option value="until">On date</option>
        </select>
        <input
          v-if="endType === 'count'"
          type="number"
          v-model.number="count"
          min="1"
          class="num-input"
        />
        <input
          v-if="endType === 'until'"
          type="date"
          v-model="untilDate"
          class="date-input"
        />
      </div>
    </template>
  </div>
</template>

<style scoped>
.recurrence-editor {
  padding: 8px 0;
}

.toggle-label {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
  cursor: pointer;
  margin-bottom: 8px;
}

.recurrence-row {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 8px;
  font-size: 12px;
}

.recurrence-row label {
  color: var(--color-text-secondary);
  min-width: 40px;
}

.recurrence-row select,
.num-input,
.date-input {
  padding: 4px 6px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  background: var(--color-bg);
  font-size: 12px;
}

.num-input {
  width: 60px;
}

.day-picker {
  display: flex;
  gap: 4px;
  margin-bottom: 8px;
}

.day-chip {
  padding: 4px 8px;
  border: 1px solid var(--color-border);
  border-radius: 4px;
  font-size: 11px;
  cursor: pointer;
  user-select: none;
}

.day-chip.active {
  background: var(--color-accent);
  color: var(--color-bg);
  border-color: var(--color-accent);
}

.hidden {
  display: none;
}
</style>
