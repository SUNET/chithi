import { describe, expect, it } from "vitest";
import {
  ADD_ACCOUNT_TYPES,
  accountSecondaryLabel,
  accountTypeDescription,
  accountTypeLabel,
  accountTypeLabelLong,
  isFastmailJmapUrl,
} from "@/lib/account-types";

describe("isFastmailJmapUrl", () => {
  it("accepts the exact Fastmail API host over https", () => {
    expect(isFastmailJmapUrl("https://api.fastmail.com")).toBe(true);
    expect(isFastmailJmapUrl("https://API.FASTMAIL.COM/jmap/session")).toBe(true);
  });

  it("rejects lookalike hosts, http and garbage", () => {
    expect(isFastmailJmapUrl("https://api.fastmail.com.attacker.example")).toBe(false);
    expect(isFastmailJmapUrl("http://api.fastmail.com")).toBe(false);
    expect(isFastmailJmapUrl("api.fastmail.com")).toBe(false);
    expect(isFastmailJmapUrl("")).toBe(false);
  });
});

describe("account type labels", () => {
  it("spells out branded providers and upper-cases protocols", () => {
    expect(accountTypeLabelLong("o365")).toBe("Microsoft 365");
    expect(accountTypeLabelLong("talk")).toBe("Nextcloud Talk");
    expect(accountTypeLabelLong("visio")).toBe("La Suite Visio");
    expect(accountTypeLabelLong("imap")).toBe("IMAP");
  });

  it("labels list entries by provider, protocol or service", () => {
    expect(accountTypeLabel({ provider: "gmail" })).toBe("GMAIL");
    expect(accountTypeLabel({ mail_protocol: "jmap" })).toBe("JMAP");
    expect(
      accountTypeLabel({ has_calendar_binding: true, has_contacts_binding: true }),
    ).toBe("Calendar and Contacts");
    expect(accountTypeLabel({ has_contacts_binding: true })).toBe("Contacts");
    expect(accountTypeLabel({ meet_protocol: "zoom" })).toBe("Zoom");
    expect(accountTypeLabel({ meet_protocol: "visio" })).toBe("La Suite Visio");
    expect(accountTypeLabel({})).toBe("");
  });

  it("falls back from email to username for the secondary line", () => {
    expect(accountSecondaryLabel({ email: "a@x.org", username: "u" })).toBe("a@x.org");
    expect(accountSecondaryLabel({ email: "", username: "u" })).toBe("u");
    expect(accountSecondaryLabel({ email: "", username: "" })).toBe("");
  });

  it("has a description for every picker card", () => {
    expect(ADD_ACCOUNT_TYPES).toContain("visio");
    for (const t of ADD_ACCOUNT_TYPES) {
      expect(accountTypeDescription(t)).toBeTruthy();
    }
  });
});
