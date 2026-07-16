import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@/lib/tauri", () => ({
  createContact: vi.fn().mockResolvedValue("new-id"),
  updateContact: vi.fn().mockResolvedValue(undefined),
}));

import ContactFormModal from "@/components/contacts/ContactFormModal.vue";
import * as api from "@/lib/tauri";
import type { Contact, ContactBook } from "@/lib/types";

const books: ContactBook[] = [
  { id: "b1", account_id: "acc1", name: "Personal", remote_id: null, sync_type: "jmap" },
  { id: "b2", account_id: "acc1", name: "Work", remote_id: null, sync_type: "jmap" },
];

const existing: Contact = {
  id: "c1",
  book_id: "b1",
  uid: "uid-1",
  display_name: "Ada King Lovelace",
  emails_json: '[{"email":"ada@x.org","label":"work"}]',
  phones_json: "[]",
  addresses_json: "[]",
  organization: "Analytical Engines",
  title: null,
  notes: null,
  vcard_data: "BEGIN:VCARD...",
  remote_id: "remote-1",
  etag: "etag-1",
};

// The modal teleports to <body>.
function bodyEl(selector: string): HTMLElement | null {
  return document.body.querySelector(selector);
}

type ModalHandle = {
  openNew: (bookId: string, prefill?: { firstName?: string; email?: string }) => void;
  openEdit: (contact: Contact) => void;
};

function mountModal(compact = false) {
  const wrapper = mount(ContactFormModal, {
    props: { books, compact },
    attachTo: document.body,
  });
  return { wrapper, vm: wrapper.vm as unknown as ModalHandle };
}

async function setInput(placeholder: string, value: string) {
  const input = Array.from(
    document.body.querySelectorAll<HTMLInputElement>(".modal input"),
  ).find((i) => i.placeholder === placeholder)!;
  input.value = value;
  input.dispatchEvent(new Event("input"));
  await flushPromises();
}

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
});

afterEach(() => {
  document.body.innerHTML = "";
});

describe("ContactFormModal", () => {
  it("requires first and last name before calling the API", async () => {
    const { vm } = mountModal();
    vm.openNew("b1");
    await flushPromises();

    bodyEl('[data-testid="contact-save-btn"]')!.click();
    await flushPromises();
    expect(document.body.querySelector(".form-error")!.textContent).toContain(
      "First name is required",
    );

    await setInput("First", "Ada");
    bodyEl('[data-testid="contact-save-btn"]')!.click();
    await flushPromises();
    expect(document.body.querySelector(".form-error")!.textContent).toContain(
      "Last name is required",
    );
    expect(api.createContact).not.toHaveBeenCalled();
  });

  it("creates with joined display name and blank entries filtered", async () => {
    const { wrapper, vm } = mountModal();
    vm.openNew("b1", { email: "ada@x.org" });
    await flushPromises();

    await setInput("First", "Ada");
    await setInput("Middle", "King");
    await setInput("Last", "Lovelace");
    // Add a second, blank email row — it must be filtered on save.
    const addEmail = Array.from(document.body.querySelectorAll(".add-btn")).find(
      (b) => b.textContent?.includes("Add email"),
    ) as HTMLElement;
    addEmail.click();
    await wrapper.vm.$nextTick();

    bodyEl('[data-testid="contact-save-btn"]')!.click();
    await flushPromises();

    expect(api.createContact).toHaveBeenCalledWith({
      book_id: "b1",
      display_name: "Ada King Lovelace",
      emails_json: '[{"email":"ada@x.org","label":"work"}]',
      phones_json: "[]",
      addresses_json: "[]",
      organization: null,
      title: null,
      notes: null,
    });
    expect(wrapper.emitted("saved")).toEqual([[null]]);
  });

  it("edit preserves identity fields via spread and applies a book change", async () => {
    const { wrapper, vm } = mountModal();
    vm.openEdit(existing);
    await flushPromises();

    const select = document.body.querySelector<HTMLSelectElement>(".modal select");
    if (select) {
      select.value = "b2";
      select.dispatchEvent(new Event("change"));
      await flushPromises();
    }

    bodyEl('[data-testid="contact-save-btn"]')!.click();
    await flushPromises();

    expect(api.updateContact).toHaveBeenCalledTimes(1);
    const arg = vi.mocked(api.updateContact).mock.calls[0][0];
    expect(arg.id).toBe("c1");
    expect(arg.uid).toBe("uid-1");
    expect(arg.remote_id).toBe("remote-1");
    expect(arg.etag).toBe("etag-1");
    expect(arg.vcard_data).toBe("BEGIN:VCARD...");
    expect(arg.display_name).toBe("Ada King Lovelace");
    expect(wrapper.emitted("saved")).toEqual([["c1"]]);
  });

  it("compact mode hides the desktop-only fields", async () => {
    const { vm } = mountModal(true);
    vm.openNew("b1");
    await flushPromises();

    const placeholders = Array.from(
      document.body.querySelectorAll<HTMLInputElement>(".modal input"),
    ).map((i) => i.placeholder);
    expect(placeholders).not.toContain("Middle");
    expect(placeholders).not.toContain("Job title");
    expect(document.body.querySelector(".modal textarea")).toBeNull();
  });
});
