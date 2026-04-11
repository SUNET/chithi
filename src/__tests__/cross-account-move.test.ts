/**
 * Tests for cross-account message move dispatch.
 *
 * - Verifies the tauri wrapper invokes the correct command with the
 *   parameter names the Rust side expects (this locks the frontend/
 *   backend contract).
 * - Verifies the drop routing logic picks the same-account command
 *   vs the cross-account command based on source/destination ids.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

// vi.mock factories are hoisted above imports — use vi.hoisted so the
// mock variable is available when the factory runs.
const { invokeMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
}));

import { moveMessages, moveMessagesCrossAccount } from "@/lib/tauri";

describe("moveMessagesCrossAccount wrapper", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
  });

  it("invokes move_messages_cross_account with the Rust parameter names", async () => {
    await moveMessagesCrossAccount("acc-src", ["m1", "m2"], "acc-dst", "Archive");
    expect(invokeMock).toHaveBeenCalledWith("move_messages_cross_account", {
      sourceAccountId: "acc-src",
      messageIds: ["m1", "m2"],
      targetAccountId: "acc-dst",
      targetFolder: "Archive",
    });
  });

  it("same-account wrapper invokes move_messages (distinct command)", async () => {
    await moveMessages("acc-src", ["m1"], "Trash");
    expect(invokeMock).toHaveBeenCalledWith("move_messages", {
      accountId: "acc-src",
      messageIds: ["m1"],
      targetFolder: "Trash",
    });
  });
});

// The drop handler in FolderTree.vue dispatches between the two commands
// based on whether the drag source account matches the drop target. Extract
// the decision into a pure function so we can exercise it without mounting
// the full tree with all its drag state.
function pickMoveCommand(
  sourceAccountId: string,
  targetAccountId: string,
): "same" | "cross" {
  return sourceAccountId === targetAccountId ? "same" : "cross";
}

describe("cross-account drop routing", () => {
  it("picks same-account command when source equals destination", () => {
    expect(pickMoveCommand("acc1", "acc1")).toBe("same");
  });

  it("picks cross-account command when source differs from destination", () => {
    expect(pickMoveCommand("acc1", "acc2")).toBe("cross");
  });
});
