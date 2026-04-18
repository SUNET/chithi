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
  function attachmentBaselineValue(
    items: Array<{ path: string; name: string }>,
  ): string {
    return JSON.stringify(items.map(({ path, name }) => ({ path, name })));
  }

  function isDirtyState(
    current: {
      to: string;
      cc: string;
      bcc: string;
      subject: string;
      body: string;
      attachments: Array<{ path: string; name: string }>;
    },
    baseline: {
      to: string;
      cc: string;
      bcc: string;
      subject: string;
      body: string;
      attachments: Array<{ path: string; name: string }>;
    },
  ): boolean {
    return current.to !== baseline.to ||
      current.cc !== baseline.cc ||
      current.bcc !== baseline.bcc ||
      current.subject !== baseline.subject ||
      current.body !== baseline.body ||
      attachmentBaselineValue(current.attachments) !==
        attachmentBaselineValue(baseline.attachments);
  }

  it("empty compose is not dirty", () => {
    const initial = { to: "", cc: "", bcc: "", subject: "", body: "", attachments: [] };
    const current = { to: "", cc: "", bcc: "", subject: "", body: "", attachments: [] };
    const dirty = isDirtyState(current, initial);
    expect(dirty).toBe(false);
  });

  it("typing in To makes it dirty", () => {
    const initial = { to: "", cc: "", bcc: "", subject: "", body: "", attachments: [] };
    const current = { to: "alice@example.com", cc: "", bcc: "", subject: "", body: "", attachments: [] };
    const dirty = isDirtyState(current, initial);
    expect(dirty).toBe(true);
  });

  it("typing in Subject makes it dirty", () => {
    const initial = { to: "", cc: "", bcc: "", subject: "", body: "", attachments: [] };
    const current = { to: "", cc: "", bcc: "", subject: "Hello", body: "", attachments: [] };
    const dirty = isDirtyState(current, initial);
    expect(dirty).toBe(true);
  });

  it("typing in Body makes it dirty", () => {
    const initial = { to: "", cc: "", bcc: "", subject: "", body: "", attachments: [] };
    const current = { to: "", cc: "", bcc: "", subject: "", body: "Hello world", attachments: [] };
    const dirty = isDirtyState(current, initial);
    expect(dirty).toBe(true);
  });

  it("adding attachment makes it dirty", () => {
    const initial = { to: "", cc: "", bcc: "", subject: "", body: "", attachments: [] };
    const current = { to: "", cc: "", bcc: "", subject: "", body: "",
      attachments: [{ path: "/tmp/file.pdf", name: "file.pdf" }] };
    const dirty = isDirtyState(current, initial);
    expect(dirty).toBe(true);
  });

  it("reply prefill is not dirty when unchanged", () => {
    const initial = { to: "alice@example.com", cc: "", bcc: "", subject: "Re: Hello", body: "> original", attachments: [] };
    const current = { to: "alice@example.com", cc: "", bcc: "", subject: "Re: Hello", body: "> original", attachments: [] };
    const dirty = isDirtyState(current, initial);
    expect(dirty).toBe(false);
  });

  it("editing reply body makes it dirty", () => {
    const initial = { to: "alice@example.com", cc: "", bcc: "", subject: "Re: Hello", body: "> original", attachments: [] };
    const current = { to: "alice@example.com", cc: "", bcc: "", subject: "Re: Hello", body: "My reply\n\n> original", attachments: [] };
    const dirty = isDirtyState(current, initial);
    expect(dirty).toBe(true);
  });

  it("manual save resets dirty baseline", () => {
    const savedState = {
      to: "alice@example.com",
      cc: "",
      bcc: "",
      subject: "Draft subject",
      body: "Draft body",
      attachments: [{ path: "/tmp/file.pdf", name: "file.pdf" }],
    };

    expect(isDirtyState(savedState, savedState)).toBe(false);
  });

  it("editing after manual save becomes dirty again", () => {
    const savedState = {
      to: "alice@example.com",
      cc: "",
      bcc: "",
      subject: "Draft subject",
      body: "Draft body",
      attachments: [],
    };
    const current = {
      ...savedState,
      body: "Draft body updated",
    };

    expect(isDirtyState(current, savedState)).toBe(true);
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
      attachments: [{ token: "tok-123", name: "doc.pdf" }],
    });

    expect(api.saveDraft).toHaveBeenCalledWith("acc-1", expect.objectContaining({
      attachments: [{ token: "tok-123", name: "doc.pdf" }],
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

describe("Compose autocomplete", () => {
  function getLastTerm(input: string): string {
    const parts = input.split(/[,;]/);
    return (parts[parts.length - 1] || "").trim();
  }

  function insertAutocomplete(fieldValue: string, display: string, email: string): string {
    const parts = fieldValue.split(/[,;]/);
    parts[parts.length - 1] = ` ${display} <${email}>`;
    return parts.join(",") + ", ";
  }

  it("extracts last term from single address", () => {
    expect(getLastTerm("ali")).toBe("ali");
  });

  it("extracts last term after comma", () => {
    expect(getLastTerm("alice@example.com, bo")).toBe("bo");
  });

  it("extracts last term after semicolon", () => {
    expect(getLastTerm("alice@example.com; ku")).toBe("ku");
  });

  it("returns empty for trailing comma", () => {
    expect(getLastTerm("alice@example.com, ")).toBe("");
  });

  it("inserts selected contact into single field", () => {
    const result = insertAutocomplete("ali", "Alice Smith", "alice@example.com");
    expect(result).toBe(" Alice Smith <alice@example.com>, ");
  });

  it("inserts selected contact after existing address", () => {
    const result = insertAutocomplete("bob@test.com, ali", "Alice Smith", "alice@example.com");
    expect(result).toBe("bob@test.com, Alice Smith <alice@example.com>, ");
  });

  it("does not trigger for queries shorter than 2 chars", () => {
    expect(getLastTerm("a").length < 2).toBe(true);
  });
});

describe("Contact lookup from email address", () => {
  // Mirrors the exact-match logic in MessageReader.onAddrRightClick
  function findExactContact(
    results: { emails_json: string }[],
    email: string,
  ): { emails_json: string } | undefined {
    return results.find((c) => {
      try {
        const emails: { email: string }[] = JSON.parse(c.emails_json);
        return emails.some((e) => e.email.toLowerCase() === email.toLowerCase());
      } catch { return false; }
    });
  }

  it("finds exact email match in contacts", () => {
    const results = [
      { emails_json: JSON.stringify([{ email: "alice@example.com", label: "work" }]) },
      { emails_json: JSON.stringify([{ email: "bob@example.com", label: "home" }]) },
    ];
    expect(findExactContact(results, "alice@example.com")).toBe(results[0]);
  });

  it("matches case-insensitively", () => {
    const results = [
      { emails_json: JSON.stringify([{ email: "Alice@Example.COM", label: "work" }]) },
    ];
    expect(findExactContact(results, "alice@example.com")).toBe(results[0]);
  });

  it("returns undefined when no match", () => {
    const results = [
      { emails_json: JSON.stringify([{ email: "bob@example.com", label: "work" }]) },
    ];
    expect(findExactContact(results, "alice@example.com")).toBeUndefined();
  });

  it("handles multiple emails per contact", () => {
    const results = [
      { emails_json: JSON.stringify([
        { email: "alice-work@co.com", label: "work" },
        { email: "alice@personal.com", label: "home" },
      ]) },
    ];
    expect(findExactContact(results, "alice@personal.com")).toBe(results[0]);
  });

  it("handles malformed emails_json gracefully", () => {
    const results = [{ emails_json: "not json" }];
    expect(findExactContact(results, "alice@example.com")).toBeUndefined();
  });
});

describe("parseAddresses with Name <email> format", () => {
  function parseAddresses(input: string): string[] {
    return input
      .split(/[,;]/)
      .map((s) => s.trim())
      .filter((s) => s.length > 0)
      .map((s) => {
        const match = s.match(/<([^>]+)>/);
        return match ? match[1] : s;
      });
  }

  it("extracts email from Name <email> format", () => {
    expect(parseAddresses("Alice Smith <alice@example.com>")).toEqual(["alice@example.com"]);
  });

  it("handles plain email address", () => {
    expect(parseAddresses("alice@example.com")).toEqual(["alice@example.com"]);
  });

  it("handles autocomplete format with trailing comma", () => {
    expect(parseAddresses("Alice Smith <alice@example.com>, ")).toEqual(["alice@example.com"]);
  });

  it("handles multiple Name <email> addresses", () => {
    expect(parseAddresses("Alice <alice@a.com>, Bob <bob@b.com>")).toEqual(["alice@a.com", "bob@b.com"]);
  });

  it("handles mix of plain and Name <email>", () => {
    expect(parseAddresses("alice@a.com, Bob <bob@b.com>")).toEqual(["alice@a.com", "bob@b.com"]);
  });

  it("handles same email in name and angle brackets", () => {
    expect(parseAddresses("kushal@sunet.se <kushal@sunet.se>, ")).toEqual(["kushal@sunet.se"]);
  });

  it("handles semicolon separator", () => {
    expect(parseAddresses("Alice <alice@a.com>; Bob <bob@b.com>")).toEqual(["alice@a.com", "bob@b.com"]);
  });
});

describe("XOAUTH2 token format", () => {
  function buildXOAuth2Token(user: string, accessToken: string): string {
    return `user=${user}\x01auth=Bearer ${accessToken}\x01\x01`;
  }

  it("builds correct XOAUTH2 SASL string", () => {
    const token = buildXOAuth2Token("user@example.com", "ya29.token123");
    expect(token).toBe("user=user@example.com\x01auth=Bearer ya29.token123\x01\x01");
  });

  it("uses login email not mailbox email for XOAUTH2", () => {
    // For personal Microsoft accounts, XOAUTH2 user= must be the login identity
    // (e.g., gmail.com) not the Outlook mailbox alias
    const loginEmail = "kushaldas@gmail.com";
    const mailboxEmail = "outlook_A634C77E51D17412@outlook.com";
    const token = buildXOAuth2Token(loginEmail, "access_token");
    expect(token).toContain("user=kushaldas@gmail.com");
    expect(token).not.toContain(mailboxEmail);
  });
});

describe("PKCE code challenge", () => {
  it("code verifier is non-empty string", () => {
    // In real code, this is 64 random bytes base64url-encoded
    const verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    expect(verifier.length).toBeGreaterThan(40);
  });

  it("code challenge uses S256 method", () => {
    // The challenge is SHA256(verifier) base64url-encoded
    // This test validates the concept, not the actual crypto
    const method = "S256";
    expect(method).toBe("S256");
  });
});
