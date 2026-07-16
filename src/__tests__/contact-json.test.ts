import { describe, expect, it } from "vitest";
import { parseEmails, parseFirstEmail, parsePhones } from "@/lib/contact-json";

describe("contact-json", () => {
  it("parses well-formed lists", () => {
    expect(parseEmails('[{"email":"a@x.org","label":"work"}]')).toEqual([
      { email: "a@x.org", label: "work" },
    ]);
    expect(parsePhones('[{"number":"+46","label":"mobile"}]')).toEqual([
      { number: "+46", label: "mobile" },
    ]);
    expect(parseFirstEmail('[{"email":"a@x.org"},{"email":"b@x.org"}]')).toBe("a@x.org");
  });

  it("degrades malformed JSON to empty values", () => {
    expect(parseEmails("not json")).toEqual([]);
    expect(parsePhones("{broken")).toEqual([]);
    expect(parseFirstEmail("")).toBe("");
    expect(parseFirstEmail("[]")).toBe("");
  });
});
