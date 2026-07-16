import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@/lib/tauri", () => ({
  listContacts: vi.fn(),
}));

import MobileContactsPane from "@/components/contacts/MobileContactsPane.vue";
import { useAccountsStore } from "@/stores/accounts";
import * as api from "@/lib/tauri";
import type { Contact, ContactBook } from "@/lib/types";

const books: ContactBook[] = [
  { id: "b1", account_id: "acc1", name: "Personal", remote_id: null, sync_type: "jmap" },
  { id: "b2", account_id: "acc2", name: "Work", remote_id: null, sync_type: "carddav" },
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

// "9lives" exercises the "#" bucket: non-letter initials group under
// "#", and that group sorts last even though "9" < "A" in codepoints.
const b1Contacts = [contact("a", "Ada", "b1"), contact("n", "9lives", "b1")];
const b2Contacts = [contact("b", "Bob", "b2")];

function account(id: string, name: string) {
  return {
    id,
    display_name: name,
    email: `${id}@x.org`,
    username: `${id}@x.org`,
    provider: "generic" as const,
    mail_protocol: "jmap" as const,
    enabled: true,
    mail_sync_interval_seconds: null,
    calendar_sync_interval_seconds: null,
    contacts_sync_interval_seconds: null,
    has_calendar_binding: true,
    has_contacts_binding: true,
    meet_protocol: "" as const,
  };
}

/// Mount with an empty book list, then swap in the real one: the pane
/// (like the pre-split view) loads on `books` *changes*, not on mount —
/// the parent's fetchBooks always mutates the array post-mount.
async function mountPane() {
  const store = useAccountsStore();
  store.accounts = [account("acc1", "One"), account("acc2", "Two")];
  const wrapper = mount(MobileContactsPane, {
    props: { books: [], search: "" },
  });
  await wrapper.setProps({ books });
  await flushPromises();
  return wrapper;
}

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
  vi.mocked(api.listContacts).mockImplementation(async (bookId: string) =>
    bookId === "b1" ? b1Contacts : b2Contacts,
  );
});

afterEach(() => {
  document.body.innerHTML = "";
});

describe("MobileContactsPane", () => {
  it("groups contacts by letter with the # bucket last", async () => {
    const wrapper = await mountPane();

    const headers = wrapper.findAll(".letter-header").map((h) => h.text());
    expect(headers).toEqual(["A", "B", "#"]);
    expect(wrapper.findAll(".mobile-row")).toHaveLength(3);
  });

  it("an account chip narrows the list to that account's contacts", async () => {
    const wrapper = await mountPane();

    const chips = wrapper.findAll(".chip");
    // [All, One, Two]
    expect(chips.map((c) => c.text())).toEqual(["All", "One", "Two"]);
    await chips[2].trigger("click");

    const names = wrapper.findAll(".mobile-row-name").map((n) => n.text());
    expect(names).toEqual(["Bob"]);
  });

  it("clicking a row emits select with the contact", async () => {
    const wrapper = await mountPane();

    await wrapper.find(".mobile-row").trigger("click");

    const emitted = wrapper.emitted("select");
    expect(emitted).toHaveLength(1);
    expect((emitted![0][0] as Contact).id).toBe("a"); // group A renders first
  });
});
