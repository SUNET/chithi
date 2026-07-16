/// Account-type vocabulary shared by the settings view, the
/// add-account picker and the per-type form sections.

export type AccountType =
  | "gmail"
  | "imap"
  | "jmap"
  | "fastmail"
  | "caldav"
  | "carddav"
  | "o365"
  | "talk"
  | "matrix"
  | "zoom";

/// Cross-account contact-book option for the default-book dropdowns.
/// The label is "Account / Book" so the same book name on two
/// different accounts (e.g. "Personal") stays distinguishable.
export type BookOption = { id: string; label: string };

/// Order of the cards in the add-account picker dialog.
export const ADD_ACCOUNT_TYPES: AccountType[] = [
  "gmail",
  "o365",
  "fastmail",
  "imap",
  "jmap",
  "caldav",
  "carddav",
  "talk",
  "matrix",
  "zoom",
];

/// Strict Fastmail JMAP endpoint check. Mirrors the Rust
/// `is_fastmail_jmap_url` helper in `db/accounts.rs`: returns
/// `true` only when the URL parses, uses https, and its hostname
/// is *exactly* `api.fastmail.com` (case-insensitive). A plain
/// `startsWith("https://api.fastmail.com")` would also approve
/// lookalike hosts like `api.fastmail.com.attacker.example`.
export function isFastmailJmapUrl(u: string): boolean {
  try {
    const parsed = new URL(u);
    return (
      parsed.protocol === "https:"
      && parsed.hostname.toLowerCase() === "api.fastmail.com"
    );
  } catch {
    return false;
  }
}

/// Long-form label for the type-selector buttons in the modal.
/// Mostly the same as `accountTypeLabel` for the listing, but the
/// modal is wider and benefits from "Nextcloud Talk" and "Matrix"
/// spelled out instead of an upper-case acronym.
export function accountTypeLabelLong(t: AccountType): string {
  switch (t) {
    case "gmail":
      return "Gmail";
    case "o365":
      return "Microsoft 365";
    case "fastmail":
      return "Fastmail";
    case "talk":
      return "Nextcloud Talk";
    case "matrix":
      return "Matrix";
    case "zoom":
      return "Zoom";
    default:
      return t.toUpperCase();
  }
}

export function accountTypeLabel(acc: {
  provider?: string;
  mail_protocol?: string;
  has_calendar_binding?: boolean;
  has_contacts_binding?: boolean;
  meet_protocol?: string;
}): string {
  if (acc.provider === "gmail") return "GMAIL";
  if (acc.provider === "o365") return "MICROSOFT 365";
  if (acc.provider === "fastmail") return "FASTMAIL";
  if (acc.mail_protocol) return acc.mail_protocol.toUpperCase();
  // Standalone DAV accounts: name them by the user-visible service
  // they provide rather than the protocol acronym. "Calendar" and
  // "Contacts" mean something to a user; "CALDAV" and "CARDDAV"
  // mean something to a protocol nerd.
  const hasCal = !!acc.has_calendar_binding;
  const hasCon = !!acc.has_contacts_binding;
  if (hasCal && hasCon) return "Calendar and Contacts";
  if (hasCal) return "Calendar";
  if (hasCon) return "Contacts";
  // Meet-only accounts (#148). Same user-visible naming approach.
  switch (acc.meet_protocol) {
    case "talk":
      return "Nextcloud Talk";
    case "matrix":
      return "Matrix";
    case "zoom":
      return "Zoom";
    default:
      return "";
  }
}

/// Secondary line for the settings account list. Standalone CalDAV /
/// CardDAV accounts created before the email back-fill landed have
/// `email = ""`; falling back to `username` means they still show
/// something identifying instead of a blank gap.
export function accountSecondaryLabel(acc: { email: string; username: string }): string {
  return acc.email || acc.username || "";
}

/// Brief subtitle shown under each card in the picker dialog.
/// Kept terse — the card's title already says what the type is.
export function accountTypeDescription(t: AccountType): string {
  switch (t) {
    case "gmail":
      return "Mail, calendar and contacts via Google";
    case "o365":
      return "Mail, calendar and contacts via Microsoft 365";
    case "imap":
      return "Generic IMAP / SMTP mail account";
    case "jmap":
      return "JMAP mail (Stalwart, generic JMAP servers)";
    case "fastmail":
      return "Fastmail mail, calendar and contacts via JMAP API token";
    case "caldav":
      return "Standalone calendar via CalDAV";
    case "carddav":
      return "Standalone contacts via CardDAV";
    case "talk":
      return "Video conferencing on a Nextcloud server";
    case "matrix":
      return "Video conferencing via Matrix / Element Call";
    case "zoom":
      return "Video conferencing via Zoom";
  }
}
