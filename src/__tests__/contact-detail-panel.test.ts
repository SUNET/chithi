import { beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@/lib/tauri", () => ({
  openLink: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("@/lib/compose-window", () => ({
  openComposeWindow: vi.fn(),
}));

import ContactDetailPanel from "@/components/contacts/ContactDetailPanel.vue";
import * as api from "@/lib/tauri";
import { openComposeWindow } from "@/lib/compose-window";
import type { Contact, ContactBook } from "@/lib/types";

const books: ContactBook[] = [
  { id: "b1", account_id: "acc1", name: "Personal", remote_id: null, sync_type: "jmap" },
];

const contact: Contact = {
  id: "c1",
  book_id: "b1",
  uid: null,
  display_name: "Ada Lovelace",
  emails_json: '[{"email":"ada@x.org","label":"work"}]',
  phones_json: '[{"number":"+46 (70) 123 45 67 ext. 8","label":"mobile"}]',
  addresses_json: "[]",
  organization: "Analytical Engines",
  title: "Countess",
  notes: null,
  vcard_data: null,
  remote_id: null,
  etag: null,
};

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
});

describe("ContactDetailPanel", () => {
  it("shows the empty state without a contact", () => {
    const wrapper = mount(ContactDetailPanel, { props: { contact: null, books } });
    expect(wrapper.find(".empty-text").text()).toContain("Select a contact");
  });

  it("email click opens compose with the address prefilled", async () => {
    const wrapper = mount(ContactDetailPanel, { props: { contact, books } });
    expect(wrapper.find('[data-testid="contact-detail-name"]').text()).toBe("Ada Lovelace");
    await wrapper.find('[data-testid="contact-detail-email"]').trigger("click");
    expect(openComposeWindow).toHaveBeenCalledWith(
      expect.objectContaining({ to: "ada@x.org" }),
    );
  });

  it("phone click hands a sanitized tel: URI to the OS", async () => {
    const wrapper = mount(ContactDetailPanel, { props: { contact, books } });
    await wrapper.find('[data-testid="contact-detail-phone"]').trigger("click");
    // Spaces, parens and the "ext." suffix are stripped; digits, "+"
    // and visual separators survive.
    expect(api.openLink).toHaveBeenCalledWith("tel:+46701234567.8");
  });

  it("edit and delete emit with the contact", async () => {
    const wrapper = mount(ContactDetailPanel, { props: { contact, books } });
    await wrapper.find('[data-testid="contact-edit-btn"]').trigger("click");
    expect((wrapper.emitted("edit")![0][0] as Contact).id).toBe("c1");
    await wrapper.find('[data-testid="contact-delete-btn"]').trigger("click");
    expect(wrapper.emitted("delete")![0][0]).toBe("c1");
  });
});
