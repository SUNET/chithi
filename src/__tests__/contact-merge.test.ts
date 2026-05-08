import { describe, it, expect } from "vitest";
import {
  mergeContacts,
  defaultChoices,
  applyMergeChoices,
  buildListItems,
} from "@/lib/contact-merge";
import type { Contact } from "@/lib/types";

function makeContact(overrides: Partial<Contact>): Contact {
  return {
    id: "c1",
    book_id: "book1",
    uid: "uid-c1",
    display_name: "",
    emails_json: "[]",
    phones_json: "[]",
    addresses_json: "[]",
    organization: null,
    title: null,
    notes: null,
    vcard_data: null,
    remote_id: null,
    etag: null,
    ...overrides,
  } as Contact;
}

describe("mergeContacts", () => {
  it("keeps the primary's identity fields", () => {
    const a = makeContact({ id: "a", uid: "uid-a", remote_id: "rem-a", etag: "et-a", book_id: "b1" });
    const b = makeContact({ id: "b", uid: "uid-b", remote_id: "rem-b", etag: "et-b", book_id: "b1" });
    const merged = mergeContacts(a, b);
    expect(merged.id).toBe("a");
    expect(merged.uid).toBe("uid-a");
    expect(merged.remote_id).toBe("rem-a");
    expect(merged.etag).toBe("et-a");
    expect(merged.book_id).toBe("b1");
  });

  it("primary atomic fields win when populated", () => {
    const a = makeContact({ display_name: "Alice", organization: "Acme", title: "Engineer" });
    const b = makeContact({ display_name: "Alicia", organization: "Beta", title: "Manager" });
    const merged = mergeContacts(a, b);
    expect(merged.display_name).toBe("Alice");
    expect(merged.organization).toBe("Acme");
    expect(merged.title).toBe("Engineer");
  });

  it("falls through to secondary when primary's atomic field is empty", () => {
    const a = makeContact({ display_name: "Alice", organization: "", title: null });
    const b = makeContact({ display_name: "Alicia", organization: "Beta", title: "Manager" });
    const merged = mergeContacts(a, b);
    // display_name stays primary (non-empty)
    expect(merged.display_name).toBe("Alice");
    // organization / title fall through
    expect(merged.organization).toBe("Beta");
    expect(merged.title).toBe("Manager");
  });

  it("unions emails and dedupes case-insensitively", () => {
    const a = makeContact({
      emails_json: JSON.stringify([
        { email: "a@example.com", label: "work" },
      ]),
    });
    const b = makeContact({
      emails_json: JSON.stringify([
        { email: "A@Example.com", label: "home" }, // duplicate of a@example.com (different case)
        { email: "b@example.com", label: "other" },
      ]),
    });
    const merged = mergeContacts(a, b);
    const emails = JSON.parse(merged.emails_json);
    expect(emails).toHaveLength(2);
    expect(emails[0].email).toBe("a@example.com"); // primary's casing wins
    expect(emails[1].email).toBe("b@example.com");
  });

  it("unions phones and preserves order primary-then-secondary", () => {
    const a = makeContact({
      phones_json: JSON.stringify([{ number: "+1-555-1111", label: "mobile" }]),
    });
    const b = makeContact({
      phones_json: JSON.stringify([
        { number: "+1-555-1111", label: "work" }, // dup
        { number: "+1-555-2222", label: "work" },
      ]),
    });
    const merged = mergeContacts(a, b);
    const phones = JSON.parse(merged.phones_json);
    expect(phones).toHaveLength(2);
    expect(phones[0].number).toBe("+1-555-1111");
    expect(phones[1].number).toBe("+1-555-2222");
  });

  it("concatenates notes with a separator when both differ", () => {
    const a = makeContact({ notes: "Met at conference 2024." });
    const b = makeContact({ notes: "Likes hiking." });
    const merged = mergeContacts(a, b);
    expect(merged.notes).toBe("Met at conference 2024.\n---\nLikes hiking.");
  });

  it("keeps the single non-empty notes when only one side has them", () => {
    const a = makeContact({ notes: null });
    const b = makeContact({ notes: "Likes hiking." });
    expect(mergeContacts(a, b).notes).toBe("Likes hiking.");

    const c = makeContact({ notes: "Met at conference 2024." });
    const d = makeContact({ notes: "" });
    expect(mergeContacts(c, d).notes).toBe("Met at conference 2024.");
  });

  it("does not duplicate identical notes", () => {
    const a = makeContact({ notes: "Same note." });
    const b = makeContact({ notes: "Same note." });
    expect(mergeContacts(a, b).notes).toBe("Same note.");
  });

  it("survives malformed JSON in either list (treats as empty)", () => {
    const a = makeContact({ emails_json: "not json" });
    const b = makeContact({
      emails_json: JSON.stringify([{ email: "b@example.com", label: "work" }]),
    });
    const merged = mergeContacts(a, b);
    const emails = JSON.parse(merged.emails_json);
    expect(emails).toHaveLength(1);
    expect(emails[0].email).toBe("b@example.com");
  });

  it("dedupes addresses by serialized payload", () => {
    const sameAddr = { street: "1 Main", city: "Town", country: "US" };
    const a = makeContact({ addresses_json: JSON.stringify([sameAddr]) });
    const b = makeContact({
      addresses_json: JSON.stringify([sameAddr, { street: "2 Other", city: "Town", country: "US" }]),
    });
    const merged = mergeContacts(a, b);
    const addrs = JSON.parse(merged.addresses_json);
    expect(addrs).toHaveLength(2);
  });
});

