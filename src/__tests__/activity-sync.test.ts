import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";

// Capture every handler registered through `listen()` so tests can drive
// the activity store the same way the Rust backend does — by emitting
// events — and assert on the resulting sync indicator state.
const handlers = new Map<string, (e: { payload: unknown }) => void>();

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((name: string, cb: (e: { payload: unknown }) => void) => {
    handlers.set(name, cb);
    return Promise.resolve(() => {});
  }),
}));

vi.mock("@/lib/toast", () => ({
  showToast: vi.fn(() => 1),
  dismissToast: vi.fn(),
}));

import { useActivityStore } from "@/stores/activity";

function emit(name: string, payload: unknown) {
  const handler = handlers.get(name);
  if (!handler) throw new Error(`no listener registered for "${name}"`);
  handler({ payload });
}

describe("activity store — sync indicator data flow", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    handlers.clear();
  });

  it("mail sync drives hasActiveOperations true while running", async () => {
    const store = useActivityStore();
    await store.initEventListeners();

    emit("sync-started", { account_id: "a1", account_name: "Acc" });
    expect(store.hasActiveOperations).toBe(true);

    emit("sync-complete", { account_id: "a1", total_synced: 0 });
    expect(store.hasActiveOperations).toBe(false);
  });

  it("calendar sync drives hasActiveOperations the same way (regression)", async () => {
    // Regression: calendar sync previously did not register a running
    // operation, so the StatusBar Sync button never spun during calendar
    // sync even though the sync was happening.
    const store = useActivityStore();
    await store.initEventListeners();

    emit("calendar-sync-started", "a1");
    expect(store.hasActiveOperations).toBe(true);

    emit("calendar-sync-complete", "a1");
    expect(store.hasActiveOperations).toBe(false);
  });

  it("calendar sync error fails the operation", async () => {
    const store = useActivityStore();
    await store.initEventListeners();

    emit("calendar-sync-started", "a1");
    emit("calendar-sync-error", {
      account_id: "a1",
      error: "network unreachable",
    });

    expect(store.hasActiveOperations).toBe(false);
    const op = store.recentOperations.find((o) => o.id === "cal-sync-a1");
    expect(op?.status).toBe("error");
    expect(op?.error).toBe("network unreachable");
  });

  it("calendar sync op id is scoped per account", async () => {
    const store = useActivityStore();
    await store.initEventListeners();

    emit("calendar-sync-started", "acc-42");
    expect(store.activeOperations.map((op) => op.id)).toEqual([
      "cal-sync-acc-42",
    ]);

    // A completion for a different account must not finish this one.
    emit("calendar-sync-complete", "other-account");
    expect(store.hasActiveOperations).toBe(true);

    emit("calendar-sync-complete", "acc-42");
    expect(store.hasActiveOperations).toBe(false);
  });

  it("'calendar-changed' (data-mutation event) does NOT complete a running sync op", async () => {
    // Regression: invite responses and push processing also emit
    // 'calendar-changed'. Coupling that event to the sync spinner would
    // stop the indicator while the backend sync was still running.
    const store = useActivityStore();
    await store.initEventListeners();

    emit("calendar-sync-started", "a1");
    expect(store.hasActiveOperations).toBe(true);

    // A stray 'calendar-changed' from any source must not affect the spinner.
    expect(handlers.has("calendar-changed")).toBe(false);
    expect(store.hasActiveOperations).toBe(true);

    emit("calendar-sync-complete", "a1");
    expect(store.hasActiveOperations).toBe(false);
  });
});

describe("activity store — pending-removal timer race", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    handlers.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("a fresh op with the same id cancels the prior removal timer", async () => {
    // Regression: completeOperation/failOperation auto-remove the entry by
    // id 60s/5min later. Before the fix, a second sync that reused the same
    // id within that window would be wiped out by the stale timer.
    const store = useActivityStore();
    await store.initEventListeners();

    emit("calendar-sync-started", "a1");
    emit("calendar-sync-complete", "a1");

    // Within the 60s removal window, start a new sync with the same id.
    vi.advanceTimersByTime(10_000);
    emit("calendar-sync-started", "a1");
    expect(store.hasActiveOperations).toBe(true);

    // The original removal timer would have fired by now; the new op must
    // still be running.
    vi.advanceTimersByTime(60_000);
    expect(store.hasActiveOperations).toBe(true);
    const op = store.activeOperations.find((o) => o.id === "cal-sync-a1");
    expect(op?.status).toBe("running");
  });

  it("terminal entry is removed after the timer fires when no restart happens", async () => {
    const store = useActivityStore();
    await store.initEventListeners();

    emit("calendar-sync-started", "a1");
    emit("calendar-sync-complete", "a1");
    expect(store.recentOperations.some((o) => o.id === "cal-sync-a1")).toBe(
      true,
    );

    vi.advanceTimersByTime(60_001);
    expect(store.recentOperations.some((o) => o.id === "cal-sync-a1")).toBe(
      false,
    );
  });

  it("pending removal timers are cleared on store dispose", async () => {
    // Regression: pendingRemovals holds live timeout handles that would
    // otherwise fire on a disposed store, mutating its state and leaking
    // the handle. onScopeDispose must clearTimeout each pending entry.
    const store = useActivityStore();
    await store.initEventListeners();

    emit("calendar-sync-started", "a1");
    emit("calendar-sync-complete", "a1");
    // A 60s removal timer for "cal-sync-a1" is now pending.
    expect(vi.getTimerCount()).toBeGreaterThan(0);

    store.$dispose();

    // All timers scheduled by the activity store must be cancelled. Any
    // remaining pending timers would be ones owned by other test machinery,
    // but in this isolated test there are none.
    expect(vi.getTimerCount()).toBe(0);
  });
});
