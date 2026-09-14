import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount, type VueWrapper } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

vi.mock("@/lib/tauri", () => ({
  getEmailInvites: vi.fn().mockResolvedValue([]),
  listCalendarImportTargets: vi.fn(),
  previewCalendarAttachment: vi.fn(),
  importCalendarAttachment: vi.fn(),
  saveAttachment: vi.fn().mockResolvedValue(undefined),
  listCalendars: vi.fn(),
  getEvents: vi.fn().mockResolvedValue([]),
}));

vi.mock("@/lib/compose-window", () => ({
  openComposeWindow: vi.fn(),
}));

import MessageReader from "@/components/mail/MessageReader.vue";
import { useAccountsStore } from "@/stores/accounts";
import { useMessagesStore } from "@/stores/messages";
import { useUiStore } from "@/stores/ui";
import * as api from "@/lib/tauri";
import type {
  Account,
  Attachment,
  Calendar,
  CalendarImportPreview,
  MessageBody,
} from "@/lib/types";

const account: Account = {
  id: "acc1",
  display_name: "Personal",
  email: "reader@example.test",
  username: "reader@example.test",
  provider: "generic",
  mail_protocol: "jmap",
  enabled: true,
  mail_sync_interval_seconds: null,
  calendar_sync_interval_seconds: null,
  contacts_sync_interval_seconds: null,
  has_calendar_binding: true,
  has_contacts_binding: false,
  meet_protocol: "",
};

const calendar: Calendar = {
  id: "cal1",
  account_id: account.id,
  name: "Home",
  color: "#123456",
  is_default: true,
  remote_id: "remote-cal1",
  is_subscribed: true,
};

const previews: CalendarImportPreview[] = [
  {
    uid: "series@example.test",
    title: "Weekly planning",
    description: null,
    location: "Room 2",
    start_time: "2026-09-14T08:00:00Z",
    end_time: "2026-09-14T09:00:00Z",
    all_day: false,
    timezone: "Europe/Stockholm",
    method: "REQUEST",
    recurrence_kind: "series",
    component_count: 2,
    organizer_email: "owner@example.test",
    attendee_count: 3,
    importable: true,
    import_error: null,
  },
  {
    uid: "single@example.test",
    title: "One-off event",
    description: null,
    location: null,
    start_time: "2026-09-15T08:00:00Z",
    end_time: "2026-09-15T09:00:00Z",
    all_day: false,
    timezone: null,
    method: "PUBLISH",
    recurrence_kind: "standalone",
    component_count: 1,
    organizer_email: null,
    attendee_count: 0,
    importable: true,
    import_error: null,
  },
];

let wrapper: VueWrapper | null = null;

function mountReader(attachment: Attachment) {
  const accountsStore = useAccountsStore();
  accountsStore.accounts = [account];
  accountsStore.activeAccountId = account.id;

  const message: MessageBody = {
    id: "message1",
    subject: "Calendar file",
    from: { email: "sender@example.test", name: "Sender" },
    to: [],
    cc: [],
    date: "2026-09-14T07:00:00Z",
    flags: [],
    body_html: null,
    body_text: "Attached",
    attachments: [attachment],
    is_encrypted: false,
    is_signed: false,
    list_id: null,
    has_remote_images: false,
  };
  const messagesStore = useMessagesStore();
  messagesStore.activeMessageId = message.id;
  messagesStore.activeMessage = message;
  wrapper = mount(MessageReader, { attachTo: document.body });
  return wrapper;
}

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
  vi.mocked(api.listCalendars).mockResolvedValue([calendar]);
  vi.mocked(api.listCalendarImportTargets).mockResolvedValue([calendar]);
  vi.mocked(api.previewCalendarAttachment).mockResolvedValue(previews);
  vi.mocked(api.importCalendarAttachment).mockResolvedValue({
    imported: 1,
    skipped_existing: 0,
  });
});

afterEach(() => {
  wrapper?.unmount();
  wrapper = null;
  document.body.innerHTML = "";
});

