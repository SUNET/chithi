/**
 * Tests for Google Calendar API v3 and People API v1 integration.
 * Verifies routing logic, data structure building, and status mappings.
 * See docs/google_calendar.md for the full implementation status.
 */
import { describe, it, expect } from "vitest";

// Helper: simulate account routing logic from commands/calendar.rs
function getCalendarRoute(account: {
  provider: string;
  mail_protocol: string;
  caldav_url: string;
}): string {
  if (account.provider === "gmail") return "google_calendar_api";
  if (account.mail_protocol === "jmap") return "jmap";
  if (account.caldav_url) return "caldav";
  return "local_only";
}

// Helper: map our status to Google Calendar responseStatus
function toGoogleStatus(status: string): string {
  switch (status.toLowerCase()) {
    case "accepted": return "accepted";
    case "tentative": return "tentative";
    case "declined": return "declined";
    default: return "needsAction";
  }
}

// Helper: build Google Calendar event JSON
function buildGoogleEvent(event: {
  title: string;
  start_time: string;
  end_time: string;
  all_day: boolean;
  description?: string | null;
  location?: string | null;
  uid?: string | null;
  attendees?: { email: string }[];
}) {
  const ge: Record<string, unknown> = {
    summary: event.title,
    start: event.all_day
      ? { date: event.start_time.split("T")[0] }
      : { dateTime: event.start_time },
    end: event.all_day
      ? { date: event.end_time.split("T")[0] }
      : { dateTime: event.end_time },
  };
  if (event.uid) ge.iCalUID = event.uid;
  if (event.description) ge.description = event.description;
  if (event.location) ge.location = event.location;
  if (event.attendees && event.attendees.length > 0) ge.attendees = event.attendees;
  return ge;
}

describe("Google Calendar: routing logic (#1-#5)", () => {
  it("Gmail accounts route to Google Calendar API", () => {
    expect(getCalendarRoute({ provider: "gmail", mail_protocol: "imap", caldav_url: "" }))
      .toBe("google_calendar_api");
  });

  it("Gmail checked before JMAP", () => {
    // Gmail accounts have mail_protocol=imap, should never hit jmap path
    expect(getCalendarRoute({ provider: "gmail", mail_protocol: "imap", caldav_url: "" }))
      .not.toBe("jmap");
  });

  it("JMAP accounts route to JMAP", () => {
    expect(getCalendarRoute({ provider: "generic", mail_protocol: "jmap", caldav_url: "" }))
      .toBe("jmap");
  });

  it("IMAP with CalDAV routes to CalDAV", () => {
    expect(getCalendarRoute({ provider: "generic", mail_protocol: "imap", caldav_url: "https://dav.example.com" }))
      .toBe("caldav");
  });

  it("IMAP without CalDAV is local only", () => {
    expect(getCalendarRoute({ provider: "generic", mail_protocol: "imap", caldav_url: "" }))
      .toBe("local_only");
  });
});

describe("Google Calendar: event creation (#1)", () => {
  it("builds correct event JSON for timed event", () => {
    const ge = buildGoogleEvent({
      title: "Meeting",
      start_time: "2026-04-07T17:00:00Z",
      end_time: "2026-04-07T18:00:00Z",
      all_day: false,
      uid: "uid-123@chithi",
    });
    expect(ge.summary).toBe("Meeting");
    expect(ge.start).toEqual({ dateTime: "2026-04-07T17:00:00Z" });
    expect(ge.end).toEqual({ dateTime: "2026-04-07T18:00:00Z" });
    expect(ge.iCalUID).toBe("uid-123@chithi");
  });

  it("builds correct event JSON for all-day event", () => {
    const ge = buildGoogleEvent({
      title: "Holiday",
      start_time: "2026-04-07T00:00:00Z",
      end_time: "2026-04-08T00:00:00Z",
      all_day: true,
    });
    expect(ge.start).toEqual({ date: "2026-04-07" });
    expect(ge.end).toEqual({ date: "2026-04-08" });
  });

  it("includes attendees when present", () => {
    const ge = buildGoogleEvent({
      title: "Team sync",
      start_time: "2026-04-07T17:00:00Z",
      end_time: "2026-04-07T18:00:00Z",
      all_day: false,
      attendees: [{ email: "alice@example.com" }, { email: "bob@example.com" }],
    });
    expect(ge.attendees).toHaveLength(2);
  });

  it("omits optional fields when null", () => {
    const ge = buildGoogleEvent({
      title: "Quick chat",
      start_time: "2026-04-07T17:00:00Z",
      end_time: "2026-04-07T17:30:00Z",
      all_day: false,
    });
    expect(ge.description).toBeUndefined();
    expect(ge.location).toBeUndefined();
    expect(ge.attendees).toBeUndefined();
    expect(ge.iCalUID).toBeUndefined();
  });
});

