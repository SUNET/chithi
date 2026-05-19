import { describe, it, expect, vi, beforeEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";

vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  listInvites: vi.fn().mockResolvedValue([]),
  respondToEvent: vi.fn().mockResolvedValue(undefined),
  triggerSync: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { useInvitesStore, nextOccurrence } from "@/stores/invites";
import { useAccountsStore } from "@/stores/accounts";
import type { Invite } from "@/lib/types";
import * as api from "@/lib/tauri";
import { listen } from "@tauri-apps/api/event";

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
    created_at: opts.created_at ?? "2026-05-01 09:00:00",
  };
}

/** Flush pending microtasks so fire-and-forget fetches settle. */
function flush() {
  return new Promise((r) => setTimeout(r, 0));
}

beforeEach(() => {
  setActivePinia(createPinia());
  vi.mocked(api.listAccounts).mockResolvedValue([]);
  vi.mocked(api.listInvites).mockReset().mockResolvedValue([]);
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
      makeInvite("acc", { my_status: "accepted" }),
      makeInvite("may", { my_status: "tentative" }),
      makeInvite("dec", { my_status: "declined" }),
    ]);

    store.setStatusFilter("all");
    expect(store.filteredInvites).toHaveLength(4);

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
      makeInvite("acc", { my_status: "accepted" }),
    ]);
    expect(store.needsActionCount).toBe(2);
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
});
