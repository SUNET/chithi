import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount, type VueWrapper } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import type { OutboxRow } from "@/lib/types";

const {
  handlers,
  listOutbox,
  retryOutboxOp,
  discardOutboxOp,
  showToast,
} = vi.hoisted(() => ({
  handlers: new Map<string, (event: { payload: unknown }) => void>(),
  listOutbox: vi.fn(),
  retryOutboxOp: vi.fn(),
  discardOutboxOp: vi.fn(),
  showToast: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(
    (name: string, handler: (event: { payload: unknown }) => void) => {
      handlers.set(name, handler);
      return Promise.resolve(() => {});
    },
  ),
}));

vi.mock("@/lib/tauri", () => ({
  listOutbox,
  retryOutboxOp,
  discardOutboxOp,
}));

vi.mock("@/lib/toast", () => ({ showToast }));

import OutboxList from "@/components/mail/OutboxList.vue";
import { useAccountsStore } from "@/stores/accounts";

function makeRow(overrides: Partial<OutboxRow> = {}): OutboxRow {
  return {
    id: 1,
    account_id: "account-1",
    action_type: "send",
    status: "dead",
    delivery_outcome_unknown: false,
    retry_count: 3,
    error_message: "send failed",
    subject: "Status report",
    to: ["reader@example.com"],
    cc: [],
    bcc: [],
    ...overrides,
  };
}

describe("OutboxList indeterminate delivery", () => {
  let wrapper: VueWrapper | undefined;
  const confirm = vi.fn();

  async function mountList(rows: OutboxRow[]) {
    listOutbox.mockResolvedValue(rows);
    wrapper = mount(OutboxList);
    await flushPromises();
    return wrapper;
  }

  beforeEach(() => {
    setActivePinia(createPinia());
    useAccountsStore().activeAccountId = "account-1";
    handlers.clear();
    listOutbox.mockReset();
    retryOutboxOp.mockReset().mockResolvedValue(undefined);
    discardOutboxOp.mockReset().mockResolvedValue(undefined);
    showToast.mockReset();
    confirm.mockReset();
    vi.stubGlobal("confirm", confirm);
    wrapper = undefined;
  });

  afterEach(() => {
    wrapper?.unmount();
    vi.unstubAllGlobals();
  });

  it("labels unknown dead rows without changing definite failures", async () => {
    const view = await mountList([
      makeRow({ id: 1, delivery_outcome_unknown: true }),
      makeRow({ id: 2, subject: "Definite failure" }),
    ]);

    const unknown = view.get('[data-testid="outbox-item-1"]');
    expect(unknown.text()).toContain("Delivery status unknown");
    expect(unknown.text()).not.toContain("Failed");
    expect(view.get('[data-testid="outbox-item-2"]').text()).toContain(
      "Failed (3 attempts)",
    );
  });

  it("warns about duplicate delivery before retrying", async () => {
    const row = makeRow({ delivery_outcome_unknown: true });
    const view = await mountList([row]);
    confirm.mockReturnValue(false);

    await view.get('[data-testid="outbox-retry-1"]').trigger("click");

    expect(confirm).toHaveBeenCalledWith(
      expect.stringMatching(/may already have occurred.*duplicate.*Retry anyway/),
    );
    expect(retryOutboxOp).not.toHaveBeenCalled();

    confirm.mockReturnValue(true);
    await view.get('[data-testid="outbox-retry-1"]').trigger("click");
    await flushPromises();
    expect(retryOutboxOp).toHaveBeenCalledWith(1);
  });

  it("explains that discarding cannot cancel delivery", async () => {
    const view = await mountList([
      makeRow({ delivery_outcome_unknown: true }),
    ]);
    confirm.mockReturnValue(false);

    await view.get('[data-testid="outbox-discard-1"]').trigger("click");

    expect(confirm).toHaveBeenCalledWith(
      expect.stringMatching(/only removes the local record.*cannot cancel delivery/),
    );
    expect(discardOutboxOp).not.toHaveBeenCalled();
  });

  it("disables retry and discard while the row is sending", async () => {
    const view = await mountList([makeRow({ status: "sending" })]);

    expect(view.get('[data-testid="outbox-retry-1"]').attributes("disabled"))
      .toBeDefined();
    expect(view.get('[data-testid="outbox-discard-1"]').attributes("disabled"))
      .toBeDefined();
  });

  it("reloads the active account when a send becomes unknown", async () => {
    await mountList([makeRow()]);
    listOutbox.mockClear();

    const handler = handlers.get("send-unknown");
    expect(handler).toBeDefined();
    handler?.({ payload: { account_id: "account-1" } });
    await flushPromises();

    expect(listOutbox).toHaveBeenCalledOnce();
    expect(listOutbox).toHaveBeenCalledWith("account-1");
  });
});
