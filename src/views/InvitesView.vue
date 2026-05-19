<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from "vue";
import { useInvitesStore } from "@/stores/invites";
import type { InviteStatusFilter, InviteSortMode } from "@/stores/invites";
import { normalizeInviteStatus, nextOccurrence } from "@/stores/invites";
import { useAccountsStore } from "@/stores/accounts";
import { useUiStore } from "@/stores/ui";
import { useCalendarStore } from "@/stores/calendar";
import type { Invite } from "@/lib/types";
import { formatInTimezone } from "@/lib/datetime";
import { describeRRule } from "@/lib/rrule";
import { acctColor } from "@/lib/account-colors";
import EventDetail from "@/components/calendar/EventDetail.vue";

const invitesStore = useInvitesStore();
const accountsStore = useAccountsStore();
const uiStore = useUiStore();
const calendarStore = useCalendarStore();

const STATUS_FILTERS: { value: InviteStatusFilter; label: string }[] = [
  { value: "all", label: "All" },
  { value: "needs-action", label: "Needs action" },
  { value: "accepted", label: "Accepted" },
  { value: "tentative", label: "Maybe" },
  { value: "declined", label: "Declined" },
];

const SORT_MODES: { value: InviteSortMode; label: string }[] = [
  { value: "date-asc", label: "Date (soonest first)" },
  { value: "date-desc", label: "Date (latest first)" },
  { value: "received", label: "Recently received" },
];

// Per-row RSVP in-flight + error state, keyed by event id.
const respondingId = ref<string | null>(null);
const errorById = ref<Record<string, string>>({});

const detailOpen = ref(false);

const invites = computed(() => invitesStore.filteredInvites);

function accountLabel(accountId: string): string {
  const acc = accountsStore.accounts.find((a) => a.id === accountId);
  return acc?.email || acc?.display_name || "Unknown account";
}

function statusLabel(invite: Invite): string {
  switch (normalizeInviteStatus(invite.my_status)) {
    case "accepted":
      return "Accepted";
    case "tentative":
      return "Maybe";
    case "declined":
      return "Declined";
    default:
      return "Not replied";
  }
}

function whenLabel(invite: Invite): string {
  const occ = nextOccurrence(invite).toISOString();
  return formatInTimezone(occ, uiStore.displayTimezone, {
    hour12: uiStore.hour12,
  });
}

function recurrenceLabel(invite: Invite): string {
  return invite.recurrence_rule ? describeRRule(invite.recurrence_rule) : "";
}

/** Whether the given response is the invite's current RSVP. */
function isCurrent(invite: Invite, response: string): boolean {
  return normalizeInviteStatus(invite.my_status) === response;
}

async function respond(invite: Invite, response: string) {
  respondingId.value = invite.id;
  delete errorById.value[invite.id];
  try {
    await invitesStore.respond(invite, response);
  } catch (e) {
    errorById.value = { ...errorById.value, [invite.id]: String(e) };
  } finally {
    respondingId.value = null;
  }
}

function openDetail(invite: Invite) {
  calendarStore.selectEvent(invite);
  detailOpen.value = true;
}

function closeDetail() {
  detailOpen.value = false;
  calendarStore.selectEvent(null);
  // An edit/delete from the detail panel may have changed the invite.
  invitesStore.fetchInvites().catch(() => {});
}

// Tell the store the view is open so it keeps invites fresh even when the
// sidebar badge preference is disabled; release that tracking on unmount.
onMounted(() => invitesStore.setViewActive(true));
onUnmounted(() => invitesStore.setViewActive(false));
</script>

<template>
  <div class="invites-view">
    <header class="invites-header">
      <h2 class="invites-title">Invites</h2>

      <div class="invites-controls">
        <div class="status-filter" role="tablist" aria-label="Filter invites by reply status">
          <button
            v-for="f in STATUS_FILTERS"
            :key="f.value"
            class="seg-btn"
            :class="{ active: invitesStore.statusFilter === f.value }"
            role="tab"
            :aria-selected="invitesStore.statusFilter === f.value"
            :data-testid="`invites-filter-${f.value}`"
            @click="invitesStore.setStatusFilter(f.value)"
          >
            {{ f.label }}
          </button>
        </div>

        <label class="sort-control">
          <span class="sort-label">Sort</span>
          <select
            :value="invitesStore.sortMode"
            data-testid="invites-sort"
            @change="invitesStore.setSortMode(($event.target as HTMLSelectElement).value as InviteSortMode)"
          >
            <option v-for="s in SORT_MODES" :key="s.value" :value="s.value">
              {{ s.label }}
            </option>
          </select>
        </label>
      </div>
    </header>

    <div class="invites-body">
      <div v-if="invitesStore.loading && invites.length === 0" class="invites-empty">
        Loading invites…
      </div>
      <div v-else-if="invites.length === 0" class="invites-empty" data-testid="invites-empty">
        No invites to show.
      </div>

      <ul v-else class="invite-list">
        <li
          v-for="invite in invites"
          :key="invite.id"
          class="invite-row"
          :data-testid="`invite-row-${invite.id}`"
        >
          <button class="invite-main" type="button" @click="openDetail(invite)">
            <span
              class="account-dot"
              :style="{ backgroundColor: acctColor(invite.account_id).fill }"
              :title="accountLabel(invite.account_id)"
            ></span>
            <span class="invite-text">
              <span class="invite-title-line">
                {{ invite.title || "(No title)" }}
                <span
                  v-if="invite.recurrence_rule"
                  class="repeat-badge"
                  :title="recurrenceLabel(invite)"
                >Repeats</span>
              </span>
              <span class="invite-when">{{ whenLabel(invite) }}</span>
              <span class="invite-sub">
                <template v-if="invite.organizer_email">
                  {{ invite.organizer_email }} ·
                </template>
                {{ accountLabel(invite.account_id) }}
              </span>
            </span>
            <span
              class="status-pill"
              :class="`status-${normalizeInviteStatus(invite.my_status)}`"
            >
              {{ statusLabel(invite) }}
            </span>
          </button>

          <div class="invite-actions">
            <button
              class="btn-accept"
              :class="{ chosen: isCurrent(invite, 'accepted') }"
              :disabled="respondingId === invite.id"
              :data-testid="`invite-accept-${invite.id}`"
              @click="respond(invite, 'accepted')"
            >
              Accept
            </button>
            <button
              class="btn-maybe"
              :class="{ chosen: isCurrent(invite, 'tentative') }"
              :disabled="respondingId === invite.id"
              :data-testid="`invite-maybe-${invite.id}`"
              @click="respond(invite, 'tentative')"
            >
              Maybe
            </button>
            <button
              class="btn-decline"
              :class="{ chosen: isCurrent(invite, 'declined') }"
              :disabled="respondingId === invite.id"
              :data-testid="`invite-decline-${invite.id}`"
              @click="respond(invite, 'declined')"
            >
              Decline
            </button>
          </div>

          <div v-if="errorById[invite.id]" class="invite-error">
            {{ errorById[invite.id] }}
          </div>
        </li>
      </ul>
    </div>

    <EventDetail v-if="detailOpen" @close="closeDetail" />
  </div>
