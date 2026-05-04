export interface Account {
  id: string;
  display_name: string;
  email: string;
  provider: "generic" | "gmail" | "microsoft365" | "o365";
  // Empty string means "no mail binding" — calendar-only / contacts-only
  // accounts (#43). Existing screens that need a mail account should
  // filter these out.
  mail_protocol: "" | "imap" | "jmap" | "graph";
  enabled: boolean;
  // Phase 4 (#43): per-binding sync intervals exposed on the summary
  // so periodic-sync timers can pick the right cadence per account
  // without a separate get_account_config call.
  mail_sync_interval_seconds: number | null;
  calendar_sync_interval_seconds: number | null;
  contacts_sync_interval_seconds: number | null;
}

export interface QuickFilter {
  unread?: boolean;
  starred?: boolean;
  has_attachment?: boolean;
  contact?: boolean;
  text?: string;
  text_fields?: string[];
}

export interface SearchFields {
  subject: boolean;
  from: boolean;
  to: boolean;
  body: boolean;
}

export interface SearchQuery {
  text: string;
  fields: SearchFields;
  has_attachment?: boolean;
  since_days?: number;
}

export interface SearchHit {
  account_id: string;
  folder_path: string;
  uid: number | null;
  message_id: string | null;
  backend_id: string;
  subject: string;
  from_name: string | null;
  from_email: string | null;
  date: number;
  snippet: string | null;
}

export interface Folder {
  name: string;
  path: string;
  folder_type: string | null;
  unread_count: number;
  total_count: number;
  children: Folder[];
}

export interface Address {
  name: string | null;
  email: string;
}

export interface MessageSummary {
  id: string;
  subject: string | null;
  from_name: string | null;
  from_email: string;
  date: string;
  flags: string[];
  has_attachments: boolean;
  is_encrypted: boolean;
  is_signed: boolean;
  snippet: string | null;
  /** RFC 5322 Message-ID with angle brackets, used to build reply trees. */
  message_id: string | null;
  /** Parent Message-ID for in-thread hierarchical rendering. */
  in_reply_to: string | null;
}

export interface MessageBody {
  id: string;
  subject: string | null;
  from: Address;
  to: Address[];
  cc: Address[];
  date: string;
  flags: string[];
  body_html: string | null;
  body_text: string | null;
  attachments: Attachment[];
  is_encrypted: boolean;
  is_signed: boolean;
  list_id: string | null;
  has_remote_images: boolean;
}

export interface Attachment {
  index: number;
  filename: string | null;
  content_type: string;
  size: number;
}

export interface MessagePage {
  messages: MessageSummary[];
  total: number;
  page: number;
  per_page: number;
}

export interface ThreadSummary {
  thread_id: string;
  subject: string | null;
  last_date: string;
  message_count: number;
  unread_count: number;
  from_name: string | null;
  from_email: string;
  has_attachments: boolean;
  flags: string[];
  snippet: string | null;
  message_ids: string[];
}

export interface ThreadedPage {
  threads: ThreadSummary[];
  total_threads: number;
  total_messages: number;
  page: number;
  per_page: number;
}

export interface SyncStatus {
  account_id: string;
  is_syncing: boolean;
  last_sync: string | null;
  error: string | null;
}

export interface AccountConfig {
  display_name: string;
  email: string;
  provider: "generic" | "gmail" | "microsoft365" | "o365";
  /// Empty string means "no mail binding" (CalDAV-only / CardDAV-only).
  mail_protocol: "" | "imap" | "jmap" | "graph";
  imap_host: string;
  imap_port: number;
  smtp_host: string;
  smtp_port: number;
  jmap_url: string;
  caldav_url: string;
  username: string;
  password: string;
  use_tls: boolean;
  signature: string;
  jmap_auth_method: "basic" | "oidc";
  oidc_token_endpoint: string;
  oidc_client_id: string;
  /// Whether the calendar binding is enabled. Mirrors mail_sync_enabled
  /// for the calendar service.
  calendar_sync_enabled: boolean;
  /// Phase 4 (#43): per-binding enabled flags + sync intervals.
  mail_sync_enabled: boolean;
  contacts_sync_enabled: boolean;
  /// Override the default sync cadence (in seconds). `null` keeps the
  /// service's default (mail handled by IDLE/push, calendar 5 min,
  /// contacts 30 min).
  mail_sync_interval_seconds: number | null;
  calendar_sync_interval_seconds: number | null;
  contacts_sync_interval_seconds: number | null;
  /// Whether a calendar / contacts binding actually exists for this
  /// account (regardless of its enabled state). Used by the Settings
  /// edit form to disambiguate standalone CalDAV-only vs CardDAV-only
  /// accounts even when the user has unchecked the matching Sync flag.
  /// Backend-populated; treated as read-only in form state.
  has_calendar_binding: boolean;
  has_contacts_binding: boolean;
}

