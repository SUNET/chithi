import { describe, it, expect, vi, beforeEach } from "vitest";

// Mock @tauri-apps/api/webviewWindow
const mockWebviewWindow = vi.fn();
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  WebviewWindow: class {
    label: string;
    options: Record<string, unknown>;
    constructor(label: string, options: Record<string, unknown>) {
      this.label = label;
      this.options = options;
      mockWebviewWindow(label, options);
    }
    once() {}
  },
}));

// Mock @tauri-apps/api/window
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    close: vi.fn(),
    label: "compose-1",
  }),
}));

import { openComposeWindow } from "@/lib/compose-window";

describe("openComposeWindow", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("should create a WebviewWindow with default options", () => {
    openComposeWindow();

    expect(mockWebviewWindow).toHaveBeenCalledTimes(1);
    const [label, options] = mockWebviewWindow.mock.calls[0];
    expect(label).toMatch(/^compose-\d+$/);
    expect(options.url).toBe("/compose");
    expect(options.title).toBe("Compose");
    expect(options.width).toBe(720);
    expect(options.height).toBe(640);
    expect(options.center).toBe(true);
    expect(options.focus).toBe(true);
    expect(options.resizable).toBe(true);
  });

  it("should pass query params for reply", () => {
    openComposeWindow({
      replyTo: "msg-123",
      to: "alice@example.com",
      subject: "Re: Hello",
      body: "> Original message",
    });

    const [, options] = mockWebviewWindow.mock.calls[0];
    const url = options.url as string;
    expect(url).toContain("/compose?");
    expect(url).toContain("replyTo=msg-123");
    expect(url).toContain("to=alice%40example.com");
    expect(url).toContain("subject=Re");
    expect(url).toContain("body=");
  });

  it("should pass cc param for reply-all", () => {
    openComposeWindow({
      to: "alice@example.com",
      cc: "bob@example.com, carol@example.com",
      subject: "Re: Team meeting",
    });

    const [, options] = mockWebviewWindow.mock.calls[0];
    const url = options.url as string;
    expect(url).toContain("cc=bob");
    expect(url).toContain("carol");
  });

  it("should not include empty params in URL", () => {
    openComposeWindow({ to: "alice@example.com" });

    const [, options] = mockWebviewWindow.mock.calls[0];
    const url = options.url as string;
    expect(url).toContain("to=");
    expect(url).not.toContain("replyTo=");
    expect(url).not.toContain("cc=");
    expect(url).not.toContain("subject=");
    expect(url).not.toContain("body=");
  });

  it("should generate unique labels for each window", () => {
    openComposeWindow();
    openComposeWindow();
    openComposeWindow();

    expect(mockWebviewWindow).toHaveBeenCalledTimes(3);
    const labels = mockWebviewWindow.mock.calls.map(
      (c: [string, unknown]) => c[0],
    );
    const uniqueLabels = new Set(labels);
    expect(uniqueLabels.size).toBe(3);
  });

  it("should handle forward with no to/cc", () => {
    openComposeWindow({
      subject: "Fwd: Important doc",
      body: "---------- Forwarded message ----------\nContent here",
    });

    const [, options] = mockWebviewWindow.mock.calls[0];
    const url = options.url as string;
    expect(url).toContain("subject=Fwd");
    expect(url).toContain("body=");
    expect(url).not.toContain("to=");
  });
});
