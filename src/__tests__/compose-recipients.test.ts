import { describe, it, expect } from "vitest";
import {
  formatRecipient,
  getLastRecipientTerm,
  getRecipientSearch,
  parseRecipients,
  rankRecipientAddressMatch,
  recipientDeduplicationKey,
  replaceLastRecipient,
} from "@/lib/compose-recipients";

describe("parseRecipients", () => {
  it.each<[string, string[]]>([
    ["", []],
    ["   ", []],
    [",", []],
    [";", []],
    [", ; ,, ;; , ", []],
    ["alice@example.com", ["alice@example.com"]],
    ["  alice@example.com  ", ["alice@example.com"]],
    ["Alice Smith <alice@example.com>", ["alice@example.com"]],
    ["Alice Smith <alice@example.com>, ", ["alice@example.com"]],
    ["<alice@example.com>", ["alice@example.com"]],
    [
      "Alice <alice@a.com>, Bob <bob@b.com>",
      ["alice@a.com", "bob@b.com"],
    ],
    ["alice@a.com, Bob <bob@b.com>", ["alice@a.com", "bob@b.com"]],
    [
      "kushal@sunet.se <kushal@sunet.se>, ",
      ["kushal@sunet.se"],
    ],
    ["Alice <alice@a.com>; Bob <bob@b.com>", ["alice@a.com", "bob@b.com"]],
    [
      "Alice <alice@a.com>; Bob <bob@b.com>;   ",
      ["alice@a.com", "bob@b.com"],
    ],
    ["alice@a.com,bob@b.com", ["alice@a.com", "bob@b.com"]],
    ["Alice <alice@a.com>,Bob <bob@b.com>", ["alice@a.com", "bob@b.com"]],
    [
      ", ; alice@a.com,, ; Bob <bob@b.com>;;; , carol@c.com, ,",
      ["alice@a.com", "bob@b.com", "carol@c.com"],
    ],
    ["same@x, same@x", ["same@x", "same@x"]],
    [
      '"Doe, Jane" <jane@example.com>,bob@example.com',
      ["jane@example.com", "bob@example.com"],
    ],
    [
      '"Team; West" <west@example.com>;east@example.com',
      ["west@example.com", "east@example.com"],
    ],
    [
      String.raw`"Doe, \"Johnny\" \\ Ops" <john@x>,next@y`,
      ["john@x", "next@y"],
    ],
    [
      String.raw`Alice "The \"Great\"" Smith <alice@x>`,
      ["alice@x"],
    ],
    ['"a,b"@x,next@y', ['"a,b"@x', "next@y"]],
    ['"a;b"@x;next@y', ['"a;b"@x', "next@y"]],
    ['"a@b"@x', ['"a@b"@x']],
    ['"a>b"@x', ['"a>b"@x']],
    ['Angle <"a>b"@x>', ['"a>b"@x']],
    ['Angle <"a<b"@x>', ['"a<b"@x']],
    ['"a b"@x', ['"a b"@x']],
    ['"a(comment)[literal]:b"@x', ['"a(comment)[literal]:b"@x']],
    [
      String.raw`Quoted <"comma,semi;at@angle>quote\"slash\\😀"@例え.テスト>`,
      [String.raw`"comma,semi;at@angle>quote\"slash\\😀"@例え.テスト`],
    ],
    [
      '"alice@example.com" <bob@example.com>',
      ["bob@example.com"],
    ],
    ["δοκιμή@παράδειγμα.δοκιμή", ["δοκιμή@παράδειγμα.δοκιμή"]],
    ["用户 <用户@例子.公司>", ["用户@例子.公司"]],
    ["Üser+tag@EXAMPLE", ["Üser+tag@EXAMPLE"]],
    ["local@localhost", ["local@localhost"]],
    ["(work) alice@example.com (Alice)", ["alice@example.com"]],
    [
      "Alice (work, team; nested (west)) <alice@example.com> (primary),bob@x",
      ["alice@example.com", "bob@x"],
    ],
    [
      String.raw`Alice (escaped \) \\ (nested, ;)) <alice@x>,bob@y`,
      ["alice@x", "bob@y"],
    ],
    [
      'Alice (ignore " < > @ [ : , ;) <alice@x>',
      ["alice@x"],
    ],
    [
      "(before) <(local) alice (note) @ (domain) example.com (after)> (end)",
      ["alice@example.com"],
    ],
    ["alice (work)@example.com", ["alice@example.com"]],
    ["alice@(work)example.com", ["alice@example.com"]],
    ["alice @ example.com", ["alice@example.com"]],
    ['(note) "a b" (note) @ x (note)', ['"a b"@x']],
    ["alice@[127.0.0.1]", ["alice@[127.0.0.1]"]],
    ["alice@[IPv6:2001:db8::1]", ["alice@[IPv6:2001:db8::1]"]],
    [
      "IPv6 <alice@[IPv6:2001:db8::1]>; bob@x",
      ["alice@[IPv6:2001:db8::1]", "bob@x"],
    ],
    [
      '"a,@;>"@[IPv6:2001:db8::1], next@y',
      ['"a,@;>"@[IPv6:2001:db8::1]', "next@y"],
    ],
  ])("parses %s", (input, addresses) => {
    expect(parseRecipients(input)).toEqual({ ok: true, addresses });
  });

  it.each(["\u00a0", "\u2003", "\u2028", "\u2029", "\ufeff"])(
    "preserves Unicode %j as local-part content, including at either edge",
    (char) => {
      for (const local of [`${char}user`, `us${char}er`, `user${char}`, char]) {
        const email = `${local}@example.com`;
        expect(parseRecipients(`  ${email} ; Name < ${email} >, `)).toEqual({
          ok: true, addresses: [email, email],
        });
      }
      expect(parseRecipients(`good@x, ;  ${char}  `)).toEqual({
        ok: false,
        error: { index: 2, message: expect.stringMatching(/missing '@'/i) },
      });
    },
  );

  it("preserves a domain-ending NBSP for the backend to validate", () => {
    const email = "user@example.com\u00a0";
    expect(parseRecipients(`  ${email} ; Name < ${email} > `)).toEqual({
      ok: true, addresses: [email, email],
    });
  });

  it.each<[string, RegExp]>([
    ["not-an-address", /missing '@'/i],
    ["Alice Smith", /missing '@'/i],
    ["@example.com", /missing local part/i],
    ["alice@", /missing domain/i],
    ["Alice <>", /empty angle-bracket address/i],
    ["< >", /empty angle-bracket address/i],
    ["Alice <(comment)>", /empty angle-bracket address/i],
    ["Alice <not-an-address>", /missing '@'/i],
    ["Alice <@x>", /missing local part/i],
    ["Alice <a@>", /missing domain/i],
    ['"Doe, Jane <jane@x>', /unclosed quote/i],
    ['"a,b@x', /unclosed quote/i],
    [String.raw`"a\"@x`, /unclosed quote/i],
    ["Alice <alice@x", /unclosed angle bracket/i],
    ["alice@x (work", /unclosed comment/i],
    ["alice@x (work (nested)", /unclosed comment/i],
    [String.raw`alice@x (escaped \)`, /unclosed comment/i],
    ["alice@[IPv6:2001:db8::1", /unclosed domain literal/i],
    [String.raw`alice@[escaped\]`, /unclosed domain literal/i],
    ["Alice <<alice@x>>", /extra angle bracket/i],
    ["Alice <alice@x>>", /unexpected closing angle/i],
    ["alice@x>", /unexpected closing angle/i],
    ["alice@x)", /unexpected closing comment/i],
    ["alice@x]", /unexpected closing domain literal/i],
    ["alice@[[x]]", /opening bracket inside a domain literal/i],
    ["Alice <alice@x> junk", /unexpected text after '>'/i],
    ["<alice@x>garbage", /unexpected text after '>'/i],
    ['<alice@x> "trailing name"', /unexpected text after '>'/i],
    ["<alice@x> bob@y", /separate recipients/i],
    ["A <a@x>B <b@y>", /separate recipients/i],
    ["A <a@x> B <b@y>", /separate recipients/i],
    ["<a@x><b@y>", /separate recipients/i],
    ["a@x <b@y>", /unexpected address in display name/i],
    ["a@x Bob <b@y>", /unexpected address in display name/i],
    ["a@x a@x <a@x>", /unexpected address in display name/i],
    ["Alice@x <alice@x>", /unexpected address in display name/i],
    ["a@x b@y", /multiple unquoted '@'/i],
    ["a@x@other", /multiple unquoted '@'/i],
    ['"a@b"@x@y', /multiple unquoted '@'/i],
    ["a b@x", /whitespace or word fragments/i],
    ["a@x garbage", /whitespace or word fragments/i],
    ["junk alice@x", /whitespace or word fragments/i],
    ["alice@exam ple.com", /whitespace or word fragments/i],
    ["foo(comment)bar@x", /whitespace or word fragments/i],
    ["a@exam(comment)ple.com", /whitespace or word fragments/i],
    ['"a"junk@x', /whitespace or word fragments/i],
    ['"a""b"@x', /whitespace or word fragments/i],
    ["a@[x]junk", /whitespace or word fragments/i],
    ["a@[x][y]", /whitespace or word fragments/i],
    ["[local]@x", /invalid local part/i],
    ['a@"example.com"', /invalid domain/i],
    ["a@[]", /empty domain literal/i],
    ["[Team] <a@x>", /invalid display name/i],
    [String.raw`a\b@x`, /backslash escapes require quotes/i],
    [String.raw`Alice\ Smith <a@x>`, /backslash escapes require quotes/i],
    ["<a@x,b@y>", /separator inside angle brackets/i],
    ["<a@x;b@y>", /separator inside angle brackets/i],
    ["Friends: a@x,b@y;", /groups are not supported/i],
    ["Friends:;", /groups are not supported/i],
    ['"Team, West": a@x;', /groups are not supported/i],
    ["Friends: a@[IPv6:2001:db8::1];", /groups are not supported/i],
    ["Doe, Jane <jane@x>", /missing '@'/i],
    ["(comment only)", /missing '@'/i],
  ])("rejects %s without returning a partial list", (input, message) => {
    expect(parseRecipients(input)).toEqual({
      ok: false,
      error: { index: 1, message: expect.stringMatching(message) },
    });
    expect(parseRecipients(`, ; good@x, ; ${input}`)).toEqual({
      ok: false,
      error: { index: 2, message: expect.stringMatching(message) },
    });
  });

  it.each(["\0", "\t", "\r", "\n", "\u001f", "\u007f", "\u0085", "\u009f"])(
    "rejects control character %j even in protected syntax or an otherwise blank field",
    (control) => {
      for (const token of [
        control,
        `${control}a@x`,
        `a@x${control}`,
        `a${control}@x`,
        `"a${control}b"@x`,
        `"A${control}B" <a@x>`,
        `a@x (comment${control})`,
        `a@[literal${control}]`,
      ]) {
        expect(parseRecipients(`good@x, ; ${token}`)).toEqual({
          ok: false,
          error: {
            index: 2,
            message: expect.stringMatching(/control characters/i),
          },
        });
      }
    },
  );

  it("uses a one-based nonempty-recipient ordinal after ignored empty tokens", () => {
    expect(parseRecipients(',; a@x,,; "B, C" <b@y>; , bad; ; c@z')).toEqual({
      ok: false,
      error: { index: 3, message: expect.stringMatching(/missing '@'/i) },
    });
  });

  it.each<[string, number]>([
    ["invalid, a@x, b@y; , ", 1],
    ["a@x, invalid, b@y; , ", 2],
    ["a@x, b@y, invalid; , ", 3],
  ])("fails the entire list for an invalid member in %s", (input, index) => {
    expect(parseRecipients(input)).toEqual({
      ok: false,
      error: { index, message: expect.stringMatching(/missing '@'/i) },
    });
  });

  it("preserves opaque domain literal content for backend validation", () => {
    const email = String.raw`a@[escaped\],comma;@>:(text)"]`;
    expect(parseRecipients(`Literal <${email}>,next@y`)).toEqual({
      ok: true,
      addresses: [email, "next@y"],
    });
  });
});

