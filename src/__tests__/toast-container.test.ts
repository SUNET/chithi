import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import ToastContainer from "@/components/common/ToastContainer.vue";
import { dismissToast, showToast } from "@/lib/toast";

describe("ToastContainer accessibility", () => {
  let wrapper: VueWrapper;
  const toastIds: number[] = [];

  beforeEach(() => {
    wrapper = mount(ToastContainer);
  });

  afterEach(() => {
    for (const id of toastIds.splice(0)) dismissToast(id);
    wrapper.unmount();
  });

  it("announces errors assertively and other messages as status", async () => {
    toastIds.push(showToast("Delivery status unknown", "error", 0));
    toastIds.push(showToast("Queued for retry", "info", 0));
    await nextTick();

    const errorToast = document.body.querySelector(".toast.error");
    const infoToast = document.body.querySelector(".toast.info");
    expect(errorToast?.getAttribute("role")).toBe("alert");
    expect(errorToast?.getAttribute("aria-atomic")).toBe("true");
    expect(infoToast?.getAttribute("role")).toBe("status");
    expect(
      errorToast?.querySelector(".toast-icon")?.getAttribute("aria-hidden"),
    ).toBe("true");
  });
});
