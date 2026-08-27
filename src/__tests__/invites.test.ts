import { describe, it, expect, vi, beforeEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";
import { mount } from "@vue/test-utils";

vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  listInvites: vi.fn().mockResolvedValue([]),
  markInviteManaged: vi.fn().mockResolvedValue(undefined),
  respondToEvent: vi.fn().mockResolvedValue(undefined),
  triggerSync: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import {
  useInvitesStore,
  isInviteManaged,
  nextOccurrence,
  parseInviteTimestamp,
} from "@/stores/invites";
import { useAccountsStore } from "@/stores/accounts";
import { useUiStore } from "@/stores/ui";
import type { Invite } from "@/lib/types";
import * as api from "@/lib/tauri";
import { listen } from "@tauri-apps/api/event";
import InvitesView from "@/views/InvitesView.vue";

function makeAccount(id: string, email: string) {
  return {
    id,
    display_name: email,
    email,
    provider: "generic" as const,
    mail_protocol: "jmap" as const,
    enabled: true,
    mail_sync_interval_seconds: null,
    calendar_sync_interval_seconds: null,
    contacts_sync_interval_seconds: null,
    username: "",
    has_calendar_binding: false,
    has_contacts_binding: false,
    meet_protocol: "" as const,
  };
}

function makeInvite(id: string, opts: Partial<Invite> = {}): Invite {
  return {
    id,
    account_id: opts.account_id ?? "acc1",
    calendar_id: "cal1",
    uid: `${id}@chithi`,
    title: opts.title ?? id,
    description: null,
    location: null,
    start_time: opts.start_time ?? "2026-06-01T10:00:00Z",
    end_time: opts.end_time ?? "2026-06-01T11:00:00Z",
    all_day: false,
    timezone: null,
    recurrence_rule: opts.recurrence_rule ?? null,
    organizer_email: opts.organizer_email ?? "boss@example.com",
    attendees_json: opts.attendees_json ?? null,
    my_status: opts.my_status ?? null,
    source_message_id: null,
    manually_managed_at: opts.manually_managed_at ?? null,
    created_at: opts.created_at ?? "2026-05-01 09:00:00",
  };
}

/** Flush pending microtasks so fire-and-forget fetches settle. */
function flush() {
  return new Promise((r) => setTimeout(r, 0));
}

beforeEach(() => {
  setActivePinia(createPinia());
  // Clear so the "show invites badge" preference starts at its default
  // (enabled) and doesn't leak between tests.
  localStorage.clear();
  vi.mocked(api.listAccounts).mockResolvedValue([]);
  vi.mocked(api.listInvites).mockReset().mockResolvedValue([]);
  vi.mocked(api.markInviteManaged).mockReset().mockResolvedValue(undefined);
  vi.mocked(api.respondToEvent).mockReset().mockResolvedValue(undefined);
  vi.mocked(listen).mockClear().mockResolvedValue(() => {});
});

describe("invites store — fetch", () => {
  it("aggregates invites across all accounts", async () => {
    const accounts = useAccountsStore();
    accounts.accounts = [
      makeAccount("acc1", "me@example.com"),
      makeAccount("acc2", "me@work.example.com"),
    ];
    vi.mocked(api.listInvites).mockImplementation((accountId: string) =>
      Promise.resolve(
        accountId === "acc1"
          ? [makeInvite("a1", { account_id: "acc1" })]
          : [
              makeInvite("b1", { account_id: "acc2" }),
              makeInvite("b2", { account_id: "acc2" }),
            ],
      ),
    );

    const store = useInvitesStore();
    await store.fetchInvites();

    expect(api.listInvites).toHaveBeenCalledWith("acc1");
    expect(api.listInvites).toHaveBeenCalledWith("acc2");
    expect(store.invites.map((i) => i.id).sort()).toEqual(["a1", "b1", "b2"]);
  });
});

describe("invites store — filtering", () => {
  beforeEach(() => {
    const accounts = useAccountsStore();
    accounts.accounts = [makeAccount("acc1", "me@example.com")];
  });

  async function loadInvites(invites: Invite[]) {
    vi.mocked(api.listInvites).mockResolvedValue(invites);
    const store = useInvitesStore();
    await store.fetchInvites();
    return store;
  }

  it("status filter narrows to the selected reply state", async () => {
    const store = await loadInvites([
      makeInvite("none", { my_status: null }),
      makeInvite("managed", { manually_managed_at: "2026-05-02 10:00:00" }),
      makeInvite("acc", { my_status: "accepted" }),
      makeInvite("may", { my_status: "tentative" }),
      makeInvite("dec", { my_status: "declined" }),
    ]);

    store.setStatusFilter("all");
    expect(store.filteredInvites).toHaveLength(5);

    store.setStatusFilter("needs-action");
    expect(store.filteredInvites.map((i) => i.id)).toEqual(["none"]);

    store.setStatusFilter("accepted");
    expect(store.filteredInvites.map((i) => i.id)).toEqual(["acc"]);

    store.setStatusFilter("tentative");
    expect(store.filteredInvites.map((i) => i.id)).toEqual(["may"]);

    store.setStatusFilter("declined");
    expect(store.filteredInvites.map((i) => i.id)).toEqual(["dec"]);
  });

  it("needsActionCount counts only unanswered invites", async () => {
    const store = await loadInvites([
      makeInvite("none1", { my_status: null }),
      makeInvite("none2", { my_status: "needs-action" }),
      makeInvite("managed", { manually_managed_at: "2026-05-02 10:00:00" }),
      makeInvite("acc", { my_status: "accepted" }),
    ]);
    expect(store.needsActionCount).toBe(2);
  });

  it("treats either a reply or manual acknowledgement as managed", () => {
    expect(isInviteManaged(makeInvite("none"))).toBe(false);
    expect(
      isInviteManaged(
        makeInvite("manual", { manually_managed_at: "2026-05-02 10:00:00" }),
      ),
    ).toBe(true);
    expect(isInviteManaged(makeInvite("accepted", { my_status: "accepted" }))).toBe(
      true,
    );
  });

  it("sorts by event date ascending and descending", async () => {
    const store = await loadInvites([
      makeInvite("late", { start_time: "2026-08-01T10:00:00Z" }),
      makeInvite("early", { start_time: "2026-06-01T10:00:00Z" }),
      makeInvite("mid", { start_time: "2026-07-01T10:00:00Z" }),
    ]);

    store.setSortMode("date-asc");
    expect(store.filteredInvites.map((i) => i.id)).toEqual([
      "early",
      "mid",
      "late",
    ]);

    store.setSortMode("date-desc");
    expect(store.filteredInvites.map((i) => i.id)).toEqual([
      "late",
      "mid",
      "early",
    ]);
  });

  it("sorts by arrival time for 'recently received'", async () => {
    const store = await loadInvites([
      makeInvite("old", { created_at: "2026-05-01 09:00:00" }),
      makeInvite("new", { created_at: "2026-05-10 09:00:00" }),
      makeInvite("mid", { created_at: "2026-05-05 09:00:00" }),
    ]);
    store.setSortMode("received");
    expect(store.filteredInvites.map((i) => i.id)).toEqual([
      "new",
      "mid",
      "old",
    ]);
  });
});

describe("invites store — RSVP data flow", () => {
  beforeEach(() => {
    const accounts = useAccountsStore();
    accounts.accounts = [makeAccount("acc1", "me@example.com")];
  });

  it("respond() calls respondToEvent with account, event id and response", async () => {
    vi.mocked(api.listInvites).mockResolvedValue([makeInvite("e1")]);
    const store = useInvitesStore();
    await store.fetchInvites();

    const fetchesBefore = vi.mocked(api.listInvites).mock.calls.length;
    await store.respond(store.invites[0], "accepted");

    expect(api.respondToEvent).toHaveBeenCalledWith("acc1", "e1", "accepted");
    // respond() refetches so the list reflects the new status.
    expect(vi.mocked(api.listInvites).mock.calls.length).toBe(
      fetchesBefore + 1,
    );
  });

  it("markManaged() uses the local-only API and never sends an RSVP", async () => {
    vi.mocked(api.listInvites).mockResolvedValue([makeInvite("e1")]);
    const store = useInvitesStore();
    await store.fetchInvites();

    const fetchesBefore = vi.mocked(api.listInvites).mock.calls.length;
    await store.markManaged(store.invites[0]);

    expect(api.markInviteManaged).toHaveBeenCalledWith("acc1", "e1");
    expect(api.respondToEvent).not.toHaveBeenCalled();
    expect(vi.mocked(api.listInvites).mock.calls.length).toBe(
      fetchesBefore + 1,
    );
  });

  it("refetches invites when a calendar-changed event fires", async () => {
    vi.mocked(api.listInvites).mockResolvedValue([]);
    useInvitesStore();
    await flush();

    const before = vi.mocked(api.listInvites).mock.calls.length;
    const subscription = vi
      .mocked(listen)
      .mock.calls.find((c) => c[0] === "calendar-changed");
    expect(subscription).toBeDefined();

    // Simulate the backend emitting the event.
    (subscription![1] as (e: unknown) => void)({});
    await flush();

    expect(vi.mocked(api.listInvites).mock.calls.length).toBeGreaterThan(
      before,
    );
  });
});

describe("Invites view — manual management", () => {
  it("marks an unanswered invite without sending an RSVP", async () => {
    const accounts = useAccountsStore();
    accounts.accounts = [makeAccount("acc1", "me@example.com")];
    vi.mocked(api.listInvites).mockResolvedValue([makeInvite("e1")]);

    const wrapper = mount(InvitesView);
    await flush();
    await wrapper.get('[data-testid="invite-mark-managed-e1"]').trigger("click");
    await flush();

    expect(api.markInviteManaged).toHaveBeenCalledWith("acc1", "e1");
    expect(api.respondToEvent).not.toHaveBeenCalled();
    wrapper.unmount();
  });

  it("labels a manually acknowledged invite without calling it accepted", async () => {
    const accounts = useAccountsStore();
    accounts.accounts = [makeAccount("acc1", "me@example.com")];
    vi.mocked(api.listInvites).mockResolvedValue([
      makeInvite("e1", { manually_managed_at: "2026-05-02 10:00:00" }),
    ]);

    const wrapper = mount(InvitesView);
    await flush();

    expect(wrapper.get(".status-pill").text()).toBe("Managed manually");
    expect(wrapper.find('[data-testid="invite-mark-managed-e1"]').exists()).toBe(false);
    wrapper.unmount();
  });
});

describe("nextOccurrence", () => {
  it("returns the event start for non-recurring invites", () => {
    const invite = makeInvite("x", { start_time: "2026-06-01T10:00:00Z" });
    expect(nextOccurrence(invite).toISOString()).toBe("2026-06-01T10:00:00.000Z");
  });

  it("returns an upcoming occurrence for a recurring invite", () => {
    const now = new Date("2026-05-19T00:00:00Z");
    const invite = makeInvite("weekly", {
      start_time: "2026-01-05T10:00:00Z",
      end_time: "2026-01-05T10:30:00Z",
      recurrence_rule: "FREQ=WEEKLY",
    });
    const occ = nextOccurrence(invite, now);
    // The next standup must not be the long-past series start.
    expect(occ.getTime()).toBeGreaterThanOrEqual(now.getTime());
  });

  it("finds the next occurrence of a daily series that started years ago", () => {
    const now = new Date("2026-05-19T00:00:00Z");
    const invite = makeInvite("daily", {
      start_time: "2022-01-01T09:00:00Z",
      end_time: "2022-01-01T09:30:00Z",
      recurrence_rule: "FREQ=DAILY",
    });
    const occ = nextOccurrence(invite, now);
    // Must be an upcoming occurrence, not the long-past series start.
    expect(occ.getTime()).toBeGreaterThanOrEqual(now.getTime());
    expect(occ.getTime()).toBeLessThan(now.getTime() + 2 * 86_400_000);
  });

  it("finds the next occurrence of a long-interval yearly series", () => {
    const now = new Date("2026-05-19T00:00:00Z");
    // Occurs in 2023, 2028, 2033, ... — the next instance is beyond a
    // naive fixed 2-year horizon, so the window must size to the interval.
    const invite = makeInvite("yearly5", {
      start_time: "2023-06-15T10:00:00Z",
      end_time: "2023-06-15T11:00:00Z",
      recurrence_rule: "FREQ=YEARLY;INTERVAL=5",
    });
    const occ = nextOccurrence(invite, now);
    expect(occ.getTime()).toBeGreaterThan(Date.parse("2028-01-01T00:00:00Z"));
    expect(occ.getTime()).toBeLessThan(Date.parse("2029-01-01T00:00:00Z"));
  });
});

describe("parseInviteTimestamp", () => {
  it("treats a SQLite CURRENT_TIMESTAMP string as UTC", () => {
    // "YYYY-MM-DD HH:MM:SS" with no zone — must be read as UTC.
    expect(parseInviteTimestamp("2026-05-01 09:00:00")).toBe(
      Date.parse("2026-05-01T09:00:00Z"),
    );
  });

  it("passes ISO-8601 strings through and handles missing values", () => {
    expect(parseInviteTimestamp("2026-05-01T09:00:00Z")).toBe(
      Date.parse("2026-05-01T09:00:00Z"),
    );
    expect(parseInviteTimestamp(null)).toBe(0);
    expect(parseInviteTimestamp("not a date")).toBe(0);
  });
});

describe("invites store — background tracking", () => {
  it("skips the eager fetch when the badge is off and the view is closed", async () => {
    const ui = useUiStore();
    ui.setShowInviteBadge(false);
    const accounts = useAccountsStore();
    accounts.accounts = [makeAccount("acc1", "me@example.com")];

    const store = useInvitesStore();
    await flush();
    // No badge, no open view → no background fetch.
    expect(api.listInvites).not.toHaveBeenCalled();

    // Opening the Invites view starts tracking and triggers a load.
    store.setViewActive(true);
    await flush();
    expect(api.listInvites).toHaveBeenCalledWith("acc1");
  });

  it("eager-fetches when the badge preference is enabled", async () => {
    const ui = useUiStore();
    ui.setShowInviteBadge(true);
    const accounts = useAccountsStore();
    accounts.accounts = [makeAccount("acc1", "me@example.com")];

    useInvitesStore();
    await flush();
    expect(api.listInvites).toHaveBeenCalledWith("acc1");
  });
});
