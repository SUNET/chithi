import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { flushPromises } from "@vue/test-utils";

const { handlers, listenEvent } = vi.hoisted(() => ({
  handlers: new Map<string, (event: { payload: unknown }) => void>(),
  listenEvent: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: listenEvent,
}));

vi.mock("@/lib/toast", () => ({
  showToast: vi.fn(),
  dismissToast: vi.fn(),
}));

import { dismissToast, showToast } from "@/lib/toast";
import {
  initializeActivityListenersWithRetry,
  useActivityStore,
} from "@/stores/activity";

function emit(name: string, payload: unknown) {
  const handler = handlers.get(name);
  if (!handler) throw new Error(`no listener registered for "${name}"`);
  handler({ payload });
}

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

describe("activity store send listeners", () => {
  let store: ReturnType<typeof useActivityStore> | undefined;

  beforeEach(() => {
    setActivePinia(createPinia());
    handlers.clear();
    listenEvent.mockReset().mockImplementation(
      (name: string, handler: (event: { payload: unknown }) => void) => {
        handlers.set(name, handler);
        return Promise.resolve(() => {});
      },
    );
    vi.mocked(showToast).mockReset();
    vi.mocked(showToast)
      .mockReturnValueOnce(101)
      .mockReturnValueOnce(102)
      .mockReturnValue(103);
    vi.mocked(dismissToast).mockReset();
    store = undefined;
  });

  afterEach(() => {
    store?.$dispose();
  });

  it("correlates concurrent sends from one account by outbox id", async () => {
    store = useActivityStore();
    await store.initEventListeners();

    emit("send-started", {
      account_id: "account-1",
      subject: "First",
      outbox_id: 41,
    });
    emit("send-started", {
      account_id: "account-1",
      subject: "Second",
      outbox_id: 42,
    });

    emit("send-complete", {
      account_id: "account-1",
      subject: "First",
      outbox_id: 41,
    });

    expect(store.operations.get("send-41")?.status).toBe("done");
    expect(store.operations.get("send-42")?.status).toBe("running");
    expect(dismissToast).toHaveBeenCalledWith(101);
    expect(dismissToast).not.toHaveBeenCalledWith(102);

    emit("send-failed", {
      account_id: "account-1",
      subject: "Second",
      outbox_id: 42,
      error: "server rejected message",
    });

    expect(store.operations.get("send-41")?.status).toBe("done");
    expect(store.operations.get("send-42")?.status).toBe("error");
    expect(store.operations.get("send-42")?.error).toBe(
      "server rejected message",
    );
    expect(dismissToast).toHaveBeenCalledWith(102);
    expect(showToast).toHaveBeenCalledWith(
      "Send failed: server rejected message",
      "error",
      10000,
    );
  });

  it("reports an unknown delivery outcome distinctly", async () => {
    store = useActivityStore();
    await store.initEventListeners();

    emit("send-started", {
      account_id: "account-1",
      subject: "Possibly sent",
      outbox_id: 77,
    });
    emit("send-unknown", {
      account_id: "account-1",
      subject: "Possibly sent",
      outbox_id: 77,
      error: "connection lost after upload",
    });

    const operation = store.operations.get("send-77");
    expect(operation?.status).toBe("error");
    expect(operation?.error).toBe(
      "Delivery status unknown: connection lost after upload",
    );
    expect(dismissToast).toHaveBeenCalledWith(101);
    expect(showToast).toHaveBeenCalledWith(
      'Delivery status unknown for "Possibly sent": connection lost after upload',
      "error",
      10000,
    );
    expect(showToast).not.toHaveBeenCalledWith(
      expect.stringContaining("Send failed"),
      expect.anything(),
      expect.anything(),
    );
  });

  it("shares one initialization while every listener registration is pending", async () => {
    const registration = deferred<() => void>();
    listenEvent.mockImplementation(
      (name: string, handler: (event: { payload: unknown }) => void) => {
        handlers.set(name, handler);
        return registration.promise;
      },
    );
    store = useActivityStore();

    const first = store.initEventListeners();
    const second = store.initEventListeners();

    expect(listenEvent).toHaveBeenCalledTimes(13);

    registration.resolve(() => {});
    await Promise.all([first, second]);
    expect(listenEvent).toHaveBeenCalledTimes(13);
  });

  it("ignores events from listeners until the whole attempt commits", async () => {
    const finalRegistration = deferred<() => void>();
    listenEvent.mockImplementation(
      (name: string, handler: (event: { payload: unknown }) => void) => {
        handlers.set(name, handler);
        return name === "send-unknown"
          ? finalRegistration.promise
          : Promise.resolve(() => {});
      },
    );
    store = useActivityStore();
    const initialization = store.initEventListeners();

    emit("send-started", {
      account_id: "account-1",
      subject: "Too early",
      outbox_id: 90,
    });
    expect(store.operations.size).toBe(0);
    expect(showToast).not.toHaveBeenCalled();

    finalRegistration.resolve(() => {});
    await initialization;
    emit("send-started", {
      account_id: "account-1",
      subject: "Committed",
      outbox_id: 91,
    });
    expect(store.operations.get("send-91")?.status).toBe("running");
    expect(showToast).toHaveBeenCalledWith(
      'Sending "Committed"...',
      "info",
      0,
    );
  });

  it("cleans up a partial failure and permits initialization retry", async () => {
    let failRegistration = true;
    const firstAttemptUnlisteners: ReturnType<typeof vi.fn>[] = [];
    listenEvent.mockImplementation(
      (name: string, handler: (event: { payload: unknown }) => void) => {
        handlers.set(name, handler);
        if (failRegistration && name === "send-failed") {
          return Promise.reject(new Error("listen failed"));
        }
        const unlisten = vi.fn();
        if (failRegistration) firstAttemptUnlisteners.push(unlisten);
        return Promise.resolve(unlisten);
      },
    );
    store = useActivityStore();

    const initialization = store.initEventListeners();
    emit("send-started", {
      account_id: "account-1",
      subject: "Partial",
      outbox_id: 92,
    });
    expect(store.operations.size).toBe(0);
    expect(showToast).not.toHaveBeenCalled();

    await expect(initialization).rejects.toThrow("listen failed");
    expect(firstAttemptUnlisteners).toHaveLength(12);
    for (const unlisten of firstAttemptUnlisteners) {
      expect(unlisten).toHaveBeenCalledOnce();
    }

    failRegistration = false;
    await expect(store.initEventListeners()).resolves.toBeUndefined();
    expect(listenEvent).toHaveBeenCalledTimes(26);
  });

  it("cleans listeners that finish after the store is disposed", async () => {
    const registration = deferred<() => void>();
    const unlisten = vi.fn();
    listenEvent.mockImplementation(
      (name: string, handler: (event: { payload: unknown }) => void) => {
        handlers.set(name, handler);
        return registration.promise;
      },
    );
    store = useActivityStore();
    const activity = store;
    const initialization = activity.initEventListeners();

    emit("send-started", {
      account_id: "account-1",
      subject: "Disposed",
      outbox_id: 93,
    });
    activity.$dispose();
    store = undefined;
    registration.resolve(unlisten);
    await initialization;

    expect(activity.operations.size).toBe(0);
    expect(showToast).not.toHaveBeenCalled();
    expect(unlisten).toHaveBeenCalledTimes(13);
  });

  it("recovers from a transient startup initialization failure", async () => {
    const initialize = vi
      .fn<() => Promise<void>>()
      .mockRejectedValueOnce(new Error("transient"))
      .mockResolvedValueOnce(undefined);

    const error = await initializeActivityListenersWithRetry(
      initialize,
      2,
      0,
      100,
    );

    expect(error).toBeNull();
    expect(initialize).toHaveBeenCalledTimes(2);
  });

  it("returns a persistent startup failure after bounded attempts", async () => {
    const failure = new Error("persistent");
    const initialize = vi
      .fn<() => Promise<void>>()
      .mockRejectedValue(failure);

    const error = await initializeActivityListenersWithRetry(
      initialize,
      2,
      0,
      100,
    );

    expect(error).toBe(failure);
    expect(initialize).toHaveBeenCalledTimes(2);
  });

  it("bounds a startup attempt whose listener registration never settles", async () => {
    const initialize = vi.fn<() => Promise<void>>(
      () => new Promise<void>(() => {}),
    );

    const error = await initializeActivityListenersWithRetry(
      initialize,
      1,
      0,
      5,
    );

    expect(error).toBeInstanceOf(Error);
    expect((error as Error).message).toContain("timed out");
    expect(initialize).toHaveBeenCalledOnce();
  });

  it("cancels a timed-out store attempt before a fresh retry commits", async () => {
    const lateRegistration = deferred<() => void>();
    const lateUnlisten = vi.fn();
    const firstAttemptUnlisteners: ReturnType<typeof vi.fn>[] = [];
    const secondAttemptUnlisteners: ReturnType<typeof vi.fn>[] = [];
    const staleHandlers = new Map<
      string,
      (event: { payload: unknown }) => void
    >();
    let registrationCount = 0;
    listenEvent.mockImplementation(
      (name: string, handler: (event: { payload: unknown }) => void) => {
        registrationCount += 1;
        const attempt = Math.ceil(registrationCount / 13);
        handlers.set(name, handler);
        if (attempt === 1) staleHandlers.set(name, handler);
        if (attempt === 1 && name === "send-unknown") {
          return lateRegistration.promise;
        }
        const unlisten = vi.fn();
        if (attempt === 1) {
          firstAttemptUnlisteners.push(unlisten);
        } else {
          secondAttemptUnlisteners.push(unlisten);
        }
        return Promise.resolve(unlisten);
      },
    );
    const activity = useActivityStore();
    store = activity;

    const error = await initializeActivityListenersWithRetry(
      (signal) => activity.initEventListeners(signal),
      2,
      0,
      10,
    );

    expect(error).toBeNull();
    expect(listenEvent).toHaveBeenCalledTimes(26);
    expect(firstAttemptUnlisteners).toHaveLength(12);
    for (const unlisten of firstAttemptUnlisteners) {
      expect(unlisten).toHaveBeenCalledOnce();
    }
    expect(secondAttemptUnlisteners).toHaveLength(13);
    for (const unlisten of secondAttemptUnlisteners) {
      expect(unlisten).not.toHaveBeenCalled();
    }

    emit("send-started", {
      account_id: "account-1",
      subject: "Fresh attempt",
      outbox_id: 94,
    });
    expect(activity.operations.get("send-94")?.status).toBe("running");

    lateRegistration.resolve(lateUnlisten);
    await flushPromises();
    expect(lateUnlisten).toHaveBeenCalledOnce();
    for (const unlisten of secondAttemptUnlisteners) {
      expect(unlisten).not.toHaveBeenCalled();
    }

    staleHandlers.get("send-started")?.({
      payload: {
        account_id: "account-1",
        subject: "Stale attempt",
        outbox_id: 95,
      },
    });
    expect(activity.operations.has("send-95")).toBe(false);
    expect(showToast).not.toHaveBeenCalledWith(
      'Sending "Stale attempt"...',
      "info",
      0,
    );
  });
});
