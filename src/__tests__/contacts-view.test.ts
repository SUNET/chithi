import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

const { listenMock } = vi.hoisted(() => ({
  listenMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: listenMock,
}));

vi.mock("@/lib/tauri", () => ({
  listContactBooks: vi.fn(),
  listContacts: vi.fn(),
  syncContacts: vi.fn().mockResolvedValue(undefined),
  deleteContact: vi.fn().mockResolvedValue(undefined),
  updateContact: vi.fn().mockResolvedValue(undefined),
  createContact: vi.fn().mockResolvedValue("new-id"),
  openLink: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("@/lib/compose-window", () => ({
  openComposeWindow: vi.fn(),
}));

import ContactsView from "@/views/ContactsView.vue";
import { useAccountsStore } from "@/stores/accounts";
import * as api from "@/lib/tauri";
import type { Contact, ContactBook } from "@/lib/types";

const books: ContactBook[] = [
  { id: "b1", account_id: "acc1", name: "Personal", remote_id: null, sync_type: "jmap" },
  { id: "b2", account_id: "acc1", name: "Work", remote_id: null, sync_type: "jmap" },
];

function contact(id: string, name: string, bookId: string): Contact {
  return {
    id,
    book_id: bookId,
    uid: null,
    display_name: name,
    emails_json: "[]",
    phones_json: "[]",
    addresses_json: "[]",
    organization: null,
    title: null,
    notes: null,
    vcard_data: null,
    remote_id: null,
    etag: null,
  };
}

const b1Contacts = [contact("a", "Ada", "b1"), contact("b", "Bob", "b1")];
const b2Contacts = [contact("c", "Cid", "b2")];

let contactsChangedHandler: (() => Promise<void>) | null = null;

function mountView() {
  const store = useAccountsStore();
  store.accounts = [
    {
      id: "acc1",
      display_name: "Acc",
      email: "a@x.org",
      username: "a@x.org",
      provider: "generic",
      mail_protocol: "jmap",
      enabled: true,
      mail_sync_interval_seconds: null,
      calendar_sync_interval_seconds: null,
      contacts_sync_interval_seconds: null,
      has_calendar_binding: true,
      has_contacts_binding: true,
      meet_protocol: "",
    },
  ];
  return mount(ContactsView, { attachTo: document.body });
}

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
  vi.mocked(api.listContactBooks).mockResolvedValue(books);
  vi.mocked(api.listContacts).mockImplementation(async (bookId: string) =>
    bookId === "b1" ? b1Contacts : b2Contacts,
  );
  contactsChangedHandler = null;
  listenMock.mockImplementation(async (event: string, handler: () => Promise<void>) => {
    if (event === "contacts-changed") contactsChangedHandler = handler;
    return () => {};
  });
});

afterEach(() => {
  document.body.innerHTML = "";
  vi.useRealTimers();
});

describe("ContactsView (desktop)", () => {
  it("renders books and loads the first book's contacts", async () => {
    const wrapper = mountView();
    await flushPromises();

    expect(wrapper.findAll(".book-item")).toHaveLength(2);
    expect(api.listContacts).toHaveBeenCalledWith("b1");
    expect(wrapper.findAll(".contact-row")).toHaveLength(2);
  });

  it("switching books loads that book's contacts", async () => {
    const wrapper = mountView();
    await flushPromises();

    await wrapper.findAll(".book-item")[1].trigger("click");
    await flushPromises();

    expect(api.listContacts).toHaveBeenCalledWith("b2");
    expect(wrapper.find(".contact-name").text()).toBe("Cid");
  });

  it("clicking a contact shows it in the detail panel", async () => {
    const wrapper = mountView();
    await flushPromises();

    await wrapper.find('[data-testid="contact-b"]').trigger("click");
    expect(wrapper.find('[data-testid="contact-detail-name"]').text()).toBe("Bob");
  });

  it("ctrl-clicking two same-book contacts arms the merge toolbar", async () => {
    const wrapper = mountView();
    await flushPromises();

    await wrapper.find('[data-testid="contact-a"]').trigger("click", { ctrlKey: true });
    await wrapper.find('[data-testid="contact-b"]').trigger("click", { ctrlKey: true });

    const btn = wrapper.find('[data-testid="merge-toolbar-btn"]');
    expect(btn.exists()).toBe(true);
    expect((btn.element as HTMLButtonElement).disabled).toBe(false);
  });

  it("the contacts-changed event refetches books and the open book", async () => {
    mountView();
    await flushPromises();
    expect(contactsChangedHandler).not.toBeNull();
    vi.mocked(api.listContactBooks).mockClear();
    vi.mocked(api.listContacts).mockClear();

    await contactsChangedHandler!();

    expect(api.listContactBooks).toHaveBeenCalled();
    expect(api.listContacts).toHaveBeenCalledWith("b1");
  });
});