describe("getLastRecipientTerm", () => {
  it.each<[string, string]>([
    ["", ""],
    ["   ", ""],
    ["ali", "ali"],
    ["alice@example.com, bo", "bo"],
    ["alice@example.com; ku", "ku"],
    ["alice@example.com,bo", "bo"],
    ["alice@example.com, ", ""],
    ["alice@example.com; ", ""],
    [", ; , ;  ", ""],
    ['"Doe, Jane" <jane@x>; bo', "bo"],
    ['alice@x, "Doe, Ja', '"Doe, Ja'],
    [String.raw`alice@x; "Doe, \"Ja`, String.raw`"Doe, \"Ja`],
    ['alice@x; "a,;@>"@ex', '"a,;@>"@ex'],
    ['alice@x; "a,;@', '"a,;@'],
    ['Angle <"a>b"@x>, bo', "bo"],
    ["alice@x; Bob <bo", "Bob <bo"],
    ["alice@x; Bob <bo, bb", "Bob <bo, bb"],
    ["alice@x; Bob (work, team; west)", "Bob (work, team; west)"],
    ["alice@x; Bob (work, (te; st", "Bob (work, (te; st"],
    ["Alice (work, (team; west)) <a@x>; bo", "bo"],
    ["a@[IPv6:2001:db8::1]; bo", "bo"],
    ["a@x; b@[IPv6:2001:db8::", "b@[IPv6:2001:db8::"],
    ["a@x; b@[unfinished,semi;", "b@[unfinished,semi;"],
    [String.raw`a@x; b@[escaped\],semi;`, String.raw`b@[escaped\],semi;`],
    ["a@x; <broken>>; bo", "bo"],
    ["a@x, \t bo\t ", "bo"],
    ["a@x,\n bo\t", "\n bo"],
    ["a@x, \rbo@x\n ", "\rbo@x\n"],
    ["a@x, \u00a0user@x ", "\u00a0user@x"],
    ["a@x, user\u2003@x ", "user\u2003@x"],
    ["a@x, \t\u00a0\u2003\t ", "\u00a0\u2003"],
    ["a@x, user@x\u00a0 ", "user@x\u00a0"],
  ])("gets the last term from %s without requiring valid syntax", (input, term) => {
    expect(getLastRecipientTerm(input)).toBe(term);
  });

  it("leaves the minimum query length decision to the caller", () => {
    expect(getLastRecipientTerm("a").length < 2).toBe(true);
  });
});

