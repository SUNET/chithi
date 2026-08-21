import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount, type VueWrapper } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { nextTick } from "vue";
import type { OutboxRow } from "@/lib/types";

const {
  handlers,
  listOutbox,
  retryOutboxOp,
  discardOutboxOp,
  showToast,
  listenEvent,
} = vi.hoisted(() => ({
  handlers: new Map<string, (event: { payload: unknown }) => void>(),
  listOutbox: vi.fn(),
  retryOutboxOp: vi.fn(),
  discardOutboxOp: vi.fn(),
  showToast: vi.fn(),
  listenEvent: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: listenEvent,
}));

function defaultListen(
  name: string,
  handler: (event: { payload: unknown }) => void,
) {
  handlers.set(name, handler);
  return Promise.resolve(() => {});
}

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

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
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
    listenEvent.mockReset().mockImplementation(defaultListen);
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
    expect(unknown.get(".outbox-status").attributes("role")).toBe("alert");
    expect(
      unknown.get('[data-testid="outbox-retry-1"]').attributes("aria-label"),
    ).toBe("Retry message: Status report");
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
    expect(retryOutboxOp).toHaveBeenCalledWith("account-1", 1);
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

  it("keeps retry disabled for pending rows", async () => {
    const view = await mountList([makeRow({ status: "pending" })]);

    expect(view.get('[data-testid="outbox-retry-1"]').attributes("disabled"))
      .toBeDefined();
    expect(view.get('[data-testid="outbox-discard-1"]').attributes("disabled"))
      .toBeUndefined();
  });

  it("reloads after a successful account-scoped discard", async () => {
    const view = await mountList([makeRow()]);
    listOutbox.mockResolvedValue([]);
    confirm.mockReturnValue(true);

    await view.get('[data-testid="outbox-discard-1"]').trigger("click");
    await flushPromises();

    expect(discardOutboxOp).toHaveBeenCalledWith("account-1", 1);
    expect(view.find('[data-testid="outbox-item-1"]').exists()).toBe(false);
  });

  it("allows only one row action at a time", async () => {
    const view = await mountList([makeRow()]);
    const mutation = deferred<void>();
    retryOutboxOp.mockReturnValueOnce(mutation.promise);
    confirm.mockReturnValue(true);

    await view.get('[data-testid="outbox-retry-1"]').trigger("click");
    expect(view.get('[data-testid="outbox-retry-1"]').attributes("disabled"))
      .toBeDefined();
    await view.get('[data-testid="outbox-retry-1"]').trigger("click");

    expect(retryOutboxOp).toHaveBeenCalledOnce();
    mutation.resolve(undefined);
    await flushPromises();
  });

  it("clears old rows and ignores stale account errors and loading", async () => {
    const view = await mountList([makeRow()]);
    const staleAccount = deferred<OutboxRow[]>();
    const currentAccount = deferred<OutboxRow[]>();
    listOutbox.mockReset();
    listOutbox
      .mockReturnValueOnce(staleAccount.promise)
      .mockReturnValueOnce(currentAccount.promise);

    await view.get('[data-testid="outbox-refresh-btn"]').trigger("click");
    useAccountsStore().activeAccountId = "account-2";
    await nextTick();

    expect(view.find('[data-testid="outbox-item-1"]').exists()).toBe(false);
    expect(view.get('[data-testid="outbox-list"]').attributes("aria-busy")).toBe(
      "true",
    );

    currentAccount.resolve([
      makeRow({ id: 2, account_id: "account-2", subject: "Current" }),
    ]);
    await flushPromises();
    staleAccount.reject(new Error("stale account failed"));
    await flushPromises();

    expect(view.get('[data-testid="outbox-item-2"]').text()).toContain("Current");
    expect(view.find('[data-testid="outbox-error"]').exists()).toBe(false);
    expect(view.get('[data-testid="outbox-list"]').attributes("aria-busy")).toBe(
      "false",
    );
  });

  it("does not act on a row after the active account changes", async () => {
    const view = await mountList([makeRow()]);
    listOutbox.mockResolvedValue([]);
    useAccountsStore().activeAccountId = "account-2";

    await view.get('[data-testid="outbox-retry-1"]').trigger("click");
    await flushPromises();

    expect(confirm).not.toHaveBeenCalled();
    expect(retryOutboxOp).not.toHaveBeenCalled();
    expect(showToast).toHaveBeenCalledWith(
      "The Outbox account changed. Refresh and try again.",
      "error",
      5000,
    );
  });

  it("keeps the newest event reload when responses finish out of order", async () => {
    const view = await mountList([makeRow()]);
    const started = deferred<OutboxRow[]>();
    const unknown = deferred<OutboxRow[]>();
    listOutbox.mockReset();
    listOutbox.mockReturnValueOnce(started.promise).mockReturnValueOnce(unknown.promise);

    handlers.get("send-started")?.({ payload: { account_id: "account-1" } });
    handlers.get("send-unknown")?.({ payload: { account_id: "account-1" } });

    unknown.resolve([
      makeRow({ delivery_outcome_unknown: true, error_message: "unknown" }),
    ]);
    await flushPromises();
    started.resolve([makeRow({ status: "sending" })]);
    await flushPromises();

    expect(view.get('[data-testid="outbox-item-1"]').text()).toContain(
      "Delivery status unknown",
    );
    expect(view.get('[data-testid="outbox-list"]').attributes("aria-busy")).toBe(
      "false",
    );
  });

  it("cleans partial listeners and still loads with manual refresh", async () => {
    const unlisteners: ReturnType<typeof vi.fn>[] = [];
    listenEvent.mockImplementation(
      (name: string, handler: (event: { payload: unknown }) => void) => {
        handlers.set(name, handler);
        if (name === "send-failed") {
          return Promise.reject(new Error("event API unavailable"));
        }
        const unlisten = vi.fn();
        unlisteners.push(unlisten);
        return Promise.resolve(unlisten);
      },
    );
    listOutbox.mockResolvedValue([makeRow()]);

    wrapper = mount(OutboxList);
    handlers.get("send-started")?.({ payload: { account_id: "account-1" } });
    await flushPromises();

    expect(listOutbox).toHaveBeenCalledOnce();
    expect(wrapper.find('[data-testid="outbox-item-1"]').exists()).toBe(true);
    expect(wrapper.get('[data-testid="outbox-listener-error"]').text()).toContain(
      "Automatic Outbox updates are unavailable",
    );
    for (const unlisten of unlisteners) {
      expect(unlisten).toHaveBeenCalledOnce();
    }
    expect(
      wrapper.get('[data-testid="outbox-refresh-btn"]').attributes("disabled"),
    ).toBeUndefined();

    handlers.get("send-started")?.({ payload: { account_id: "account-1" } });
    await flushPromises();
    expect(listOutbox).toHaveBeenCalledOnce();

    await wrapper.get('[data-testid="outbox-refresh-btn"]').trigger("click");
    await flushPromises();
    expect(listOutbox).toHaveBeenCalledTimes(2);
  });

  it("cleans fulfilled and late listeners when unmounted during setup", async () => {
    const lateRegistration = deferred<() => void>();
    const fulfilledUnlisteners: ReturnType<typeof vi.fn>[] = [];
    const lateUnlisten = vi.fn();
    listenEvent.mockImplementation(
      (name: string, handler: (event: { payload: unknown }) => void) => {
        handlers.set(name, handler);
        if (name === "send-unknown") return lateRegistration.promise;
        const unlisten = vi.fn();
        fulfilledUnlisteners.push(unlisten);
        return Promise.resolve(unlisten);
      },
    );
    listOutbox.mockResolvedValue([makeRow()]);

    wrapper = mount(OutboxList);
    await flushPromises();
    expect(wrapper.find('[data-testid="outbox-item-1"]').exists()).toBe(true);

    wrapper.unmount();
    wrapper = undefined;
    for (const unlisten of fulfilledUnlisteners) {
      expect(unlisten).toHaveBeenCalledOnce();
    }

    lateRegistration.resolve(lateUnlisten);
    await flushPromises();
    expect(lateUnlisten).toHaveBeenCalledOnce();
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
