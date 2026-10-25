export const ADD_ACCOUNT_TYPES = [
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
  "visio",
] as const;

export type AccountType = (typeof ADD_ACCOUNT_TYPES)[number];

export type BookOption = { id: string; label: string };

const ACCOUNT_TYPE_DETAILS = {
  gmail: {
    label: "Gmail",
    description: "Mail, calendar and contacts via Google",
  },
  o365: {
    label: "Microsoft 365",
    description: "Mail, calendar and contacts via Microsoft 365",
  },
  fastmail: {
    label: "Fastmail",
    description: "Fastmail mail, calendar and contacts via JMAP API token",
  },
  imap: {
    label: "IMAP",
    description: "Generic IMAP / SMTP mail account",
  },
  jmap: {
    label: "JMAP",
    description: "JMAP mail (Stalwart, generic JMAP servers)",
  },
  caldav: {
    label: "CALDAV",
    description: "Standalone calendar via CalDAV",
  },
  carddav: {
    label: "CARDDAV",
    description: "Standalone contacts via CardDAV",
  },
  talk: {
    label: "Nextcloud Talk",
    description: "Video conferencing on a Nextcloud server",
  },
  matrix: {
    label: "Matrix",
    description: "Video conferencing via Matrix / Element Call",
  },
  zoom: {
    label: "Zoom",
    description: "Video conferencing via Zoom",
  },
  visio: {
    label: "La Suite Visio",
    description: "Video conferencing via a La Suite Visio instance",
  },
} satisfies Record<AccountType, { label: string; description: string }>;

/** Matches backend validation by requiring HTTPS and the exact Fastmail API host. */
export function isFastmailJmapUrl(url: string): boolean {
  try {
    const parsed = new URL(url);
    return (
      parsed.protocol === "https:" &&
      parsed.hostname.toLowerCase() === "api.fastmail.com"
    );
  } catch {
    return false;
  }
}

export function accountTypeLabelLong(accountType: AccountType): string {
  return ACCOUNT_TYPE_DETAILS[accountType].label;
}

export function accountTypeLabel(account: {
  provider?: string;
  mail_protocol?: string;
  has_calendar_binding?: boolean;
  has_contacts_binding?: boolean;
  meet_protocol?: string;
}): string {
  if (
    account.provider === "gmail" ||
    account.provider === "o365" ||
    account.provider === "fastmail"
  ) {
    return ACCOUNT_TYPE_DETAILS[account.provider].label.toUpperCase();
  }
  if (account.mail_protocol) return account.mail_protocol.toUpperCase();

  const hasCalendar = Boolean(account.has_calendar_binding);
  const hasContacts = Boolean(account.has_contacts_binding);
  if (hasCalendar && hasContacts) return "Calendar and Contacts";
  if (hasCalendar) return "Calendar";
  if (hasContacts) return "Contacts";

  switch (account.meet_protocol) {
    case "talk":
    case "matrix":
    case "zoom":
    case "visio":
      return ACCOUNT_TYPE_DETAILS[account.meet_protocol].label;
    default:
      return "";
  }
}

export function accountSecondaryLabel(account: {
  email: string;
  username: string;
}): string {
  return account.email || account.username;
}

export function accountTypeDescription(accountType: AccountType): string {
  return ACCOUNT_TYPE_DETAILS[accountType].description;
}