describe("defaultChoices / applyMergeChoices", () => {
  it("defaults atomic fields to keeper when populated, loser when keeper is empty", () => {
    const k = makeContact({ display_name: "Alice", organization: "", title: null });
    const l = makeContact({ display_name: "Alicia", organization: "Beta", title: "Manager" });
    const c = defaultChoices(k, l);
    expect(c.display_name).toBe("keeper");
    expect(c.organization).toBe("loser");
    expect(c.title).toBe("loser");
  });

  it("defaults notes to 'both' only when both sides have non-empty differing notes", () => {
    expect(defaultChoices(
      makeContact({ notes: "A" }),
      makeContact({ notes: "B" }),
    ).notes).toBe("both");
    expect(defaultChoices(
      makeContact({ notes: "" }),
      makeContact({ notes: "B" }),
    ).notes).toBe("loser");
    expect(defaultChoices(
      makeContact({ notes: "Same" }),
      makeContact({ notes: "Same" }),
    ).notes).toBe("keeper");
  });

  it("user can override an atomic field to the loser's value", () => {
    const k = makeContact({ display_name: "Alice", organization: "Acme" });
    const l = makeContact({ display_name: "Alicia Smith", organization: "Beta" });
    const c = defaultChoices(k, l);
    c.display_name = "loser";
    const merged = applyMergeChoices(k, l, c);
    expect(merged.display_name).toBe("Alicia Smith");
    // Untouched: still keeper
    expect(merged.organization).toBe("Acme");
  });

  it("notes 'keeper' / 'loser' / 'both' produce the three expected results", () => {
    const k = makeContact({ notes: "Met at conf." });
    const l = makeContact({ notes: "Likes hiking." });
    const c = defaultChoices(k, l);
    c.notes = "keeper";
    expect(applyMergeChoices(k, l, c).notes).toBe("Met at conf.");
    c.notes = "loser";
    expect(applyMergeChoices(k, l, c).notes).toBe("Likes hiking.");
    c.notes = "both";
    expect(applyMergeChoices(k, l, c).notes).toBe("Met at conf.\n---\nLikes hiking.");
  });

  it("excludes a list item the user unchecks", () => {
    const k = makeContact({
      emails_json: JSON.stringify([{ email: "alice@a.com", label: "work" }]),
    });
    const l = makeContact({
      emails_json: JSON.stringify([{ email: "alice@b.com", label: "home" }]),
    });
    const c = defaultChoices(k, l);
    expect(c.emails).toHaveLength(2);
    // User unchecks the loser's email.
    c.emails[1].include = false;
    const merged = applyMergeChoices(k, l, c);
    expect(JSON.parse(merged.emails_json)).toEqual([
      { email: "alice@a.com", label: "work" },
    ]);
  });

  it("buildListItems annotates each entry with its source side and dedupes case-insensitively", () => {
    const items = buildListItems(
      JSON.stringify([{ email: "k@x.com" }]),
      JSON.stringify([{ email: "K@X.com" }, { email: "l2@x.com" }]),
      (it) => String(it.email ?? ""),
    );
    expect(items).toHaveLength(2); // "K@X.com" collapses against "k@x.com"
    expect(items[0].source).toBe("keeper");
    expect(items[0].item.email).toBe("k@x.com"); // keeper's casing wins
    expect(items[1].source).toBe("loser");
    expect(items[1].item.email).toBe("l2@x.com");
    expect(items.every((i) => i.include)).toBe(true);
  });

  it("identity fields always come from keeper regardless of choices", () => {
    const k = makeContact({ id: "k", uid: "uid-k", remote_id: "r-k", etag: "e-k" });
    const l = makeContact({ id: "l", uid: "uid-l", remote_id: "r-l", etag: "e-l" });
    const c = defaultChoices(k, l);
    // Even with every choice flipped to the loser, identity stays.
    c.display_name = "loser";
    c.organization = "loser";
    c.title = "loser";
    c.notes = "loser";
    const merged = applyMergeChoices(k, l, c);
    expect(merged.id).toBe("k");
    expect(merged.uid).toBe("uid-k");
    expect(merged.remote_id).toBe("r-k");
    expect(merged.etag).toBe("e-k");
  });
});
