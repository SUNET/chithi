import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

vi.mock("@/lib/tauri", () => ({
  listContactBooks: vi.fn(),
  searchContacts: vi.fn(),
  createContact: vi.fn().mockResolvedValue("new-id"),
  updateContact: vi.fn().mockResolvedValue(undefined),
  getEmailInvites: vi.fn().mockResolvedValue([]),
}));
vi.mock("@/lib/compose-window", () => ({
  openComposeWindow: vi.fn(),
}));

import MessageReader from "@/components/mail/MessageReader.vue";
import { useAccountsStore } from "@/stores/accounts";
import { useMessagesStore } from "@/stores/messages";
import * as api from "@/lib/tauri";
import type { Contact, ContactBook, MessageBody } from "@/lib/types";

const books: ContactBook[] = [
  { id: "b1", account_id: "acc1", name: "Personal", remote_id: null, sync_type: "jmap" },
];

const message: MessageBody = {
  id: "m1",
  subject: "Hello",
  from: { email: "ada@x.org", name: "Ada Lovelace" },
  to: [],
  cc: [],
  date: "2026-07-16T10:00:00Z",
  flags: [],
  body_html: null,
  body_text: "hi",
  attachments: [],
  is_encrypted: false,
  is_signed: false,
  list_id: null,
  has_remote_images: false,
};

const existingContact: Contact = {
  id: "c1",
  book_id: "b1",
  uid: null,
  display_name: "Ada Lovelace",
  emails_json: '[{"email":"ada@x.org","label":"work"}]',
  phones_json: "[]",
  addresses_json: "[]",
  organization: null,
  title: null,
  notes: null,
  vcard_data: null,
  remote_id: null,
  etag: null,
};

function mountReader() {
  const accountsStore = useAccountsStore();
  accountsStore.accounts = [
    {
      id: "acc1",
      display_name: "Acc",
      email: "a@x.org",
      username: "a@x.org",
      provider: "generic" as const,
      mail_protocol: "jmap" as const,
      enabled: true,
      mail_sync_interval_seconds: null,
      calendar_sync_interval_seconds: null,
      contacts_sync_interval_seconds: null,
      has_calendar_binding: true,
      has_contacts_binding: true,
      meet_protocol: "" as const,
    },
  ];
  accountsStore.activeAccountId = "acc1";
  const messagesStore = useMessagesStore();
  messagesStore.activeMessageId = "m1";
  messagesStore.activeMessage = message;
  return mount(MessageReader, { attachTo: document.body });
}

function bodyEl(selector: string): HTMLElement | null {
  return document.body.querySelector(selector);
}

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
  vi.mocked(api.listContactBooks).mockResolvedValue(books);
  vi.mocked(api.searchContacts).mockResolvedValue([]);
});

afterEach(() => {
  document.body.innerHTML = "";
});

describe("MessageReader address → contact form", () => {
  it("right-click on an unknown address opens the prefilled new-contact form", async () => {
    const wrapper = mountReader();
    await flushPromises();

    await wrapper
      .find('[data-testid="reader-from"] .addr-clickable')
      .trigger("contextmenu", { clientX: 10, clientY: 10 });
    await flushPromises();

    const ctxItem = bodyEl(".addr-context-menu .ctx-item")!;
    expect(ctxItem.textContent).toContain("Add to Contacts");
    ctxItem.click();
    await flushPromises();

    // Shared ContactFormModal, teleported to body, prefilled from the
    // clicked address ("Ada Lovelace" <ada@x.org>).
    const inputs = Array.from(
      document.body.querySelectorAll<HTMLInputElement>(".modal input"),
    );
    const values = inputs.map((i) => i.value);
    expect(values).toContain("Ada");
    expect(values).toContain("Lovelace");
    expect(values).toContain("ada@x.org");

    bodyEl('[data-testid="contact-save-btn"]')!.click();
    await flushPromises();

    expect(api.createContact).toHaveBeenCalledTimes(1);
    const payload = vi.mocked(api.createContact).mock.calls[0][0];
    expect(payload.book_id).toBe("b1");
    expect(payload.display_name).toBe("Ada Lovelace");
    expect(payload.emails_json).toContain("ada@x.org");
  });

  it("right-click on a known address offers Edit Contact and updates it", async () => {
    vi.mocked(api.searchContacts).mockResolvedValue([existingContact]);
    const wrapper = mountReader();
    await flushPromises();

    await wrapper
      .find('[data-testid="reader-from"] .addr-clickable')
      .trigger("contextmenu", { clientX: 10, clientY: 10 });
    await flushPromises();

    const ctxItem = bodyEl(".addr-context-menu .ctx-item")!;
    expect(ctxItem.textContent).toContain("Edit Contact");
    ctxItem.click();
    await flushPromises();

    bodyEl('[data-testid="contact-save-btn"]')!.click();
    await flushPromises();

    expect(api.createContact).not.toHaveBeenCalled();
    expect(api.updateContact).toHaveBeenCalledTimes(1);
    const payload = vi.mocked(api.updateContact).mock.calls[0][0];
    // Identity fields survive the edit spread.
    expect(payload.id).toBe("c1");
    expect(payload.book_id).toBe("b1");
  });
});
