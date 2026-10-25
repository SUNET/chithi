export type AccountProvider =
  | "generic"
  | "gmail"
  | "microsoft365"
  | "o365"
  | "fastmail";

export type MailProtocol = "" | "imap" | "jmap" | "graph";
export type MeetProtocol = "" | "talk" | "matrix" | "zoom" | "visio";

/** Null leaves the service's default sync cadence unchanged. */
interface SyncIntervals {
  mail_sync_interval_seconds: number | null;
  calendar_sync_interval_seconds: number | null;
  contacts_sync_interval_seconds: number | null;
}

export interface Account extends SyncIntervals {
  id: string;
  display_name: string;
  email: string;
  username: string;
  provider: AccountProvider;
  /** Empty means the account has no mail binding. */
  mail_protocol: MailProtocol;
  enabled: boolean;
  /** True only when the corresponding binding exists and is enabled. */
  has_calendar_binding: boolean;
  has_contacts_binding: boolean;
  /** Empty means the account has no enabled meeting binding. */
  meet_protocol: MeetProtocol;
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

export interface LastView {
  account_id: string | null;
  folder_path: string | null;
}

interface Address {
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

type PgpKind = "mimeEncrypted" | "mimeSigned" | "inlineArmor";

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
  /** Detected PGP shape; absent for plain mail. */
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

export interface AccountConfig extends SyncIntervals {
  display_name: string;
  sender_name: string;
  email: string;
  provider: AccountProvider;
  /** Empty means the account has no mail binding. */
  mail_protocol: MailProtocol;
  imap_host: string;
  imap_port: number;
  smtp_host: string;
  smtp_port: number;
  jmap_url: string;
  caldav_url: string;
  /** Provider server URL; hosted Zoom accounts use a marker URL. */
  meet_url: string;
  meet_protocol: MeetProtocol;
  username: string;
  password: string;
  use_tls: boolean;
  signature: string;
  jmap_auth_method: "basic" | "bearer" | "oidc";
  oidc_token_endpoint: string;
  oidc_client_id: string;
  calendar_sync_enabled: boolean;
  mail_sync_enabled: boolean;
  contacts_sync_enabled: boolean;
  /** Binding existence, independent of the enabled flags above. */
  has_calendar_binding: boolean;
  has_contacts_binding: boolean;
  /** Per-account OpenPGP settings; all default to true. */
  pgp_attach_pubkey_on_sign: boolean;
  pgp_autocrypt_header: boolean;
  pgp_encrypt_subject: boolean;
  pgp_encrypt_drafts: boolean;
}

export interface AutoconfigResult {
  imap_host: string;
  imap_port: number;
  imap_use_tls: boolean;
  smtp_host: string;
  smtp_port: number;
  smtp_use_tls: boolean;
  source:
    | ""
    | "isp-db"
    | "domain-autoconfig"
    | "well-known"
    | "mx"
    | "host-probe";
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

interface FilterCondition {
  field:
    | "from"
    | "to"
    | "cc"
    | "to_cc"
    | "subject"
    | "size"
    | "has_attachment";
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

export interface Calendar {
  id: string;
  account_id: string;
  name: string;
  color: string;
  is_default: boolean;
  remote_id: string | null;
  is_subscribed: boolean;
}

export interface ArchivedGraphCalendar {
  id: string;
  account_id: string;
  name: string;
  retained_event_count: number;
  replay_address_count: number;
  acknowledged: boolean;
}

export type RecurrenceKind = "unknown" | "standalone" | "series" | "occurrence";

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
  recurrence_kind: RecurrenceKind;
  organizer_email: string | null;
  attendees_json: string | null;
  my_status: string | null;
  source_message_id: string | null;
}

interface CalendarSelection {
  event_id: string;
  token: string;
  original_start: string | null;
}

export interface CalendarOccurrenceFields {
  title: string;
  description: string | null;
  location: string | null;
  start_time: string;
  end_time: string;
  all_day: boolean;
  timezone: string | null;
}

export interface CalendarOccurrence {
  selection: CalendarSelection;
  event_id: string;
  account_id: string;
  calendar_id: string;
  fields: CalendarOccurrenceFields;
  recurrence_kind: RecurrenceKind;
  recurrence_rule: string | null;
  is_exception: boolean;
}

export interface CalendarOccurrencePage {
  occurrences: CalendarOccurrence[];
  has_more: boolean;
  needs_hydration: string[];
  /** Unverified rows that must not be projected or edited as occurrences. */
  unresolved: { event_id: string; calendar_id: string; account_id: string }[];
  /** Per-resource failures that do not invalidate other occurrences. */
  display_errors?: Array<{
    event_id: string;
    calendar_id: string;
    account_id: string;
    message: string;
  }>;
}

export interface CalendarEventSetView {
  master: CalendarOccurrence;
  page: CalendarOccurrencePage;
  exception_count: number;
}

export interface CalendarEdit {
  title?: string;
  description?: string;
  location?: string;
  start_time?: string;
  end_time?: string;
  all_day?: boolean;
  timezone?: string;
  recurrence_rule?: string;
}

export interface CalendarActionInput {
  selection: CalendarSelection;
  scope: "this-occurrence" | "entire-series";
  edit: CalendarEdit;
  destination_calendar_id: string | null;
  reset_exceptions: boolean;
}

export interface CalendarConfirmations {
  replacement_meeting_identity: boolean;
  reset_exceptions: boolean;
}

export interface CalendarActionPlan {
  operation_id: string;
  requires: CalendarConfirmations;
  preview: CalendarOccurrenceFields;
}

export interface CalendarActionResult {
  operation_id: string;
  stage:
    | "planned"
    | "applying"
    | "destination-verified"
    | "source-removal-pending"
    | "reconciling"
    | "completed";
  event_id: string;
  requires: CalendarConfirmations;
  auto_resume?: boolean;
}

export interface Attendee {
  email: string;
  name: string | null;
  status: string;
  is_self?: boolean;
}

/** Stored invitation with local handling and arrival metadata. */
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

export interface CalendarImportPreview {
  uid: string;
  title: string;
  description: string | null;
  location: string | null;
  start_time: string;
  end_time: string;
  all_day: boolean;
  timezone: string | null;
  method: string;
  recurrence_kind: RecurrenceKind;
  component_count: number;
  organizer_email: string | null;
  attendee_count: number;
  importable: boolean;
  import_error: string | null;
}

export interface CalendarImportResult {
  imported: number;
  skipped_existing: number;
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
  /** Remote meeting identity needed for later rescheduling or cancellation. */
  meet_binding?: MeetBinding | null;
}

/** Provider-agnostic handle for a remote meeting created for an event. */
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
  /** Internal message ID used to generate In-Reply-To and References. */
  reply_to_message_id?: string | null;
  /** OpenPGP toggles; both default to false. */
  pgp_sign?: boolean;
  pgp_encrypt?: boolean;
  /** Renders body_text as Markdown unless body_html is already set. */
  markdown?: boolean;
}

export interface PgpRecipientStatus {
  email: string;
  hasKey: boolean;
  fingerprint: string | null;
}

export interface DraftSaveOutcome {
  /** True when requested draft encryption fell back to plaintext. */
  plaintext_fallback: boolean;
}

export interface ComposeAttachment {
  token: string;
  name: string;
}

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

export interface FailedOp {
  account_id: string;
  op_type: string;
  error: string;
  timestamp: number;
}

/** Invalid as an IMAP/JMAP mailbox name, preventing remote-folder collisions. */
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

interface PgpUserId {
  uid: string;
  email: string | null;
}

interface PgpSubkey {
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
