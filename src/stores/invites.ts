import { defineStore } from "pinia";
import { ref, computed, onScopeDispose } from "vue";
import { listen } from "@tauri-apps/api/event";
import type { Invite } from "@/lib/types";
import { expandRRule } from "@/lib/rrule";
import * as api from "@/lib/tauri";
import { useAccountsStore } from "./accounts";

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
        ? invites.value.slice()
        : invites.value.filter(
            (inv) =>
              normalizeInviteStatus(inv.my_status) === statusFilter.value,
          );

    if (sortMode.value === "received") {
      list.sort((a, b) => {
        const ta = a.created_at ? new Date(a.created_at).getTime() : 0;
        const tb = b.created_at ? new Date(b.created_at).getTime() : 0;
        return tb - ta;
      });
    } else {
      const now = new Date();
      list.sort((a, b) => {
        const ta = nextOccurrence(a, now).getTime();
        const tb = nextOccurrence(b, now).getTime();
        return sortMode.value === "date-asc" ? ta - tb : tb - ta;
      });
    }
    return list;
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

  // Eager load + refresh on backend calendar changes so the sidebar badge
  // stays accurate even before the Invites tab is ever opened.
  let stopListener: null | (() => void) = null;
  let disposed = false;
  void fetchInvites();
  void listen<string>("calendar-changed", () => {
    if (disposed) return;
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
    respond,
  };
});
