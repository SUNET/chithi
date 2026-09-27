<script setup lang="ts">
import type { ArchivedGraphCalendar } from "@/lib/types";
import { useAccountsStore } from "@/stores/accounts";

defineProps<{ calendars: ArchivedGraphCalendar[] }>();
const accounts = useAccountsStore();

function accountName(accountId: string): string {
  return accounts.accounts.find((account) => account.id === accountId)?.display_name
    ?? accountId;
}
</script>

<template>
  <ul class="archived-graph-summary">
    <li v-for="calendar in calendars" :key="calendar.id">
      {{ accountName(calendar.account_id) }} · {{ calendar.name }}:
      {{ calendar.retained_event_count }} cached event(s),
      {{ calendar.replay_address_count }} replay address(es)
      <span v-if="calendar.acknowledged">· Acknowledged</span>
    </li>
  </ul>
</template>

<style scoped>
.archived-graph-summary {
  margin: 4px 0;
  padding-left: 20px;
}
</style>
