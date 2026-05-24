import { describe, it, expect, vi, beforeEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";
import { nextTick } from "vue";

// Capture the listener callback so tests can synthesise backend events.
let capturedListener: ((event: { payload: unknown }) => void) | null = null;

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((_evt: string, cb: (e: { payload: unknown }) => void) => {
    capturedListener = cb;
    return Promise.resolve(() => {
      capturedListener = null;
    });
  }),
}));

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

import { usePgpPromptsStore } from "@/stores/pgp-prompts";
import type { PgpSecretPrompt } from "@/stores/pgp-prompts";
import PassphraseDialog from "@/components/pgp/PassphraseDialog.vue";
import PinDialog from "@/components/pgp/PinDialog.vue";

function makePrompt(
  kind: PgpSecretPrompt["kind"],
  requestId = "req-1",
): PgpSecretPrompt {
  return {
    requestId,
    kind,
    target:
      kind === "passphrase"
        ? "DEAD" + "0".repeat(36)
        : "0006:DEADBEEF",
    reason: `Test prompt for ${kind}`,
  };
}

beforeEach(async () => {
  setActivePinia(createPinia());
  capturedListener = null;
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  // Reset the listen mock's call history so per-test assertions about
  // "how many times did we subscribe" aren't polluted by prior tests in
  // this file. The implementation (capturedListener capture) stays.
  const { listen } = await import("@tauri-apps/api/event");
  vi.mocked(listen).mockClear();
});

describe("pgp prompts store", () => {
  it("queues prompts FIFO as events arrive", async () => {
    const store = usePgpPromptsStore();
    await store.start();
    expect(capturedListener).toBeTruthy();

    capturedListener!({ payload: makePrompt("passphrase", "r1") });
    capturedListener!({ payload: makePrompt("pin", "r2") });
    await nextTick();

    expect(store.queue).toHaveLength(2);
    expect(store.currentPrompt?.requestId).toBe("r1");
  });

  it("provide() invokes the backend, then drops the request from the queue", async () => {
    const store = usePgpPromptsStore();
    await store.start();
    capturedListener!({ payload: makePrompt("passphrase", "r1") });
    await nextTick();

    await store.provide("r1", "hunter2");

    expect(invokeMock).toHaveBeenCalledWith("pgp_provide_secret", {
      requestId: "r1",
      value: "hunter2",
    });
    expect(store.queue).toHaveLength(0);
    expect(store.currentPrompt).toBeNull();
  });

  it("cancel() invokes the backend and clears the queue entry", async () => {
    const store = usePgpPromptsStore();
    await store.start();
    capturedListener!({ payload: makePrompt("pin", "r9") });
    await nextTick();

    await store.cancel("r9");

    expect(invokeMock).toHaveBeenCalledWith("pgp_cancel_secret", {
      requestId: "r9",
    });
    expect(store.queue).toHaveLength(0);
  });

  it("start() is idempotent", async () => {
    const store = usePgpPromptsStore();
    await store.start();
    await store.start();
    // listen should only have been called once
    const { listen } = await import("@tauri-apps/api/event");
    expect(vi.mocked(listen)).toHaveBeenCalledOnce();
  });
});

describe("PassphraseDialog", () => {
  it("clears the input ref AND the DOM input value after submit", async () => {
    const prompt = makePrompt("passphrase", "r1");
    const wrapper = mount(PassphraseDialog, { props: { prompt } });
    const input = wrapper.find('[data-testid="pgp-passphrase-input"]');
    await input.setValue("super-secret");
    expect((input.element as HTMLInputElement).value).toBe("super-secret");

    await wrapper.find('[data-testid="pgp-passphrase-submit"]').trigger("click");
    await flushPromises();

    // Vue ref is back to "" (drives v-model -> DOM .value).
    expect((input.element as HTMLInputElement).value).toBe("");
    // Backend got the value we entered before the wipe.
    expect(invokeMock).toHaveBeenCalledWith("pgp_provide_secret", {
      requestId: "r1",
      value: "super-secret",
    });
  });

  it("cancel() wipes the input even when the user typed something", async () => {
    const prompt = makePrompt("passphrase", "r2");
    const wrapper = mount(PassphraseDialog, { props: { prompt } });
    const input = wrapper.find('[data-testid="pgp-passphrase-input"]');
    await input.setValue("typed-but-cancelled");

    // Trigger cancel via the overlay click (calls cancel()).
    await wrapper.find(".overlay").trigger("click");
    await flushPromises();

    expect((input.element as HTMLInputElement).value).toBe("");
    expect(invokeMock).toHaveBeenCalledWith("pgp_cancel_secret", {
      requestId: "r2",
    });
  });

  it("refuses submit on empty input", async () => {
    const prompt = makePrompt("passphrase", "r3");
    const wrapper = mount(PassphraseDialog, { props: { prompt } });
    await wrapper.find('[data-testid="pgp-passphrase-submit"]').trigger("click");
    await flushPromises();

    expect(invokeMock).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("Enter the passphrase.");
  });
});

describe("PinDialog", () => {
  it("clears the PIN buffer on submit", async () => {
    const prompt = makePrompt("pin", "p1");
    const wrapper = mount(PinDialog, { props: { prompt } });
    const input = wrapper.find('[data-testid="pgp-pin-input"]');
    await input.setValue("12345678");
    await wrapper.find('[data-testid="pgp-pin-submit"]').trigger("click");
    await flushPromises();

    expect((input.element as HTMLInputElement).value).toBe("");
    expect(invokeMock).toHaveBeenCalledWith("pgp_provide_secret", {
      requestId: "p1",
      value: "12345678",
    });
  });

  it("rejects PIN shorter than 4 digits", async () => {
    const prompt = makePrompt("pin", "p2");
    const wrapper = mount(PinDialog, { props: { prompt } });
    const input = wrapper.find('[data-testid="pgp-pin-input"]');
    await input.setValue("12");
    await wrapper.find('[data-testid="pgp-pin-submit"]').trigger("click");
    await flushPromises();

    expect(invokeMock).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("PIN must be at least 4 digits.");
  });
});
