import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";

vi.mock("@/lib/tauri", () => ({
  updateContact: vi.fn().mockResolvedValue(undefined),
  deleteContact: vi.fn().mockResolvedValue(undefined),
}));

import MergeDialog from "@/components/contacts/MergeDialog.vue";
import * as api from "@/lib/tauri";
import type { Contact } from "@/lib/types";

function contact(id: string, name: string, emails: string): Contact {
  return {
    id,
    book_id: "b1",
    uid: `uid-${id}`,
    display_name: name,
    emails_json: emails,
    phones_json: "[]",
    addresses_json: "[]",
    organization: null,
    title: null,
    notes: null,
    vcard_data: null,
    remote_id: `remote-${id}`,
    etag: `etag-${id}`,
  };
}

const keeper = contact("k", "Ada Lovelace", '[{"email":"ada@x.org","label":"work"}]');
const loser = contact("l", "A. Lovelace", '[{"email":"al@y.org","label":"home"}]');

function bodyEl(selector: string): HTMLElement | null {
  return document.body.querySelector(selector);
}

function mountDialog() {
  return mount(MergeDialog, {
    props: { pair: { keeper, loser } },
    attachTo: document.body,
  });
}

beforeEach(() => {
  vi.clearAllMocks();
});

afterEach(() => {
  document.body.innerHTML = "";
});

describe("MergeDialog", () => {
  it("merging with the loser's name keeps the keeper's identity", async () => {
    const wrapper = mountDialog();
    await flushPromises();

    (bodyEl('[data-testid="merge-name-loser"]') as HTMLInputElement).click();
    await flushPromises();
    bodyEl('[data-testid="merge-confirm-btn"]')!.click();
    await flushPromises();

    expect(api.updateContact).toHaveBeenCalledTimes(1);
    const surviving = vi.mocked(api.updateContact).mock.calls[0][0];
    expect(surviving.display_name).toBe("A. Lovelace");
    expect(surviving.id).toBe("k");
    expect(surviving.remote_id).toBe("remote-k");
    expect(api.deleteContact).toHaveBeenCalledWith("l");
    expect(wrapper.emitted("merged")).toHaveLength(1);
  });

  it("an update failure keeps the dialog open and never deletes", async () => {
    vi.mocked(api.updateContact).mockRejectedValueOnce(new Error("server said no"));
    const wrapper = mountDialog();
    await flushPromises();

    bodyEl('[data-testid="merge-confirm-btn"]')!.click();
    await flushPromises();

    expect(api.deleteContact).not.toHaveBeenCalled();
    expect(bodyEl('[data-testid="merge-error"]')!.textContent).toContain("server said no");
    expect(bodyEl('[data-testid="merge-dialog"]')).toBeTruthy();
    expect(wrapper.emitted("merged")).toBeUndefined();
  });

  it("unchecking a loser email excludes it from the surviving contact", async () => {
    mountDialog();
    await flushPromises();

    const checkboxes = Array.from(
      document.body.querySelectorAll<HTMLInputElement>(
        '[data-testid="merge-field-emails"] input[type="checkbox"]',
      ),
    );
    expect(checkboxes).toHaveLength(2);
    // Second row is the loser's email (keeper items list first).
    checkboxes[1].click();
    await flushPromises();

    bodyEl('[data-testid="merge-confirm-btn"]')!.click();
    await flushPromises();

    const surviving = vi.mocked(api.updateContact).mock.calls[0][0];
    expect(surviving.emails_json).toContain("ada@x.org");
    expect(surviving.emails_json).not.toContain("al@y.org");
  });

  it("cancel emits without touching the API", async () => {
    const wrapper = mountDialog();
    await flushPromises();
    (document.body.querySelector(".modal-close") as HTMLElement).click();
    await flushPromises();
    expect(wrapper.emitted("cancel")).toHaveLength(1);
    expect(api.updateContact).not.toHaveBeenCalled();
  });
});