</template>

<style scoped>
.invites-view {
  display: flex;
  flex-direction: column;
  height: 100%;
  width: 100%;
  background: var(--color-bg);
  overflow: hidden;
}

.invites-header {
  flex-shrink: 0;
  padding: 12px 16px;
  border-bottom: 1px solid var(--color-divider, #e9e0cd);
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.invites-title {
  font-size: 16px;
  font-weight: 600;
  color: var(--color-text);
}

.invites-controls {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  flex-wrap: wrap;
}

.status-filter {
  display: flex;
  gap: 4px;
  flex-wrap: wrap;
}

.seg-btn {
  height: 30px;
  padding: 0 12px;
  border: 0;
  border-radius: 999px;
  background: var(--color-bg-tertiary);
  font-family: inherit;
  font-size: 12px;
  font-weight: 500;
  color: var(--color-text);
  cursor: pointer;
  transition: background 0.12s;
}

.seg-btn.active {
  background: var(--color-accent);
  color: #fff;
  font-weight: 600;
}

.sort-control {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--color-text-secondary);
}

.sort-control select {
  height: 30px;
  padding: 0 8px;
  border: 1px solid var(--color-border);
  border-radius: 6px;
  background: var(--color-bg);
  font-family: inherit;
  font-size: 12px;
  color: var(--color-text);
}

.invites-body {
  flex: 1;
  overflow-y: auto;
}

.invites-empty {
  padding: 40px 16px;
  text-align: center;
  color: var(--color-text-muted);
  font-size: 13px;
}

.invite-list {
  list-style: none;
  margin: 0;
  padding: 0;
}

.invite-row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 12px;
  padding: 12px 16px;
  border-bottom: 1px solid var(--color-divider, #e9e0cd);
}

.invite-main {
  flex: 1;
  min-width: 240px;
  display: flex;
  align-items: flex-start;
  gap: 10px;
  background: transparent;
  border: 0;
  padding: 0;
  text-align: left;
  cursor: pointer;
  font-family: inherit;
}

.account-dot {
  flex-shrink: 0;
  width: 10px;
  height: 10px;
  border-radius: 3px;
  margin-top: 3px;
}

.invite-text {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.invite-title-line {
  font-size: 14px;
  font-weight: 600;
  color: var(--color-text);
  display: flex;
  align-items: center;
  gap: 6px;
}

.repeat-badge {
  font-size: 10px;
  font-weight: 600;
  padding: 1px 6px;
  border-radius: 999px;
  background: var(--color-bg-tertiary);
  color: var(--color-text-secondary);
}

.invite-when {
  font-size: 12px;
  color: var(--color-text-secondary);
}

.invite-sub {
  font-size: 11px;
  color: var(--color-text-muted);
}

.status-pill {
  flex-shrink: 0;
  font-size: 11px;
  font-weight: 600;
  padding: 2px 8px;
  border-radius: 999px;
  background: var(--color-bg-tertiary);
  color: var(--color-text-secondary);
}

.status-accepted {
  background: rgba(0, 166, 62, 0.14);
  color: #00802f;
}

.status-tentative {
  background: rgba(225, 113, 0, 0.14);
  color: #b35900;
}

.status-declined {
  background: rgba(251, 44, 54, 0.14);
  color: #c20710;
}

.invite-actions {
  display: flex;
  gap: 6px;
  flex-shrink: 0;
}

.invite-actions button {
  padding: 5px 12px;
  border-radius: 6px;
  font-size: 12px;
  font-weight: 500;
  font-family: inherit;
  cursor: pointer;
  border: 1px solid var(--color-border);
  background: var(--color-bg);
  color: var(--color-text-secondary);
  transition: all 0.12s;
}

.invite-actions button:disabled {
  opacity: 0.5;
  cursor: default;
}

.btn-accept.chosen {
  background: #00a63e;
  border-color: #00a63e;
  color: #fff;
}

.btn-maybe.chosen {
  background: #e17100;
  border-color: #e17100;
  color: #fff;
}

.btn-decline.chosen {
  background: #fb2c36;
  border-color: #fb2c36;
  color: #fff;
}

.invite-error {
  flex-basis: 100%;
  font-size: 11px;
  color: var(--color-danger, #c20710);
}
</style>