describe("getRecipientSearch", () => {
  it.each<[string, string]>([
    ["", ""],
    ['existing@x; "Doe, Ja', "Doe, Ja"],
    [String.raw`existing@x; "West, \"Te`, 'West, "Te'],
    [String.raw`"Back\\slash"`, String.raw`Back\slash`],
    ['"Doe, Jane" <jane@ex', "jane@ex"],
    ['"Doe, Jane" <jane@example.com>', "jane@example.com"],
    ['existing@x; Bob <bo', "bo"],
    ['existing@x; <', ""],
    ['"a,b"@example.com', '"a,b"@example.com'],
    ['Name <"a>b"@example.com>', '"a>b"@example.com'],
    ['Alice (work, team)', "Alice"],
    ['"Doe," Jane', "Doe, Jane"],
  ])("derives a substring-search term from %s", (input, term) => {
    expect(getRecipientSearch(input).query).toBe(term);
  });

  it.each<[string, string]>([
    [" \tuser@ex\t ", "user@ex"],
    [" \u00a0user@ex ", "\u00a0user@ex"],
    [" user\u2003@ex ", "user\u2003@ex"],
    [" user@ex\u00a0 ", "user@ex\u00a0"],
    ["Name < \tuser@ex\t >", "user@ex"],
    ["Name < \u00a0user@ex >", "\u00a0user@ex"],
    ["Name < user\u2003@ex >", "user\u2003@ex"],
    ["Name < user@ex\u00a0 >", "user@ex\u00a0"],
    ["Name < \u00a0\u2003 >", "\u00a0\u2003"],
    [" \ruser@ex\n ", "\ruser@ex\n"],
    ["Name < \nuser@ex\r >", "\nuser@ex\r"],
  ])("removes only ASCII SP/HTAB padding from address query %j", (input, query) => {
    expect(getRecipientSearch(`existing@x; ${input}`)).toEqual({ query, kind: "address" });
  });

  it("keeps name-only search normalization separate from address padding", () => {
    expect(getRecipientSearch('\u00a0"Alice"\u2003')).toEqual({
      query: "Alice", kind: "name",
    });
  });

  it("distinguishes a quoted name containing @ from an address query", () => {
    expect(getRecipientSearch('"Team@Work"')).toEqual({ query: "Team@Work", kind: "name" });
    expect(getRecipientSearch('Name <ali')).toEqual({ query: "ali", kind: "address" });
    expect(getRecipientSearch('"a@b"@example.com')).toEqual({
      query: '"a@b"@example.com', kind: "address",
    });
  });
});

