import { afterEach, describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import AccountTypePicker from "@/components/settings/AccountTypePicker.vue";
import { ADD_ACCOUNT_TYPES } from "@/lib/account-types";

// The picker teleports to <body>, so assertions go through
// document.body rather than wrapper.find.
function bodyEl(selector: string): HTMLElement | null {
  return document.body.querySelector(selector);
}

afterEach(() => {
  document.body.innerHTML = "";
});

describe("AccountTypePicker", () => {
  it("renders one card per account type when open", () => {
    mount(AccountTypePicker, { props: { open: true }, attachTo: document.body });
    for (const t of ADD_ACCOUNT_TYPES) {
      expect(bodyEl(`[data-testid="picker-${t}"]`)).toBeTruthy();
    }
  });

  it("renders nothing when closed", () => {
    mount(AccountTypePicker, { props: { open: false }, attachTo: document.body });
    expect(bodyEl('[data-testid="account-type-picker"]')).toBeNull();
  });

  it("emits pick with the clicked type", async () => {
    const wrapper = mount(AccountTypePicker, {
      props: { open: true },
      attachTo: document.body,
    });
    bodyEl('[data-testid="picker-fastmail"]')!.click();
    await wrapper.vm.$nextTick();
    expect(wrapper.emitted("pick")).toEqual([["fastmail"]]);
  });

  it("emits cancel from the close and cancel buttons", async () => {
    const wrapper = mount(AccountTypePicker, {
      props: { open: true },
      attachTo: document.body,
    });
    (bodyEl(".modal-close") as HTMLElement).click();
    await wrapper.vm.$nextTick();
    expect(wrapper.emitted("cancel")).toHaveLength(1);
  });
});
