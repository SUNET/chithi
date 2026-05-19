import { defineStore } from "pinia";
import { ref, computed, watch, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import type { Invite } from "@/lib/types";
import { expandRRule } from "@/lib/rrule";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";
import { useUiStore } from "./ui";

/** The four canonical RSVP states an invite can be filtered by. */
export type InviteStatusFilter =
  | "all"
  | "needs-action"
  | "accepted"
  | "tentative"
  | "declined";

export type InviteSortMode = "date-asc" | "date-desc" | "received";

/**
 * Collapse a stored `my_status` into one of the four canonical states.
 * A missing/unknown status counts as "needs-action" — i.e. unanswered.
 */
export function normalizeInviteStatus(
  status: string | null | undefined,
): Exclude<InviteStatusFilter, "all"> {
  switch (status) {
    case "accepted":
      return "accepted";
    case "tentative":
      return "tentative";
    case "declined":
      return "declined";
    default:
      return "needs-action";
  }
}

/**
 * The occurrence of an invite to display/sort by. Non-recurring invites
 * use their own start. Recurring invites expand the RRULE and pick the
 * first occurrence that hasn't ended yet; a fully-past series falls back
 * to its last occurrence so it still sorts sensibly.
 */
export function nextOccurrence(invite: Invite, now: Date = new Date()): Date {
  const start = new Date(invite.start_time);
  if (!invite.recurrence_rule) return start;

  const end = new Date(invite.end_time);
  const DAY = 86_400_000;
  const rangeStart = new Date(now.getTime() - 366 * DAY);
  const rangeEnd = new Date(now.getTime() + 730 * DAY);
  const occ = expandRRule(
    invite.recurrence_rule,
    start,
    end,
    rangeStart,
    rangeEnd,
  );
  if (occ.length === 0) return start;
  const upcoming = occ.find((o) => o.end.getTime() >= now.getTime());
  return upcoming ? upcoming.start : occ[occ.length - 1].start;
}

/**
 * Parse an invite's `created_at` into epoch millis. The backend sources it
 * from SQLite's `CURRENT_TIMESTAMP`, which is `YYYY-MM-DD HH:MM:SS` in UTC
 * with no zone marker — a format `new Date()` parses inconsistently across
 * runtimes (some treat it as local time). Normalize it to ISO-8601 UTC
 * first; already-ISO values pass through unchanged. Returns 0 when absent
 * or unparseable so such rows sort last.
 */
export function parseInviteTimestamp(ts: string | null | undefined): number {
  if (!ts) return 0;
  const normalized = /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/.test(ts)
    ? `${ts.replace(" ", "T")}Z`
    : ts;
  const t = new Date(normalized).getTime();
  return Number.isNaN(t) ? 0 : t;
}

export const useInvitesStore = defineStore("invites", () => {
  const invites = ref<Invite[]>([]);
  const loading = ref(false);
  const statusFilter = ref<InviteStatusFilter>("all");
  const sortMode = ref<InviteSortMode>("date-asc");

  const accountsStore = useAccountsStore();

  /** Fan out across every account and merge the results into one list. */
  async function fetchInvites() {
    loading.value = true;
    try {
      if (accountsStore.accounts.length === 0) {
        await accountsStore.fetchAccounts();
      }
      const results = await Promise.all(
        accountsStore.accounts.map((account) =>
          api.listInvites(account.id).catch((e) => {
            console.error("Failed to fetch invites for", account.id, e);
            return [] as Invite[];
          }),
        ),
      );
      invites.value = results.flat();
    } finally {
      loading.value = false;
    }
  }

  /** Count of invites still awaiting a reply — backs the sidebar badge. */
  const needsActionCount = computed(
    () =>
      invites.value.filter(
        (inv) => normalizeInviteStatus(inv.my_status) === "needs-action",
      ).length,
  );

  /** Invites narrowed by `statusFilter` and ordered by `sortMode`. */
  const filteredInvites = computed<Invite[]>(() => {
    const list =
      statusFilter.value === "all"
        ? invites.value
        : invites.value.filter(
            (inv) =>
              normalizeInviteStatus(inv.my_status) === statusFilter.value,
          );

    // Precompute one sort key per invite, then sort on the cached value.
    // `nextOccurrence()` expands the RRULE for recurring invites, so
    // calling it inside the comparator would re-expand it O(log n) times.
    const now = new Date();
    const keyed = list.map((invite) => ({
      invite,
      key:
        sortMode.value === "received"
          ? parseInviteTimestamp(invite.created_at)
          : nextOccurrence(invite, now).getTime(),
    }));

    // "received" and "date-desc" are newest/latest first; "date-asc" first.
    const descending = sortMode.value !== "date-asc";
    keyed.sort((a, b) => (descending ? b.key - a.key : a.key - b.key));
    return keyed.map((k) => k.invite);
  });

  function setStatusFilter(filter: InviteStatusFilter) {
    statusFilter.value = filter;
  }

  function setSortMode(mode: InviteSortMode) {
    sortMode.value = mode;
  }

  /**
   * Change the RSVP for an invite. The backend rebuilds the iTIP REPLY
   * from the stored event and emits `calendar-changed`; we also refetch
   * directly so the list reflects the new status immediately.
   */
  async function respond(invite: Invite, response: string) {
    await api.respondToEvent(invite.account_id, invite.id, response);
    await fetchInvites();
  }

  // Whether the Invites view is currently open. InvitesView toggles this
  // on mount/unmount so background work can stop when nothing needs it.
  const viewActive = ref(false);
  function setViewActive(active: boolean) {
    viewActive.value = active;
  }

  // Background invite tracking (the eager fetch and the calendar-changed
  // refresh) only earns its keep when something consumes the data: either
  // the sidebar badge is enabled, or the Invites view is open. With both
  // off we skip the fetch entirely so disabling the badge truly removes
  // the background activity.
  const uiStore = useUiStore();
  const tracking = computed(
    () => uiStore.showInviteBadge || viewActive.value,
  );

  // Refresh whenever tracking turns on (also fires immediately at startup
  // when the badge is enabled).
  watch(
    tracking,
    (on) => {
      if (on) void fetchInvites();
    },
    { immediate: true },
  );

  // Subscribe to backend calendar changes once; the handler is a no-op
  // while nothing is tracking, so it does no fetch work when the badge is
  // off and the view is closed.
  let stopListener: null | (() => void) = null;
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
    .catch((e) =>
      console.error("invites: failed to subscribe to calendar-changed", e),
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
  };
});
