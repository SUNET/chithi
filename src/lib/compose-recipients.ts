export interface RecipientParseError {
  /** One-based ordinal among nonempty recipients, not separator fields. */
  index: number;
  message: string;
}

export type RecipientParseResult =
  | { ok: true; addresses: string[] }
  | { ok: false; error: RecipientParseError };

type PartKind = "text" | "quoted" | "literal" | "<" | ">" | "@" | ":";

interface RecipientPart {
  kind: PartKind;
  start: number;
  end: number;
}

interface RecipientSpan {
  start: number;
  end: number;
  parts: RecipientPart[];
  error?: string;
}

type MailboxResult =
  | { ok: true; address: string; localPart: string; domainPart: string }
  | { ok: false; message: string };

const controlCharacters = /[\u0000-\u001f\u007f-\u009f]/u;
const missingSeparator = "Separate recipients with a comma or semicolon.";

function isRecipientPadding(char: string): boolean {
  return char === " " || char === "\t";
}

/** Strip ASCII SP/HTAB only; Unicode whitespace is significant mailbox content. */
export function trimRecipientPadding(value: string): string {
  let start = 0;
  let end = value.length;
  while (start < end && isRecipientPadding(value[start])) start++;
  while (end > start && isRecipientPadding(value[end - 1])) end--;
  return value.slice(start, end);
}

/** Keep raw offsets even for unfinished syntax so autocomplete can use it. */
function tokenizeRecipients(input: string): RecipientSpan[] {
  const spans: RecipientSpan[] = [];
  let start = 0;
  let parts: RecipientPart[] = [];
  let error: string | undefined;
  let angleDepth = 0;
  let seenAngle = false;
  let cursor = 0;

  function finish(end: number): void {
    spans.push({
      start,
      end,
      parts,
      error: controlCharacters.test(input.slice(start, end))
        ? "Control characters are not allowed in recipients."
        : error,
    });
    start = end + 1;
    parts = [];
    error = undefined;
    angleDepth = 0;
    seenAngle = false;
  }

  while (cursor < input.length) {
    const char = input[cursor];
    if (isRecipientPadding(char)) {
      cursor++;
      continue;
    }

    if (char === "," || char === ";") {
      if (angleDepth === 0) {
        finish(cursor);
      } else {
        error ??= "Unexpected separator inside angle brackets.";
      }
      cursor++;
      continue;
    }

    if (char === '"' || char === "(" || char === "[") {
      const protectedStart = cursor++;
      const closing = char === "(" ? ")" : char === "[" ? "]" : '"';
      let depth = 1;
      let escaped = false;

      while (cursor < input.length && depth > 0) {
        const inner = input[cursor++];
        if (escaped) {
          escaped = false;
        } else if (inner === "\\") {
          escaped = true;
        } else if (char === "(" && inner === "(") {
          depth++;
        } else if (inner === closing) {
          depth--;
        } else if (char === "[" && inner === "[") {
          error ??= "Unexpected opening bracket inside a domain literal.";
        }
      }

      if (depth > 0) {
        const label = char === "(" ? "comment" : char === "[" ? "domain literal" : "quote";
        error ??= `Unclosed ${label}.`;
      }
      if (char !== "(") {
        parts.push({
          kind: char === '"' ? "quoted" : "literal",
          start: protectedStart,
          end: cursor,
        });
      }
      continue;
    }

    if (char === "<" || char === ">" || char === "@" || char === ":") {
      if (char === "<") {
        if (seenAngle) {
          error ??= `Extra angle bracket. ${missingSeparator}`;
        }
        seenAngle = true;
        angleDepth++;
      } else if (char === ">") {
        if (angleDepth === 0) {
          error ??= "Unexpected closing angle bracket.";
        } else {
          angleDepth--;
        }
      }
      parts.push({ kind: char, start: cursor, end: cursor + 1 });
    } else if (char === ")" || char === "]") {
      error ??= char === ")"
        ? "Unexpected closing comment parenthesis."
        : "Unexpected closing domain literal bracket.";
    } else if (char === "\\") {
      error ??= "Backslash escapes require quotes, a comment, or a domain literal.";
    } else {
      const previous = parts[parts.length - 1];
      if (previous?.kind === "text" && previous.end === cursor) {
        previous.end++;
      } else {
        parts.push({ kind: "text", start: cursor, end: cursor + 1 });
      }
    }
    cursor++;
  }

  if (angleDepth > 0) error ??= "Unclosed angle bracket.";
  finish(input.length);
  return spans;
}

