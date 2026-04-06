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

describe("Attachment mention detection", () => {
  function mentionsAttachment(body: string, subject = ""): boolean {
    const text = (body + "\n" + subject).toLowerCase();
    return /\battach(ed|ment|ments|ing)?\b/.test(text);
  }

  it("detects 'attached' in body", () => {
    expect(mentionsAttachment("Please see the attached file.")).toBe(true);
  });

  it("detects 'attachment' in body", () => {
    expect(mentionsAttachment("I've included an attachment.")).toBe(true);
  });

  it("detects 'attachments' in body", () => {
    expect(mentionsAttachment("See the attachments below.")).toBe(true);
  });

  it("detects 'attaching' in body", () => {
    expect(mentionsAttachment("I'm attaching the report.")).toBe(true);
  });

  it("detects 'attach' in body", () => {
    expect(mentionsAttachment("Let me attach the file.")).toBe(true);
  });

  it("detects mention in subject", () => {
    expect(mentionsAttachment("Hello", "Report attached")).toBe(true);
  });

  it("does not false-positive on unrelated words", () => {
    expect(mentionsAttachment("Hello, how are you?")).toBe(false);
  });

  it("does not match partial words", () => {
    expect(mentionsAttachment("The detachment was noted.")).toBe(false);
  });

  it("handles empty body", () => {
    expect(mentionsAttachment("")).toBe(false);
  });

  it("case insensitive", () => {
    expect(mentionsAttachment("PLEASE SEE ATTACHED")).toBe(true);
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

describe("Signature management", () => {
  // Mirror the compose view's buildSignatureBlock logic
  function buildSignatureBlock(sig: string, hasBody: boolean): string {
    if (!sig) return "";
    const gap = hasBody ? "\n\n" : "\n\n\n\n\n";
    return gap + sig;
  }

  it("appends signature to empty body with 5-line gap", () => {
    let body = "";
    const block = buildSignatureBlock("-- \nAlice Smith", false);
    body += block;
    expect(body).toBe("\n\n\n\n\n-- \nAlice Smith");
  });

  it("appends signature to reply body with 2-line gap", () => {
    let body = "> Original message";
    const block = buildSignatureBlock("-- \nAlice Smith", true);
    body += block;
    expect(body).toBe("> Original message\n\n-- \nAlice Smith");
  });

  it("replaces old signature block when switching accounts", () => {
    const oldBlock = buildSignatureBlock("-- \nAlice Smith", false);
    const newBlock = buildSignatureBlock("-- \nBob Jones", false);
    let body = "" + oldBlock; // empty compose + sig

    if (oldBlock && body.endsWith(oldBlock)) {
      body = body.slice(0, -oldBlock.length) + newBlock;
    }
    expect(body).toBe("\n\n\n\n\n-- \nBob Jones");
  });

  it("removes old signature when new account has none", () => {
    const oldBlock = buildSignatureBlock("-- \nAlice Smith", true);
    const newBlock = buildSignatureBlock("", true);
    let body = "> Original" + oldBlock;

    if (oldBlock && body.endsWith(oldBlock)) {
      body = body.slice(0, -oldBlock.length) + newBlock;
    }
    expect(body).toBe("> Original");
  });

  it("appends new signature when old account had none", () => {
    let body = "Hello world";
    const oldBlock: string = "";
    const newBlock = buildSignatureBlock("-- \nBob Jones", true);

    if (oldBlock.length > 0 && body.endsWith(oldBlock)) {
      body = body.slice(0, -oldBlock.length) + newBlock;
    } else if (newBlock) {
      body += newBlock;
    }
    expect(body).toBe("Hello world\n\n-- \nBob Jones");
  });

  it("signature-only body is not dirty", () => {
    // After applySignature, baselineBody is updated to match bodyText
    const bodyText: string = "\n\n\n\n\n-- \nAlice Smith";
    const baselineBody: string = "\n\n\n\n\n-- \nAlice Smith";
    expect(bodyText !== baselineBody).toBe(false);
  });

  it("typing above signature makes it dirty", () => {
    const bodyText: string = "Hello\n\n\n\n\n-- \nAlice Smith";
    const baselineBody: string = "\n\n\n\n\n-- \nAlice Smith";
    expect(bodyText !== baselineBody).toBe(true);
  });
});
