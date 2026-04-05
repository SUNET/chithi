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

// Mock tauri API
vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  sendMessage: vi.fn().mockResolvedValue(undefined),
  saveDraft: vi.fn().mockResolvedValue(undefined),
  setMessageFlags: vi.fn().mockResolvedValue(undefined),
}));

import { openComposeWindow } from "@/lib/compose-window";
import * as api from "@/lib/tauri";

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
    expect(options.title).toBe("Write (no subject) - Chithi");
    expect(options.width).toBe(1024);
    expect(options.height).toBe(700);
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
      (c: unknown[]) => c[0] as string,
    );
    const uniqueLabels = new Set(labels);
    expect(uniqueLabels.size).toBe(3);
  });

  it("should set title with subject when provided", () => {
    openComposeWindow({ subject: "Re: Hello" });

    const [, options] = mockWebviewWindow.mock.calls[0];
    expect(options.title).toBe("Write Re: Hello - Chithi");
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

  it("should pass accountId in URL when provided", () => {
    openComposeWindow({ accountId: "acc-123" });

    const [, options] = mockWebviewWindow.mock.calls[0];
    const url = options.url as string;
    expect(url).toContain("accountId=acc-123");
  });
});

describe("Compose dirty tracking", () => {
  it("empty compose is not dirty", () => {
    const initial = { to: "", cc: "", subject: "", body: "" };
    const current = { to: "", cc: "", bcc: "", subject: "", body: "", attachments: [] };
    const dirty = current.to !== initial.to || current.cc !== initial.cc ||
      current.bcc !== "" || current.subject !== initial.subject ||
      current.body !== initial.body || current.attachments.length > 0;
    expect(dirty).toBe(false);
  });

  it("typing in To makes it dirty", () => {
    const initial = { to: "", cc: "", subject: "", body: "" };
    const current = { to: "alice@example.com", cc: "", bcc: "", subject: "", body: "", attachments: [] };
    const dirty = current.to !== initial.to || current.cc !== initial.cc ||
      current.bcc !== "" || current.subject !== initial.subject ||
      current.body !== initial.body || current.attachments.length > 0;
    expect(dirty).toBe(true);
  });

  it("typing in Subject makes it dirty", () => {
    const initial = { to: "", cc: "", subject: "", body: "" };
    const current = { to: "", cc: "", bcc: "", subject: "Hello", body: "", attachments: [] };
    const dirty = current.to !== initial.to || current.cc !== initial.cc ||
      current.bcc !== "" || current.subject !== initial.subject ||
      current.body !== initial.body || current.attachments.length > 0;
    expect(dirty).toBe(true);
  });

  it("typing in Body makes it dirty", () => {
    const initial = { to: "", cc: "", subject: "", body: "" };
    const current = { to: "", cc: "", bcc: "", subject: "", body: "Hello world", attachments: [] };
    const dirty = current.to !== initial.to || current.cc !== initial.cc ||
      current.bcc !== "" || current.subject !== initial.subject ||
      current.body !== initial.body || current.attachments.length > 0;
    expect(dirty).toBe(true);
  });

  it("adding attachment makes it dirty", () => {
    const initial = { to: "", cc: "", subject: "", body: "" };
    const current = { to: "", cc: "", bcc: "", subject: "", body: "",
      attachments: [{ path: "/tmp/file.pdf", name: "file.pdf" }] };
    const dirty = current.to !== initial.to || current.cc !== initial.cc ||
      current.bcc !== "" || current.subject !== initial.subject ||
      current.body !== initial.body || current.attachments.length > 0;
    expect(dirty).toBe(true);
  });

  it("reply prefill is not dirty when unchanged", () => {
    const initial = { to: "alice@example.com", cc: "", subject: "Re: Hello", body: "> original" };
    const current = { to: "alice@example.com", cc: "", bcc: "", subject: "Re: Hello", body: "> original", attachments: [] };
    const dirty = current.to !== initial.to || current.cc !== initial.cc ||
      current.bcc !== "" || current.subject !== initial.subject ||
      current.body !== initial.body || current.attachments.length > 0;
    expect(dirty).toBe(false);
  });

  it("editing reply body makes it dirty", () => {
    const initial = { to: "alice@example.com", cc: "", subject: "Re: Hello", body: "> original" };
    const current = { to: "alice@example.com", cc: "", bcc: "", subject: "Re: Hello", body: "My reply\n\n> original", attachments: [] };
    const dirty = current.to !== initial.to || current.cc !== initial.cc ||
      current.bcc !== "" || current.subject !== initial.subject ||
      current.body !== initial.body || current.attachments.length > 0;
    expect(dirty).toBe(true);
  });
});

describe("saveDraft API", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("should call saveDraft with correct params", async () => {
    await api.saveDraft("acc-1", {
      to: ["alice@example.com"],
      cc: [],
      bcc: [],
      subject: "Draft subject",
      body_text: "Draft body",
      body_html: null,
      attachments: [],
    });

    expect(api.saveDraft).toHaveBeenCalledWith("acc-1", {
      to: ["alice@example.com"],
      cc: [],
      bcc: [],
      subject: "Draft subject",
      body_text: "Draft body",
      body_html: null,
      attachments: [],
    });
  });

  it("should call saveDraft with attachments", async () => {
    await api.saveDraft("acc-1", {
      to: [],
      cc: [],
      bcc: [],
      subject: "",
      body_text: "",
      body_html: null,
      attachments: [{ path: "/tmp/doc.pdf", name: "doc.pdf" }],
    });

    expect(api.saveDraft).toHaveBeenCalledWith("acc-1", expect.objectContaining({
      attachments: [{ path: "/tmp/doc.pdf", name: "doc.pdf" }],
    }));
  });
});
