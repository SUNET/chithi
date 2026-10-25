import { describe, expect, it } from "vitest";
import {
  encodeMailtoAddress,
  parseMailto,
  sanitizeTel,
} from "@/lib/mailto";

describe("encodeMailtoAddress", () => {
  it("encodes unsafe characters without obscuring common address punctuation", () => {
    expect(encodeMailtoAddress(" alice+tag@example.com ")).toBe(
      "alice+tag@example.com",
    );
    expect(encodeMailtoAddress("alice smith@example.com")).toBe(
      "alice%20smith@example.com",
    );
  });
});

describe("sanitizeTel", () => {
  it("keeps dialer characters and removes presentation text", () => {
    expect(sanitizeTel("+46 (70) 123-45.67 ext. 8#")).toBe(
      "+4670123-45.67.8#",
    );
  });
});

describe("parseMailto", () => {
  it("rejects other URI schemes", () => {
    expect(parseMailto("https://example.com")).toBeNull();
  });

  it("accepts a case-insensitive scheme and decodes message fields", () => {
    expect(
      parseMailto(
        "MAILTO:alice@example.com?subject=Hello+there&body=Line%201%0ALine%202",
      ),
    ).toEqual({
      to: "alice@example.com",
      subject: "Hello there",
      body: "Line 1\nLine 2",
    });
  });

  it("merges repeated and comma-separated recipient parameters", () => {
    expect(
      parseMailto(
        "mailto:alice@example.com,bob@example.com" +
          "?to=bob@example.com&to=carol%40example.com%2Cdave%40example.com" +
          "&cc=team%40example.com&bcc=archive%40example.com",
      ),
    ).toEqual({
      to: "alice@example.com, bob@example.com, carol@example.com, dave@example.com",
      cc: "team@example.com",
      bcc: "archive@example.com",
    });
  });

  it("discards incomplete recipients and tolerates malformed path encoding", () => {
    expect(
      parseMailto(
        "mailto:alice%ZZ@example.com,invalid?to=missing-at&cc=valid@example.com",
      ),
    ).toEqual({
      to: "alice%ZZ@example.com",
      cc: "valid@example.com",
    });
  });
});
