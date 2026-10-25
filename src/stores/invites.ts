import { defineStore } from "pinia";
import { ref, computed, watch, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import type { Invite } from "@/lib/types";
import { expandRRule, parseRRule } from "@/lib/rrule";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";
import { useUiStore } from "./ui";

export type InviteStatusFilter =
  | "all"
  | "needs-action"
  | "accepted"
  | "tentative"
  | "declined";

export type InviteSortMode = "date-asc" | "date-desc" | "received";

/** Missing or unknown statuses require action. */
export function normalizeInviteStatus(
  status: string | null | undefined,
): Exclude<InviteStatusFilter, "all"> {
  if (
    status === "accepted" ||
    status === "tentative" ||
    status === "declined"
  ) {
    return status;
  }
  return "needs-action";
}

export function isInviteManaged(
  invite: Pick<Invite, "my_status" | "manually_managed_at">,
): boolean {
  return (
    invite.manually_managed_at != null ||
    normalizeInviteStatus(invite.my_status) !== "needs-action"
  );
}

/** Returns the next unfinished occurrence, or the last of a past series. */
export function nextOccurrence(invite: Invite, now: Date = new Date()): Date {
  const start = new Date(invite.start_time);
  if (!invite.recurrence_rule) return start;

  const end = new Date(invite.end_time);
  const DAY_MS = 86_400_000;

  // Scale the horizon so long recurrence intervals still find their next date.
  const parsed = parseRRule(invite.recurrence_rule);
  const interval = parsed?.interval ?? 1;
  const intervalDays =
    parsed?.freq === "WEEKLY"
      ? 7 * interval
      : parsed?.freq === "MONTHLY"
        ? 31 * interval
        : parsed?.freq === "YEARLY"
          ? 366 * interval
          : interval;
  const rangeStart = new Date(now.getTime() - 366 * DAY_MS);
  const rangeEnd = new Date(now.getTime() + (intervalDays + 14) * DAY_MS);

  const occurrences = expandRRule(
    invite.recurrence_rule,
    start,
    end,
    rangeStart,
    rangeEnd,
  );
  if (occurrences.length === 0) return start;
  const upcoming = occurrences.find(
    (occurrence) => occurrence.end.getTime() >= now.getTime(),
  );
  return upcoming ? upcoming.start : occurrences[occurrences.length - 1].start;
}

/** Normalizes SQLite UTC timestamps; invalid values sort last as zero. */
export function parseInviteTimestamp(
  timestamp: string | null | undefined,
): number {
  if (!timestamp) return 0;
  const normalized = /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/.test(timestamp)
    ? `${timestamp.replace(" ", "T")}Z`
    : timestamp;
  const parsedTime = new Date(normalized).getTime();
  return Number.isNaN(parsedTime) ? 0 : parsedTime;
}

export const useInvitesStore = defineStore("invites", () => {
  const invites = ref<Invite[]>([]);
  const loading = ref(false);
  const statusFilter = ref<InviteStatusFilter>("all");
  const sortMode = ref<InviteSortMode>("date-asc");

  const accountsStore = useAccountsStore();

  async function fetchInvites() {
    loading.value = true;
    try {
      if (accountsStore.accounts.length === 0) {
        await accountsStore.fetchAccounts();
      }
      const results = await Promise.all(
        accountsStore.accounts.map((account) =>
          api.listInvites(account.id).catch((error) => {
            console.error("Failed to fetch invites for", account.id, error);
            return [] as Invite[];
          }),
        ),
      );
      invites.value = results.flat();
    } finally {
      loading.value = false;
    }
  }

  const needsActionCount = computed(
    () => invites.value.filter((invite) => !isInviteManaged(invite)).length,
  );

  const filteredInvites = computed<Invite[]>(() => {
    const list =
      statusFilter.value === "all"
        ? invites.value
        : invites.value.filter(
            (invite) =>
              statusFilter.value === "needs-action"
                ? !isInviteManaged(invite)
                : normalizeInviteStatus(invite.my_status) === statusFilter.value,
          );

    // Cache keys so sorting does not repeatedly expand recurrence rules.
    const now = new Date();
    const keyed = list.map((invite) => ({
      invite,
      key:
        sortMode.value === "received"
          ? parseInviteTimestamp(invite.created_at)
          : nextOccurrence(invite, now).getTime(),
    }));

    const descending = sortMode.value !== "date-asc";
    keyed.sort((a, b) => (descending ? b.key - a.key : a.key - b.key));
    return keyed.map((entry) => entry.invite);
  });

  function setStatusFilter(filter: InviteStatusFilter) {
    statusFilter.value = filter;
  }

  function setSortMode(mode: InviteSortMode) {
    sortMode.value = mode;
  }

  /** Sends an RSVP rebuilt from the stored event, then refreshes its status. */
  async function respond(invite: Invite, response: string) {
    await api.respondToEvent(invite.account_id, invite.id, response);
    await fetchInvites();
  }

  /** Acknowledge an invite locally without sending or changing its RSVP. */
  async function markManaged(invite: Invite) {
    await api.markInviteManaged(invite.account_id, invite.id);
    await fetchInvites();
  }

  const viewActive = ref(false);
  function setViewActive(active: boolean) {
    viewActive.value = active;
  }

  // Skip background fetches when neither the badge nor view consumes them.
  const uiStore = useUiStore();
  const tracking = computed(
    () => uiStore.showInviteBadge || viewActive.value,
  );

  watch(
    tracking,
    (enabled) => {
      if (enabled) void fetchInvites();
    },
    { immediate: true },
  );

  let stopListener: (() => void) | null = null;
  let disposed = false;
  void listen<string>("calendar-changed", () => {
    if (disposed || !tracking.value) return;
    fetchInvites().catch(() => {});
  })
    .then((unlisten) => {
      if (disposed) {
        unlisten();
        return;
      }
      stopListener = unlisten;
    })
    .catch((error) =>
      console.error("invites: failed to subscribe to calendar-changed", error),
    );

  onScopeDispose(() => {
    disposed = true;
    stopListener?.();
  });

  return {
    invites,
    loading,
    statusFilter,
    sortMode,
    needsActionCount,
    filteredInvites,
    fetchInvites,
    setStatusFilter,
    setSortMode,
    setViewActive,
    respond,
    markManaged,
  };
});
