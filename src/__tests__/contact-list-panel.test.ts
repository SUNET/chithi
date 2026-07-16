import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import ContactListPanel from "@/components/contacts/ContactListPanel.vue";
import type { Contact, ContactBook } from "@/lib/types";

setActivePinia(createPinia());

const books: ContactBook[] = [
  { id: "b1", account_id: "acc1", name: "Personal", remote_id: null, sync_type: "jmap" },
  { id: "b2", account_id: "acc2", name: "Work", remote_id: null, sync_type: "carddav" },
];

function contact(id: string, name: string, bookId = "b1", org: string | null = null): Contact {
  return {
    id,
    book_id: bookId,
    uid: null,
    display_name: name,
    emails_json: `[{"email":"${id}@x.org","label":"work"}]`,
    phones_json: "[]",
    addresses_json: "[]",
    organization: org,
    title: null,
    notes: null,
    vcard_data: null,
    remote_id: null,
    etag: null,
  };
}

function mountPanel(overrides: Record<string, unknown> = {}) {
  return mount(ContactListPanel, {
    props: {
      contacts: [contact("a", "Ada"), contact("b", "Bob", "b1", "Acme"), contact("c", "Cid", "b2")],
      books,
      selectedContactId: null,
      selectedIds: [],
      hasBook: true,
      search: "",
      "onUpdate:search": () => {},
      ...overrides,
    },
  });
}

describe("ContactListPanel", () => {
  it("filters by name, email and organization", async () => {
    const wrapper = mountPanel({ search: "acme" });
    expect(wrapper.findAll(".contact-row")).toHaveLength(1);
    expect(wrapper.find(".contact-name").text()).toBe("Bob");

    await wrapper.setProps({ search: "c@x.org" });
    expect(wrapper.findAll(".contact-row")).toHaveLength(1);
    expect(wrapper.find(".contact-name").text()).toBe("Cid");
  });

  it("select emit carries the contact and the mouse event", async () => {
    const wrapper = mountPanel();
    await wrapper.find('[data-testid="contact-b"]').trigger("click", { ctrlKey: true });
    const [emitted] = wrapper.emitted("select")!;
    expect((emitted[0] as Contact).id).toBe("b");
    expect((emitted[1] as MouseEvent).ctrlKey).toBe(true);
  });

  it("merge is enabled for two same-book picks and carries keeper first", async () => {
    const wrapper = mountPanel({ selectedIds: ["b", "a"] });
    const btn = wrapper.find('[data-testid="merge-toolbar-btn"]');
    expect((btn.element as HTMLButtonElement).disabled).toBe(false);
    await btn.trigger("click");
    const [pair] = wrapper.emitted("merge")!;
    expect((pair[0] as Contact).id).toBe("b");
    expect((pair[1] as Contact).id).toBe("a");
  });

  it("merge is disabled across books", () => {
    const wrapper = mountPanel({ selectedIds: ["a", "c"] });
    const btn = wrapper.find('[data-testid="merge-toolbar-btn"]');
    expect((btn.element as HTMLButtonElement).disabled).toBe(true);
  });
});