/// Combined result of Thunderbird-style autoconfig + DAV probing.
/// Empty strings / zero ports mean "not found"; the frontend only
/// applies non-empty fields to the form. `source` is informational:
/// "isp-db" | "domain-autoconfig" | "well-known" | "mx" | "".
export interface AutoconfigResult {
  imap_host: string;
  imap_port: number;
  imap_use_tls: boolean;
  smtp_host: string;
  smtp_port: number;
  smtp_use_tls: boolean;
  caldav_url: string;
  carddav_url: string;
  source: string;
}

// Old name kept as an alias so existing imports compile while we
// migrate. New callers should use AutoconfigResult.
export type DavProbeResult = AutoconfigResult;

export interface FilterRule {
  id: string;
  account_id: string | null;
  name: string;
  enabled: boolean;
  priority: number;
  match_type: "all" | "any";
  conditions: FilterCondition[];
  actions: FilterAction[];
  stop_processing: boolean;
}

export interface FilterCondition {
  field: "from" | "to" | "cc" | "subject" | "size" | "has_attachment";
  op:
    | "contains"
    | "not_contains"
    | "equals"
    | "not_equals"
    | "matches_regex"
    | "greater_than"
    | "less_than";
  value: string;
}

export type FilterAction =
  | { action: "move"; target: string }
  | { action: "copy"; target: string }
  | { action: "delete" }
  | { action: "flag"; value: string }
  | { action: "unflag"; value: string }
  | { action: "mark_read" }
  | { action: "mark_unread" }
  | { action: "stop" };

// Calendar types
export interface Calendar {
  id: string;
  account_id: string;
  name: string;
  color: string;
  is_default: boolean;
  remote_id: string | null;
  is_subscribed: boolean;
}

export interface CalendarEvent {
  id: string;
  account_id: string;
  calendar_id: string;
  uid: string | null;
  title: string;
  description: string | null;
  location: string | null;
  start_time: string;
  end_time: string;
  all_day: boolean;
  timezone: string | null;
  recurrence_rule: string | null;
  organizer_email: string | null;
  attendees_json: string | null;
  my_status: string | null;
  source_message_id: string | null;
}

export interface Attendee {
  email: string;
  name: string | null;
  status: string;
}

export interface ParsedInvite {
  method: string;
  uid: string;
  summary: string | null;
  description: string | null;
  location: string | null;
  dtstart: string;
  dtend: string;
  all_day: boolean;
  timezone: string | null;
  organizer_email: string | null;
  organizer_name: string | null;
  attendees: Attendee[];
  recurrence_rule: string | null;
  sequence: number;
}

export interface NewEventInput {
  account_id: string;
  calendar_id: string;
  title: string;
  description: string | null;
  location: string | null;
  start_time: string;
  end_time: string;
  all_day: boolean;
  timezone: string | null;
  recurrence_rule: string | null;
  attendees: Attendee[];
}

export interface ComposeMessage {
  to: string[];
  cc: string[];
  bcc: string[];
  subject: string;
  body_text: string;
  body_html: string | null;
  attachments: ComposeAttachment[];
  /** chithi's internal id of the message being replied to. Drives
   *  In-Reply-To / References on the outgoing email. Omit for new mails. */
  reply_to_message_id?: string | null;
}

export interface ComposeAttachment {
  token: string;
  name: string;
  size?: number;
}

// Contacts types
export interface ContactBook {
  id: string;
  account_id: string;
  name: string;
  remote_id: string | null;
  sync_type: string;
}

export interface Contact {
  id: string;
  book_id: string;
  uid: string | null;
  display_name: string;
  emails_json: string;
  phones_json: string;
  addresses_json: string;
  organization: string | null;
  title: string | null;
  notes: string | null;
  vcard_data: string | null;
  remote_id: string | null;
  etag: string | null;
}

export interface CollectedContact {
  id: number;
  account_id: string;
  email: string;
  name: string | null;
  last_used: string;
  use_count: number;
}

// --- Operation status types (for sync architecture) ---

export interface FailedOp {
  account_id: string;
  op_type: string;
  error: string;
  timestamp: number;
}

export interface OfflineQueueChange {
  account_id: string;
  dead_op_id: number;
  action_type: string;
}
