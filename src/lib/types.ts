export interface Account {
  id: string;
  display_name: string;
  email: string;
  // Carried in the summary so the settings list can show *something*
  // for standalone CalDAV / CardDAV accounts whose `email` was never
  // set (older accounts created before the DAV-tab email back-fill).
  username: string;
  provider: "generic" | "gmail" | "microsoft365" | "o365" | "fastmail";
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
  // Whether the account has a calendar / contacts binding *and*
  // that binding is currently enabled. The settings list keys off
  // these to label standalone CalDAV-only vs CardDAV-only accounts
  // (a CalDAV-tab account derives a disabled contacts binding;
  // existence-only flags would always say true for both).
  has_calendar_binding: boolean;
  has_contacts_binding: boolean;
  // #148. Protocol of the account's enabled meet binding —
  // `talk`, `matrix`, `zoom`, or whatever else is registered
  // in `meet::registry()` — empty when there is none. Lets the
  // calendar event editor populate its "Add video link"
  // dropdown without an extra round-trip per row.
  meet_protocol: "" | "talk" | "matrix" | "zoom";
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

// The account/folder the user was last viewing, persisted via
// `save_last_view` and restored on startup (#191). Either field is
// `null` on a fresh install or before the first navigation is saved.
export interface LastView {
  account_id: string | null;
  folder_path: string | null;
}

export interface Address {
  name: string | null;
  email: string;
}

export interface RoomSuggestion {
  name: string;
  address: string;
}

export interface RoomAvailability {
  state: "available" | "busy" | "unknown";
  busy_start: string | null;
  busy_end: string | null;
}

export interface ParticipantSchedule {
  email: string;
  /** False means the provider could not disclose this calendar's availability. */
  available: boolean;
  busy: Array<{ start: string; end: string }>;
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

export type PgpKind = "mimeEncrypted" | "mimeSigned" | "inlineArmor";

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
  /** Detected PGP shape — absent for plain mail. Drives the reader UI's
   *  Decrypt button / signature badge. */
  pgp_kind?: PgpKind;
}

export type PgpVerifyOutcome =
  | { kind: "unsigned" }
  | {
      kind: "good";
      signerUid: string | null;
      signerFingerprint: string;
      verifierFingerprint: string;
    }
  | { kind: "bad"; signerUid: string | null; signerFingerprint: string }
  | { kind: "unknownKey"; keyId: string }
  | { kind: "error"; message: string };

export interface PgpDecryptedMessage {
  plaintextBody: MessageBody;
  verifyOutcome: PgpVerifyOutcome;
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

export interface AccountConfig {
  display_name: string;
  email: string;
  provider: "generic" | "gmail" | "microsoft365" | "o365" | "fastmail";
  /// Empty string means "no mail binding" (CalDAV-only / CardDAV-only).
  mail_protocol: "" | "imap" | "jmap" | "graph";
  imap_host: string;
  imap_port: number;
  smtp_host: string;
  smtp_port: number;
  jmap_url: string;
  caldav_url: string;
  /// #148. Server URL the provider keys off. Nextcloud root for
  /// Talk, homeserver for Matrix. For Zoom this is a marker
  /// (`https://zoom.us`) since Zoom is a hosted service with no
  /// per-user URL — `create_url` reads the OAuth token from the
  /// keyring and ignores this. Empty when the account has no
  /// meet binding.
  meet_url: string;
  /// Provider discriminator chosen by the Settings tab the user
  /// signed in through. The set in this union is whatever the
  /// `meet::registry()` exposes today; empty when there's no
  /// meet binding.
  meet_protocol: "" | "talk" | "matrix" | "zoom";
  username: string;
  password: string;
  use_tls: boolean;
  signature: string;
  jmap_auth_method: "basic" | "bearer" | "oidc";
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
  /// Per-account OpenPGP "Advanced settings". All default `true`. The
  /// compose / draft backend reads these to govern feature behavior at
  /// send / save time.
  pgp_attach_pubkey_on_sign: boolean;
  pgp_autocrypt_header: boolean;
  pgp_encrypt_subject: boolean;
  pgp_encrypt_drafts: boolean;
}

/// IMAP / SMTP discovery result returned by `discoverMailServers`.
/// Mail-only — CalDAV / CardDAV are not probed here, those have
/// dedicated account types. Empty strings / zero ports mean "not
/// found"; the frontend only applies non-empty fields to the form.
/// `source` is informational: "isp-db" | "domain-autoconfig" |
/// "well-known" | "mx" | "host-probe" | "".
export interface AutoconfigResult {
  imap_host: string;
  imap_port: number;
  imap_use_tls: boolean;
  smtp_host: string;
  smtp_port: number;
  smtp_use_tls: boolean;
  source: string;
}

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
  field: "from" | "to" | "cc" | "to_cc" | "subject" | "size" | "has_attachment";
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
  is_self?: boolean;
}

/**
 * A calendar invite returned by `list_invites` — a CalendarEvent the
 * account was invited to (attendee, not organizer), plus the row's
 * `created_at` arrival timestamp used for the "recently received" sort.
 */
export interface Invite extends CalendarEvent {
  manually_managed_at: string | null;
  created_at: string | null;
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
  /** Optional meet binding to persist with the event so later
   * reschedule / cancel calls can act on the right remote meeting.
   * Comes from `meetCreateUrl` when the user adds a video link. */
  meet_binding?: MeetBinding | null;
}

/** Provider-agnostic handle for a remote meeting Chithi created on
 * behalf of an event. Returned by `meetCreateUrl` and accepted by
 * `createEvent` / `updateEvent` so the backend can persist the
 * link in `meet_meetings`. */
export interface MeetBinding {
  lifecycle_id: string;
  account_id: string;
  protocol: string;
  meeting_id: string;
  join_url: string;
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
  /** OpenPGP toggles. Default false. */
  pgp_sign?: boolean;
  pgp_encrypt?: boolean;
  /** "Compose in Markdown": when true the backend treats `body_text` as
   *  Markdown, rendering it to safe HTML and sending both parts. Ignored
   *  when `body_html` is already set. Default false. */
  markdown?: boolean;
}

export interface PgpRecipientStatus {
  email: string;
  hasKey: boolean;
  fingerprint: string | null;
}

/** Result of `save_draft`. Lets the composer tell the user when the
 *  "Store drafts encrypted" toggle could not be honored. */
export interface DraftSaveOutcome {
  /** True when encryption was requested for this account but the draft
   *  was stored in plaintext anyway (Microsoft Graph account, or no
   *  usable public key in the keystore). */
  plaintext_fallback: boolean;
}

export interface ComposeAttachment {
  token: string;
  name: string;
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

// --- Outbox ---

/// Synthetic folder path used by the FolderTree to flag the per-account
/// Outbox node. Picked to be invalid as a real IMAP/JMAP mailbox name so
/// it can never collide with a server-side folder.
export const OUTBOX_FOLDER = "__chithi_outbox__";

export interface OutboxRow {
  id: number;
  account_id: string;
  action_type: string;
  status: "pending" | "sending" | "dead";
  delivery_outcome_unknown: boolean;
  retry_count: number;
  error_message: string | null;
  subject: string | null;
  to: string[];
  cc: string[];
  bcc: string[];
}

// --- OpenPGP ---

export interface PgpUserId {
  uid: string;
  email: string | null;
}

export interface PgpSubkey {
  fingerprint: string;
  keyId: string;
  /** "signing" | "encryption" | "authentication" | "certification" | "unknown" */
  keyType: string;
  algorithm: string | null;
  bitLength: number | null;
}

export interface PgpKeySummary {
  fingerprint: string;
  isSecret: boolean;
  primaryUid: string | null;
  userIds: PgpUserId[];
  subkeys: PgpSubkey[];
  /** ISO-8601 UTC timestamp or null */
  creationTime: string | null;
  expirationTime: string | null;
  isRevoked: boolean;
  revocationTime: string | null;
  /** Card idents this key is linked to (may repeat per slot). */
  cardIdents: string[];
}

export interface PgpCardSummary {
  ident: string;
  manufacturerName: string;
  serialNumber: string;
  cardholderName: string | null;
}

export interface PgpCardDetails {
  ident: string;
  serialNumber: string;
  cardholderName: string | null;
  manufacturerName: string | null;
  publicKeyUrl: string | null;
  signatureFingerprint: string | null;
  encryptionFingerprint: string | null;
  authenticationFingerprint: string | null;
  signatureCounter: number;
  pinRetryCounter: number;
  resetCodeRetryCounter: number;
  adminPinRetryCounter: number;
}

export interface PgpCardDetection {
  keyFingerprint: string;
  cardIdent: string;
  /** "signature" | "encryption" | "authentication" */
  slot: string;
  slotFingerprint: string;
}

export interface PgpImportResult {
  fingerprint: string;
  isSecret: boolean;
}
