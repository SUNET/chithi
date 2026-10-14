<script setup lang="ts">
import type { ArchivedGraphCalendar } from "@/lib/types";
import ArchivedGraphSummary from "./ArchivedGraphSummary.vue";

defineProps<{
  calendars: ArchivedGraphCalendar[];
  error: string | null;
  acknowledgementError: string | null;
  acknowledging: boolean;
}>();
defineEmits<{ retry: []; acknowledge: [] }>();
</script>

<template>
  <div v-if="calendars.length || error || acknowledgementError" class="archived-graph-notice" role="alert">
    <span v-if="error">
      {{ error }} <button @click="$emit('retry')">Retry</button>
    </span>
    <span v-if="acknowledgementError">{{ acknowledgementError }}</span>
    <div v-if="calendars.length">
      {{ calendars.length }} Graph calendar(s) no longer reported by the provider
      were archived. Their cached events and action history are preserved
      read-only, not shown in the live calendar.
      <button :disabled="acknowledging" @click="$emit('acknowledge')">
        {{ acknowledging ? 'Acknowledging…' : 'Acknowledge' }}
      </button>
      <details>
        <summary>Archived calendar summary</summary>
        <ArchivedGraphSummary :calendars="calendars" />
      </details>
    </div>
  </div>
</template>

<style scoped>
.archived-graph-notice {
  flex-shrink: 0;
  padding: 6px 14px;
  font-size: 12px;
  background: var(--color-bg-secondary);
  color: var(--color-text);
}

button, summary {
  color: var(--color-accent);
  cursor: pointer;
}

button {
  margin-left: 8px;
  text-decoration: underline;
}

button:disabled {
  opacity: 0.6;
  cursor: wait;
}
</style>