/** Remove edge comments/ASCII padding, never join separate word fragments. */
function readAddrSpec(input: string, parts: RecipientPart[]): MailboxResult {
  const atSigns = parts.filter((part) => part.kind === "@");
  if (atSigns.length === 0) {
    return { ok: false, message: "Missing '@' in recipient address." };
  }
  if (atSigns.length > 1) {
    return {
      ok: false,
      message: `Multiple unquoted '@' signs. ${missingSeparator}`,
    };
  }

  const at = parts.indexOf(atSigns[0]);
  const local = parts.slice(0, at);
  const domain = parts.slice(at + 1);
  if (local.length === 0) {
    return { ok: false, message: "Missing local part before '@'." };
  }
  if (domain.length === 0) {
    return { ok: false, message: "Missing domain after '@'." };
  }
  if (local.length !== 1 || domain.length !== 1) {
    return {
      ok: false,
      message: `Unexpected whitespace or word fragments in address. ${missingSeparator}`,
    };
  }
  if (local[0].kind !== "text" && local[0].kind !== "quoted") {
    return { ok: false, message: "Invalid local part before '@'." };
  }
  if (domain[0].kind !== "text" && domain[0].kind !== "literal") {
    return { ok: false, message: "Invalid domain after '@'." };
  }

  const localPart = input.slice(local[0].start, local[0].end);
  const domainPart = input.slice(domain[0].start, domain[0].end);
  if (domainPart === "[]") {
    return { ok: false, message: "Empty domain literal." };
  }
  return { ok: true, address: `${localPart}@${domainPart}`, localPart, domainPart };
}

function readMailbox(input: string, parts: RecipientPart[]): MailboxResult {
  if (parts.some((part) => part.kind === ":")) {
    return { ok: false, message: "Recipient groups are not supported." };
  }

  const open = parts.findIndex((part) => part.kind === "<");
  if (open === -1) return readAddrSpec(input, parts);

  const close = parts.findIndex((part) => part.kind === ">");
  if (close !== parts.length - 1) {
    return {
      ok: false,
      message: `Unexpected text after '>'. ${missingSeparator}`,
    };
  }
  if (close === open + 1) {
    return { ok: false, message: "Empty angle-bracket address." };
  }

  const mailbox = readAddrSpec(input, parts.slice(open + 1, close));
  if (!mailbox.ok) return mailbox;

  const display = parts.slice(0, open);
  if (display.some((part) => part.kind === "@")) {
    // An unquoted email display name must repeat this exact mailbox.
    const legacyDisplay = readAddrSpec(input, display);
    if (!legacyDisplay.ok || legacyDisplay.address !== mailbox.address) {
      return {
        ok: false,
        message: `Unexpected address in display name. ${missingSeparator}`,
      };
    }
  } else if (display.some((part) => part.kind !== "text" && part.kind !== "quoted")) {
    return {
      ok: false,
      message: "Invalid display name; quote names containing special characters.",
    };
  }
  return mailbox;
}

/** Reject raw controls and check list structure; the backend validates mailboxes. */
export function parseRecipients(input: string): RecipientParseResult {
  const addresses: string[] = [];
  let index = 0;

  for (const span of tokenizeRecipients(input)) {
    if (!span.error && trimRecipientPadding(input.slice(span.start, span.end)) === "") continue;
    index++;
    if (span.error) {
      return { ok: false, error: { index, message: span.error } };
    }
    const mailbox = readMailbox(input, span.parts);
    if (!mailbox.ok) {
      return { ok: false, error: { index, message: mailbox.message } };
    }
    addresses.push(mailbox.address);
  }

  return { ok: true, addresses };
}