describe("rankRecipientAddressMatch", () => {
  it.each<[string, string, number]>([
    ["work@example.com", "work@example.com", 0],
    ["work@example.com", "work@EXAMPLE.com", 1],
    ["Work@example.com", "work@EXAMPLE.com", 4],
    ["other-work@example.com", "work@EXAMPLE.com", 3],
    ["work@example.com", "work@EXAM", 2],
    ['"a@b"@EXAMPLE.com', '"a@b"@example.com', 1],
    ['"A@b"@example.com', '"a@b"@EXAMPLE.com', 4],
    ["\u00a0user@example.com", "\u00a0user@EXAMPLE.com", 1],
    ["user@example.com", "\u00a0user@EXAMPLE.com", 4],
    ["\u00a0user@example.com", "user@EXAMPLE.com", 3],
    ["us\u00a0er@example.com", "us\u00a0er@EXAM", 2],
    ["user\u2003@example.com", "user\u2003@EXAMPLE.com", 1],
    ["user@example.com", "user\u2003@EXAMPLE.com", 4],
    ["user@example.com", "user@example.com\u00a0", 4],
  ])("ranks %s against %s without folding local-part case", (email, query, rank) => {
    expect(rankRecipientAddressMatch(email, query)).toBe(rank);
  });
});

describe("recipientDeduplicationKey", () => {
  it.each<[string, string]>([
    ["user@example.com", "user@EXAMPLE.com"],
    ["user@Example.COM", "user@example.com"],
    ["User@example.com", "User@EXAMPLE.COM"],
  ])("gives %s and %s the same key regardless of domain case", (email, equivalent) => {
    expect(recipientDeduplicationKey(email)).toBe(recipientDeduplicationKey(equivalent));
  });

  it("tags valid mailbox keys and preserves local-part case", () => {
    expect(recipientDeduplicationKey("user@EXAMPLE.com"))
      .toBe(JSON.stringify(["mailbox", "user", "example.com"]));
    expect(recipientDeduplicationKey("User@example.com"))
      .toBe(JSON.stringify(["mailbox", "User", "example.com"]));
    expect(recipientDeduplicationKey("user@EXAMPLE.com"))
      .not.toBe(recipientDeduplicationKey("User@example.com"));
  });

  it.each(["\u00a0", "\u2003"])("keeps local-part %j significant while folding domain case", (char) => {
    const locals = ["user", `${char}user`, `us${char}er`, `user${char}`];
    const keys = locals.map((local) => recipientDeduplicationKey(`${local}@example.com`));
    expect(new Set(keys).size).toBe(locals.length);
    for (const [index, local] of locals.entries()) {
      expect(recipientDeduplicationKey(`  ${local}@EXAMPLE.com  `)).toBe(keys[index]);
      expect(keys[index]).toBe(JSON.stringify(["mailbox", local, "example.com"]));
    }
    expect(recipientDeduplicationKey(`user@example.com${char}`)).not.toBe(keys[0]);
  });

  it("preserves quoted local-part @ signs, escaping and case while folding only the domain", () => {
    const localPart = String.raw`"Team@Work\"Ops\\West"`;
    const email = `${localPart}@EXAMPLE.com`;
    expect(recipientDeduplicationKey(email))
      .toBe(JSON.stringify(["mailbox", localPart, "example.com"]));
    expect(recipientDeduplicationKey(email))
      .toBe(recipientDeduplicationKey(`${localPart}@example.COM`));
    expect(recipientDeduplicationKey(email))
      .not.toBe(recipientDeduplicationKey(String.raw`"team@Work\"Ops\\West"@example.com`));
    expect(recipientDeduplicationKey(email))
      .not.toBe(recipientDeduplicationKey(String.raw`"Team@Work\"OpsWest"@example.com`));
    expect(recipientDeduplicationKey(String.raw`"team\@work"@EXAMPLE.com`))
      .not.toBe(recipientDeduplicationKey('"team@work"@example.com'));
  });

  it.each([
    "",
    "not-an-address",
    "user@",
    '"Unclosed@example.com',
    "user@example.com, other@example.com",
    "User Name <user@example.com>",
    JSON.stringify(["mailbox", "user", "example.com"]),
  ])("gives invalid bare input %j a stable raw key that cannot collide with a mailbox key", (input) => {
    const key = recipientDeduplicationKey(input);
    expect(typeof key).toBe("string");
    expect(key).toBe(recipientDeduplicationKey(input));
    expect(key).not.toBe(recipientDeduplicationKey("user@example.com"));
    const tagged = JSON.parse(key);
    expect(tagged).toEqual([expect.any(String), input]);
    expect(tagged[0]).not.toBe("mailbox");
  });

  it("does not case-fold or merge different invalid raw inputs", () => {
    const invalidInputs = [
      "user@", "User@", "user@@example.com", "user@@EXAMPLE.com",
      "user@example.com, other@example.com", "user@example.com; other@example.com",
    ];
    const keys = invalidInputs.map(recipientDeduplicationKey);
    expect(new Set(keys).size).toBe(invalidInputs.length);
  });
});

