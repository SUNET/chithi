import { describe, it, expect, vi, beforeEach } from "vitest";

// Mock tauri invoke
const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

import { moveMessagesCrossAccount } from "@/lib/tauri";

describe("Cross-account move", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    invokeMock.mockResolvedValue(undefined);
  });

  it("invokes move_messages_cross_account with correct payload", async () => {
    await moveMessagesCrossAccount("acc1", ["msg1", "msg2"], "acc2", "INBOX");
    expect(invokeMock).toHaveBeenCalledWith("move_messages_cross_account", {
      sourceAccountId: "acc1",
      messageIds: ["msg1", "msg2"],
      targetAccountId: "acc2",
      targetFolder: "INBOX",
    });
  });

  it("propagates errors from the backend", async () => {
    invokeMock.mockRejectedValue(new Error("Cross-account move failed"));
    await expect(
      moveMessagesCrossAccount("acc1", ["msg1"], "acc2", "INBOX"),
    ).rejects.toThrow("Cross-account move failed");
  });
});