export function getLastRecipientTerm(input: string): string {
  const spans = tokenizeRecipients(input);
  const last = spans[spans.length - 1];
  return trimRecipientPadding(input.slice(last.start, last.end));
}

export interface RecipientSearch {
  query: string;
  kind: "name" | "address";
}

function bareAddress(value: string): Extract<MailboxResult, { ok: true }> | null {
  const spans = tokenizeRecipients(value);
  if (spans.length !== 1 || spans[0].error) return null;
  const parsed = readAddrSpec(value, spans[0].parts);
  return parsed.ok && parsed.address === trimRecipientPadding(value) ? parsed : null;
}

/** Match domain-case variants without merging case-distinct local parts. */
export function recipientDeduplicationKey(email: string): string {
  const mailbox = bareAddress(email);
  return mailbox
    ? JSON.stringify(["mailbox", mailbox.localPart, mailbox.domainPart.toLowerCase()])
    : JSON.stringify(["raw", email]);
}

/** Domain case is insignificant; local-part spelling must not be changed. */
export function rankRecipientAddressMatch(email: string, query: string): number {
  if (email === query) return 0;
  const candidate = bareAddress(email);
  const typed = bareAddress(query);
  if (candidate && typed) {
    const domain = candidate.domainPart.toLowerCase();
    const typedDomain = typed.domainPart.toLowerCase();
    if (candidate.localPart === typed.localPart) {
      if (domain === typedDomain) return 1;
      if (domain.startsWith(typedDomain)) return 2;
    }
    if (`${candidate.localPart}@${domain}`.includes(`${typed.localPart}@${typedDomain}`)) {
      return 3;
    }
  }
  return email.includes(query) ? 3 : 4;
}

/** Contact search consumes names/addresses, not display-name quoting syntax. */
export function getRecipientSearch(input: string): RecipientSearch {
  const raw = getLastRecipientTerm(input);
  const { parts } = tokenizeRecipients(raw)[0];
  const open = parts.find((part) => part.kind === "<");
  if (open) {
    const close = parts.find((part) => part.kind === ">");
    return {
      query: trimRecipientPadding(raw.slice(open.end, close?.start ?? raw.length)),
      kind: "address",
    };
  }
  // Quotes in a bare addr-spec are part of the stored mailbox spelling.
  if (parts.some((part) => part.kind === "@")) return { query: raw, kind: "address" };
  const query = parts.map((part) => {
    const text = raw.slice(part.start, part.end);
    if (part.kind !== "quoted") return text;
    let decoded = "";
    let escaped = false;
    for (const char of text.slice(1)) {
      if (escaped) {
        decoded += char;
        escaped = false;
      } else if (char === "\\") {
        escaped = true;
      } else if (char === '"') {
        break;
      } else {
        decoded += char;
      }
    }
    return decoded;
  }).join(" ").trim();
  return { query, kind: "name" };
}

export function replaceLastRecipient(input: string, name: string, email: string): string {
  const spans = tokenizeRecipients(input);
  const last = spans[spans.length - 1];
  let contentStart = last.start;
  while (contentStart < last.end && isRecipientPadding(input[contentStart])) contentStart++;
  return input.slice(0, contentStart) + formatRecipient({ name, email }) + ", ";
}

export function formatRecipient(address: { name?: string | null; email: string }): string {
  const { email } = address;
  // Header unfolding can retain HTAB in a parsed display name. Normalize
  // presentation whitespace only, never the mailbox itself or typed input.
  const name = address.name?.replace(/\t/g, " ");
  if (!name || name === email) return email;

  const needsQuotes = /[()[\]<>:;@\\,."]/u.test(name)
    || controlCharacters.test(name)
    || name !== name.trim();
  const display = needsQuotes ? `"${name.replace(/["\\]/g, "\\$&")}"` : name;
  return `${display} <${email}>`;
}