describe("replaceLastRecipient", () => {
  it.each<[string, string]>([
    ["", "Alice Smith <alice@example.com>, "],
    ["ali", "Alice Smith <alice@example.com>, "],
    ["  ali  ", "  Alice Smith <alice@example.com>, "],
    ["  ", "  Alice Smith <alice@example.com>, "],
    [" \t ", " \t Alice Smith <alice@example.com>, "],
    ["\u00a0ali", "Alice Smith <alice@example.com>, "],
    [" \u2003ali", " Alice Smith <alice@example.com>, "],
    ["bob@test.com; \t\u00a0\u2003", "bob@test.com; \tAlice Smith <alice@example.com>, "],
    ["bob@test.com; \nali", "bob@test.com; Alice Smith <alice@example.com>, "],
    [
      "bob@test.com, ali",
      "bob@test.com, Alice Smith <alice@example.com>, ",
    ],
    ["bob@test.com,ali", "bob@test.com,Alice Smith <alice@example.com>, "],
    [
      "bob@test.com;  ali \t",
      "bob@test.com;  Alice Smith <alice@example.com>, ",
    ],
    ["bob@test.com;", "bob@test.com;Alice Smith <alice@example.com>, "],
    [
      "bob@test.com;  ",
      "bob@test.com;  Alice Smith <alice@example.com>, ",
    ],
    [
      '  "Doe, Jane" <jane@x> ;\t ali  ',
      '  "Doe, Jane" <jane@x> ;\t Alice Smith <alice@example.com>, ',
    ],
    [
      String.raw`"Doe, \"J\"" <j@x>  ;  "a,;>"@x (work, nested (team; west)),ali`,
      String.raw`"Doe, \"J\"" <j@x>  ;  "a,;>"@x (work, nested (team; west)),Alice Smith <alice@example.com>, `,
    ],
    [
      "a@[IPv6:2001:db8::1];  ali",
      "a@[IPv6:2001:db8::1];  Alice Smith <alice@example.com>, ",
    ],
    ["a@x, ;  ;", "a@x, ;  ;Alice Smith <alice@example.com>, "],
    ["a@x, ;  ;  ali", "a@x, ;  ;  Alice Smith <alice@example.com>, "],
    ["not-an-address ; ali", "not-an-address ; Alice Smith <alice@example.com>, "],
    ['a@x; "Doe, Al', "a@x; Alice Smith <alice@example.com>, "],
    ["a@x; Alice <al, foo", "a@x; Alice Smith <alice@example.com>, "],
    ["a@x; Al (comment, nested (semi;", "a@x; Alice Smith <alice@example.com>, "],
    ["a@x; al@[literal,semi;", "a@x; Alice Smith <alice@example.com>, "],
  ])("preserves the exact prefix in %s", (input, expected) => {
    expect(replaceLastRecipient(input, "Alice Smith", "alice@example.com"))
      .toBe(expected);
  });

  it("uses the shared display-name formatter", () => {
    expect(replaceLastRecipient("j", 'Doe, "Jane" \\ Ops', "jane@example.com"))
      .toBe(String.raw`"Doe, \"Jane\" \\ Ops" <jane@example.com>, `);
  });

  it("inserts a bare email when the name is empty or equals the email", () => {
    expect(replaceLastRecipient("j", "", "jane@example.com"))
      .toBe("jane@example.com, ");
    expect(replaceLastRecipient("j", "jane@example.com", "jane@example.com"))
      .toBe("jane@example.com, ");
  });
});