describe("MessageReader calendar attachments", () => {
  it("opens a grouped import dialog for a case-insensitive .ics filename", async () => {
    const reader = mountReader({
      index: 4,
      filename: "MEETING.ICS",
      content_type: "application/octet-stream",
      size: 1024,
    });

    await reader.get('[data-testid="attachment-4"]').trigger("click");
    await flushPromises();

    expect(api.saveAttachment).not.toHaveBeenCalled();
    expect(api.previewCalendarAttachment).toHaveBeenCalledWith(
      "acc1",
      "message1",
      4,
      "cal1",
    );
    expect(document.body.querySelector('[data-testid="calendar-import-dialog"]'))
      .not.toBeNull();
    expect(document.body.textContent).toContain("Weekly planning");
    expect(document.body.textContent).toContain("1 exception");

    const second = document.body.querySelector<HTMLInputElement>(
      '[data-testid="calendar-import-event-single@example.test"]',
    );
    second?.click();
    await flushPromises();
    document.body
      .querySelector<HTMLButtonElement>('[data-testid="calendar-import-submit"]')
      ?.click();
    await flushPromises();

    expect(api.importCalendarAttachment).toHaveBeenCalledWith(
      "acc1",
      "message1",
      4,
      "cal1",
      ["series@example.test"],
    );
  });

  it("recognizes text/calendar and retains an explicit download action", async () => {
    const reader = mountReader({
      index: 2,
      filename: "invite.dat",
      content_type: "text/calendar; charset=utf-8",
      size: 512,
    });

    await reader.get('[data-testid="attachment-2"]').trigger("click");
    await flushPromises();
    const download = Array.from(document.body.querySelectorAll("button")).find(
      (button) => button.textContent?.includes("Download instead"),
    ) as HTMLButtonElement | undefined;
    download?.click();
    await flushPromises();

    expect(api.saveAttachment).toHaveBeenCalledWith(
      "acc1",
      "message1",
      2,
      "invite.dat",
    );
  });

  it("continues to save ordinary attachments directly", async () => {
    const reader = mountReader({
      index: 1,
      filename: "notes.txt",
      content_type: "text/plain",
      size: 32,
    });

    await reader.get('[data-testid="attachment-1"]').trigger("click");
    await flushPromises();

    expect(api.previewCalendarAttachment).not.toHaveBeenCalled();
    expect(api.saveAttachment).toHaveBeenCalledWith(
      "acc1",
      "message1",
      1,
      "notes.txt",
    );
  });

  it("names the import dialog for assistive technology", async () => {
    const reader = mountReader({
      index: 0,
      filename: "event.ics",
      content_type: "text/calendar",
      size: 100,
    });

    await reader.get('[data-testid="attachment-0"]').trigger("click");
    await flushPromises();

    expect(
      document.body
        .querySelector('[role="dialog"]')
        ?.getAttribute("aria-label"),
    ).toBe("Import calendar events");
  });

  it("renders all-day dates without shifting display timezones", async () => {
    vi.mocked(api.previewCalendarAttachment).mockResolvedValue([
      {
        ...previews[0],
        all_day: true,
        start_time: "2026-01-15T00:00:00Z",
        end_time: "2026-01-16T00:00:00Z",
      },
    ]);
    const uiStore = useUiStore();
    uiStore.displayTimezone = "America/Los_Angeles";
    const reader = mountReader({
      index: 0,
      filename: "event.ics",
      content_type: "text/calendar",
      size: 100,
    });

    await reader.get('[data-testid="attachment-0"]').trigger("click");
    await flushPromises();

    expect(document.body.textContent).toContain("January 15, 2026");
    expect(document.body.textContent).not.toContain("January 14, 2026");
  });

  it("prefers the source default and reassesses a changed target", async () => {
    const otherCalendar: Calendar = {
      ...calendar,
      id: "cal2",
      account_id: "acc2",
      name: "Work",
    };
    vi.mocked(api.listCalendarImportTargets).mockResolvedValue([
      otherCalendar,
      calendar,
    ]);
    vi.mocked(api.previewCalendarAttachment).mockImplementation(
      async (_accountId, _messageId, _attachmentIndex, calendarId) =>
        calendarId === otherCalendar.id
          ? [
              {
                ...previews[0],
                importable: false,
                import_error:
                  "Recurring events cannot be imported into this provider.",
              },
            ]
          : previews,
    );
    const reader = mountReader({
      index: 0,
      filename: "event.ics",
      content_type: "text/calendar",
      size: 100,
    });

    await reader.get('[data-testid="attachment-0"]').trigger("click");
    await flushPromises();
    expect(api.previewCalendarAttachment).toHaveBeenLastCalledWith(
      "acc1",
      "message1",
      0,
      "cal1",
    );

    const select = document.body.querySelector<HTMLButtonElement>(
      '[data-testid="calendar-import-calendar"]',
    );
    select?.click();
    await flushPromises();
    const workOption = Array.from(
      document.body.querySelectorAll<HTMLElement>('[role="option"]'),
    ).find((option) => option.textContent?.includes("Work"));
    expect(workOption).toBeDefined();
    workOption?.dispatchEvent(
      new MouseEvent("mousedown", { bubbles: true, cancelable: true }),
    );
    await flushPromises();

    expect(api.previewCalendarAttachment).toHaveBeenLastCalledWith(
      "acc1",
      "message1",
      0,
      "cal2",
    );
    expect(document.body.textContent).toContain(
      "Recurring events cannot be imported into this provider.",
    );
    expect(
      document.body.querySelector<HTMLInputElement>(
        '[data-testid="calendar-import-event-series@example.test"]',
      )?.disabled,
    ).toBe(true);
  });
});
