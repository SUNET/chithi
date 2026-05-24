import { beforeEach, describe, expect, it, vi } from "vitest";
import { mount, flushPromises } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

// Backend mock — every method has a sensible default so the view's
// onMounted fetches resolve cleanly. Tests override `pgpCardDetails`
// per-case with `mockResolvedValueOnce`.
vi.mock("@/lib/tauri", () => ({
  pgpListKeys: vi.fn().mockResolvedValue([]),
  pgpListCards: vi.fn().mockResolvedValue([
    {
      ident: "0000:00000001",
      manufacturerName: "Testcard",
      serialNumber: "00000001",
      cardholderName: "Kushal Das",
    },
  ]),
  pgpCardDetails: vi.fn().mockResolvedValue({
    ident: "0000:00000001",
    serialNumber: "00000001",
    cardholderName: "Kushal Das",
    manufacturerName: "Testcard",
    publicKeyUrl: null,
    signatureFingerprint: "AAAA1111BBBB2222CCCC3333DDDD4444EEEE5555",
    encryptionFingerprint: null,
    authenticationFingerprint: null,
    signatureCounter: 7,
    pinRetryCounter: 3,
    resetCodeRetryCounter: 0,
    adminPinRetryCounter: 3,
  }),
  pgpAutoLinkCards: vi.fn().mockResolvedValue([]),
  pgpImportKey: vi.fn(),
  pgpPickAndImportKey: vi.fn(),
  pgpDeleteKey: vi.fn(),
  pgpExportPublic: vi.fn(),
  pgpWkdFetch: vi.fn(),
  pgpDecryptMessage: vi.fn(),
  pgpVerifyMessage: vi.fn(),
}));

import OpenPGPView from "@/views/OpenPGPView.vue";
import * as api from "@/lib/tauri";

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
  vi.mocked(api.pgpListCards).mockResolvedValue([
    {
      ident: "0000:00000001",
      manufacturerName: "Testcard",
      serialNumber: "00000001",
      cardholderName: "Kushal Das",
    },
  ]);
  vi.mocked(api.pgpCardDetails).mockResolvedValue({
    ident: "0000:00000001",
    serialNumber: "00000001",
    cardholderName: "Kushal Das",
    manufacturerName: "Testcard",
    publicKeyUrl: null,
    signatureFingerprint: "AAAA1111BBBB2222CCCC3333DDDD4444EEEE5555",
    encryptionFingerprint: null,
    authenticationFingerprint: null,
    signatureCounter: 7,
    pinRetryCounter: 3,
    resetCodeRetryCounter: 0,
    adminPinRetryCounter: 3,
  });
});

describe("OpenPGPView — smartcard click → detail view", () => {
  it("clicking a card in the sidebar opens the detail pane with that card's fields", async () => {
    const wrapper = mount(OpenPGPView);
    // Wait for onMounted's fetchKeys + fetchCards to settle.
    await flushPromises();

    const card = wrapper.find('[data-testid="pgp-card-0000:00000001"]');
    expect(card.exists()).toBe(true);
    // No detail pane until a card is selected.
    expect(wrapper.find('[data-testid="pgp-card-detail"]').exists()).toBe(false);

    await card.trigger("click");
    await flushPromises();

    expect(api.pgpCardDetails).toHaveBeenCalledWith("0000:00000001");
    const detail = wrapper.find('[data-testid="pgp-card-detail"]');
    expect(detail.exists()).toBe(true);
    // Fields rendered. signatureCounter, locked reset-code, slot fingerprint.
    expect(detail.text()).toContain("Kushal Das");
    expect(detail.text()).toContain("Testcard");
    expect(detail.text()).toContain("7"); // signature counter
    expect(detail.text()).toContain("0 (locked)"); // reset-code retries = 0
    // Last 16 chex of the signature slot fingerprint, grouped 4-by-4.
    expect(detail.text()).toContain("DDDD 4444 EEEE 5555");
  });

  it("keyboard activation (Enter) on a card opens its detail view", async () => {
    const wrapper = mount(OpenPGPView);
    await flushPromises();
    const card = wrapper.find('[data-testid="pgp-card-0000:00000001"]');
    await card.trigger("keydown", { key: "Enter" });
    await flushPromises();
    expect(api.pgpCardDetails).toHaveBeenCalledWith("0000:00000001");
    expect(wrapper.find('[data-testid="pgp-card-detail"]').exists()).toBe(true);
  });

  it("surfaces a backend error in the detail pane without throwing", async () => {
    vi.mocked(api.pgpCardDetails).mockRejectedValueOnce(
      new Error("card not present"),
    );
    const wrapper = mount(OpenPGPView);
    await flushPromises();
    await wrapper
      .find('[data-testid="pgp-card-0000:00000001"]')
      .trigger("click");
    await flushPromises();
    const detail = wrapper.find('[data-testid="pgp-card-detail"]');
    expect(detail.exists()).toBe(true);
    expect(detail.text()).toContain("card not present");
  });
});
