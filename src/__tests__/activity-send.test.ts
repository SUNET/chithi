import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";

const handlers = new Map<string, (event: { payload: unknown }) => void>();

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(
    (name: string, handler: (event: { payload: unknown }) => void) => {
      handlers.set(name, handler);
      return Promise.resolve(() => {});
    },
  ),
}));

vi.mock("@/lib/toast", () => ({
  showToast: vi.fn(),
  dismissToast: vi.fn(),
}));

import { dismissToast, showToast } from "@/lib/toast";
import { useActivityStore } from "@/stores/activity";

function emit(name: string, payload: unknown) {
  const handler = handlers.get(name);
  if (!handler) throw new Error(`no listener registered for "${name}"`);
  handler({ payload });
}

describe("activity store send listeners", () => {
  let store: ReturnType<typeof useActivityStore> | undefined;

  beforeEach(() => {
    setActivePinia(createPinia());
    handlers.clear();
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
});
