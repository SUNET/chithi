/**
 * Tests for the `lib/tauri.ts` wrappers backing #191 ("Open on Inbox of
 * the first account (or last-viewed folder) on startup").
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

// Use vi.hoisted so the mock is available before vi.mock's hoisted factory runs
const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

import { getLastView, saveLastView } from "@/lib/tauri";

describe("lib/tauri: last-view wrappers (#191)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("getLastView invokes get_last_view", async () => {
    invokeMock.mockResolvedValue({ account_id: "acc1", folder_path: "INBOX" });
    const result = await getLastView();
    expect(invokeMock).toHaveBeenCalledWith("get_last_view");
    expect(result).toEqual({ account_id: "acc1", folder_path: "INBOX" });
  });

  it("saveLastView invokes save_last_view with camelCase args", async () => {
    invokeMock.mockResolvedValue(undefined);
    await saveLastView("acc1", "Archive");
    expect(invokeMock).toHaveBeenCalledWith("save_last_view", {
      accountId: "acc1",
      folderPath: "Archive",
    });
  });

  it("propagates errors from the backend", async () => {
    invokeMock.mockRejectedValue(new Error("db error"));
    await expect(getLastView()).rejects.toThrow("db error");
  });
});
