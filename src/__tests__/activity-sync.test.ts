import { describe, it, expect, vi, beforeEach } from "vitest";
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
    // Regression: calendar sync only emitted "calendar-changed" (a no-op
    // completeOperation for an id that was never started), so the StatusBar
    // Sync button never spun during calendar sync.
    const store = useActivityStore();
    await store.initEventListeners();

    emit("calendar-sync-started", "a1");
    expect(store.hasActiveOperations).toBe(true);

    emit("calendar-changed", "a1");
    expect(store.hasActiveOperations).toBe(false);
  });

  it("calendar op id matches between start and completion events", async () => {
    const store = useActivityStore();
    await store.initEventListeners();

    emit("calendar-sync-started", "acc-42");
    expect(store.activeOperations.map((op) => op.id)).toEqual([
      "cal-sync-acc-42",
    ]);

    // A "calendar-changed" for a different account must not complete it.
    emit("calendar-changed", "other-account");
    expect(store.hasActiveOperations).toBe(true);

    emit("calendar-changed", "acc-42");
    expect(store.hasActiveOperations).toBe(false);
  });
});