describe("formatRecipient", () => {
  it("normalizes parsed-name tabs without altering the mailbox or source object", () => {
    const address = { name: "Alice\tSmith", email: '"a,b"@example.com' };
    expect(formatRecipient(address)).toBe('Alice Smith <"a,b"@example.com>');
    expect(parseRecipients(formatRecipient(address))).toEqual({
      ok: true, addresses: [address.email],
    });
    expect(address.name).toBe("Alice\tSmith");
    expect(formatRecipient({ name: "Alice", email: '"a\tb"@example.com' }))
      .toBe('Alice <"a\tb"@example.com>');
  });

  it("keeps a simple display name unquoted", () => {
    expect(formatRecipient({ name: "Alice Smith", email: "alice@example.com" }))
      .toBe("Alice Smith <alice@example.com>");
  });

  it.each([undefined, null, "", "bob@example.com"])(
    "formats a bare address for display name %s",
    (name) => {
      expect(formatRecipient({ name, email: "bob@example.com" }))
        .toBe("bob@example.com");
    },
  );

  it("accepts an omitted name", () => {
    expect(formatRecipient({ email: "bob@example.com" })).toBe("bob@example.com");
  });

  it("joins draft recipients with comma-space", () => {
    const addresses = [
      { name: "Alice", email: "alice@a.com" },
      { name: null, email: "bob@b.com" },
    ];
    expect(addresses.map(formatRecipient).join(", "))
      .toBe("Alice <alice@a.com>, bob@b.com");
  });

  it.each<[string, string]>([
    ["Doe, Jane", '"Doe, Jane" <jane@x>'],
    ["Team; West", '"Team; West" <jane@x>'],
    ["Team: West", '"Team: West" <jane@x>'],
    ["Jane (work)", '"Jane (work)" <jane@x>'],
    ["Jane <work>", '"Jane <work>" <jane@x>'],
    ["Jane [work]", '"Jane [work]" <jane@x>'],
    ["Jane@work", '"Jane@work" <jane@x>'],
    ["Dr. Jane", '"Dr. Jane" <jane@x>'],
    ['Jane "JJ"', String.raw`"Jane \"JJ\"" <jane@x>`],
    [String.raw`Jane\Ops`, String.raw`"Jane\\Ops" <jane@x>`],
    [" Jane ", '" Jane " <jane@x>'],
    ["Zoë 李", "Zoë 李 <jane@x>"],
    ["O'Connor", "O'Connor <jane@x>"],
  ])("formats %s for round-trip recipient parsing", (name, expected) => {
    const formatted = formatRecipient({ name, email: "jane@x" });
    expect(formatted).toBe(expected);
    expect(parseRecipients(formatted)).toEqual({ ok: true, addresses: ["jane@x"] });
  });

  it("round-trips special names and quoted Unicode local parts together", () => {
    const email = String.raw`"用,;@>戸\"\\"@[IPv6:2001:db8::1]`;
    const name = 'Team: "West", Ops; [on-call] (夜) \\';
    const formatted = formatRecipient({ name, email });
    expect(formatted.endsWith(`<${email}>`)).toBe(true);
    expect(parseRecipients(`${formatted}, next@y; `)).toEqual({
      ok: true,
      addresses: [email, "next@y"],
    });
    expect(parseRecipients(replaceLastRecipient("te", name, email))).toEqual({
      ok: true,
      addresses: [email],
    });
  });

  it("does not trim or rewrite the supplied raw email", () => {
    const email = '  "a,b"@例子.公司  ';
    expect(formatRecipient({ email })).toBe(email);
    expect(formatRecipient({ name: "Alice", email })).toBe(`Alice <${email}>`);
  });
});

describe("recipient input preservation", () => {
  it("does not mutate source text or the address used for formatting", () => {
    const input = '  "Doe, Jane" <jane@x> ;  "A, B" <ab@y>  ';
    const original = input;
    const address = Object.freeze({ name: 'Doe, "Jane"', email: "jane@x" });

    expect(parseRecipients(input)).toEqual({ ok: true, addresses: ["jane@x", "ab@y"] });
    expect(getLastRecipientTerm(input)).toBe('"A, B" <ab@y>');
    replaceLastRecipient(input, address.name, address.email);
    formatRecipient(address);

    expect(input).toBe(original);
    expect(address).toEqual({ name: 'Doe, "Jane"', email: "jane@x" });
    expect(parseRecipients(input)).toEqual({ ok: true, addresses: ["jane@x", "ab@y"] });
  });
});