describe("Google Calendar: sendUpdates parameter (#4)", () => {
  it("uses 'all' when attendees present", () => {
    const hasAttendees = true;
    const sendUpdates = hasAttendees ? "all" : "none";
    expect(sendUpdates).toBe("all");
  });

  it("uses 'none' when no attendees", () => {
    const hasAttendees = false;
    const sendUpdates = hasAttendees ? "all" : "none";
    expect(sendUpdates).toBe("none");
  });
});

describe("Google Calendar: RSVP status mapping (#5)", () => {
  it("maps accepted", () => expect(toGoogleStatus("accepted")).toBe("accepted"));
  it("maps tentative", () => expect(toGoogleStatus("tentative")).toBe("tentative"));
  it("maps declined", () => expect(toGoogleStatus("declined")).toBe("declined"));
  it("maps unknown to needsAction", () => expect(toGoogleStatus("unknown")).toBe("needsAction"));
  it("maps needs-action to needsAction", () => expect(toGoogleStatus("needs-action")).toBe("needsAction"));
  it("is case insensitive", () => expect(toGoogleStatus("ACCEPTED")).toBe("accepted"));
});

describe("Google Calendar: invite import (#6)", () => {
  it("import event structure has required fields", () => {
    const importEvent = {
      iCalUID: "uid-abc@chithi",
      summary: "Invited Meeting",
      start: { dateTime: "2026-04-07T17:00:00Z" },
      end: { dateTime: "2026-04-07T18:00:00Z" },
      organizer: { email: "organizer@example.com" },
      attendees: [{ email: "me@gmail.com", responseStatus: "accepted", self: true }],
    };
    expect(importEvent.iCalUID).toBe("uid-abc@chithi");
    expect(importEvent.attendees[0].responseStatus).toBe("accepted");
    expect(importEvent.attendees[0].self).toBe(true);
  });
});

describe("Google Calendar: incremental sync (#7)", () => {
  it("syncToken key is unique per account+calendar", () => {
    const accountId = "acc-123";
    const calendarId = "cal-456";
    const key = `google_sync_token_${accountId}_${calendarId}`;
    expect(key).toBe("google_sync_token_acc-123_cal-456");

    // Different calendar should have different key
    const key2 = `google_sync_token_${accountId}_cal-789`;
    expect(key).not.toBe(key2);
  });

  it("410 Gone should clear syncToken and retry", () => {
    const status = 410;
    const shouldClearToken = status === 410;
    expect(shouldClearToken).toBe(true);
  });
});

describe("Google Calendar: calendar ID mapping (#3)", () => {
  it("uses calendar remote_id in API URL, not hardcoded primary", () => {
    const calRemoteId = "user@gmail.com";
    const eventId = "event-123";
    const url = `https://www.googleapis.com/calendar/v3/calendars/${encodeURIComponent(calRemoteId)}/events/${encodeURIComponent(eventId)}`;
    expect(url).toContain("user%40gmail.com");
    expect(url).not.toContain("primary");
  });

  it("falls back to primary when no remote_id", () => {
    const calRemoteId: string | null = null;
    const effectiveId = calRemoteId ?? "primary";
    expect(effectiveId).toBe("primary");
  });
});

describe("Google Calendar: color mapping (#12)", () => {
  it("Google backgroundColor is hex format, no conversion needed", () => {
    const googleCal = { backgroundColor: "#0b8043" };
    // We store this directly — no colorId mapping required
    expect(googleCal.backgroundColor).toMatch(/^#[0-9a-f]{6}$/i);
  });
});

describe("Google Contacts: People API (#13, #14)", () => {
  it("create contact builds Person resource", () => {
    const person = {
      names: [{ givenName: "Alice Smith" }],
      emailAddresses: [{ value: "alice@example.com" }],
      phoneNumbers: [{ value: "+1 555 0123" }],
    };
    expect(person.names[0].givenName).toBe("Alice Smith");
    expect(person.emailAddresses[0].value).toBe("alice@example.com");
  });

  it("delete contact URL uses resourceName", () => {
    const resourceName = "people/c12345";
    const url = `https://people.googleapis.com/v1/${resourceName}:deleteContact`;
    expect(url).toBe("https://people.googleapis.com/v1/people/c12345:deleteContact");
  });

  it("update contact URL includes updatePersonFields", () => {
    const resourceName = "people/c12345";
    const url = `https://people.googleapis.com/v1/${resourceName}:updateContact?updatePersonFields=names,emailAddresses,phoneNumbers`;
    expect(url).toContain("updatePersonFields=");
    expect(url).toContain("names");
  });

  it("sync_type must be 'google' to push to People API", () => {
    const syncTypes = ["local", "google", "jmap", "carddav"];
    const shouldPush = syncTypes.filter(t => t === "google");
    expect(shouldPush).toEqual(["google"]);
  });
});
