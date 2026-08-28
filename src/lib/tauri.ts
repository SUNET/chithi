import { invoke } from "@tauri-apps/api/core";
import type {
  Account,
  AccountConfig,
  AutoconfigResult,
  Folder,
  LastView,
  MessagePage,
  MessageBody,
  QuickFilter,
  SearchQuery,
  SearchHit,
} from "./types";

export async function listAccounts(): Promise<Account[]> {
  return invoke("list_accounts");
}

/// The account/folder the user was last viewing (#191), read once at
/// startup by the accounts/folders stores to restore that view.
export async function getLastView(): Promise<LastView> {
  return invoke("get_last_view");
}

/// Persist the account/folder the user is currently viewing (#191).
/// Called debounced, so callers should let rejections just log rather
/// than surface an error to the user.
export async function saveLastView(
  accountId: string,
  folderPath: string,
): Promise<void> {
  return invoke("save_last_view", { accountId, folderPath });
}

export async function addAccount(config: AccountConfig): Promise<string> {
  return invoke("add_account", { config });
}

export async function getAccountConfig(
  accountId: string,
): Promise<import("./types").AccountConfig> {
  return invoke("get_account_config", { accountId });
}

export async function updateAccount(
  accountId: string,
  config: import("./types").AccountConfig,
): Promise<void> {
  return invoke("update_account", { accountId, config });
}

export async function deleteAccount(accountId: string): Promise<void> {
  return invoke("delete_account", { accountId });
}

export async function abandonZoomAccount(
  accountId: string,
  confirmation: string,
): Promise<void> {
  return invoke("abandon_zoom_account", { accountId, confirmation });
}

/// Thunderbird-style mail-server discovery for the IMAP tab
/// (Mozilla ISP DB / provider autoconfig / .well-known / MX). No
/// CalDAV / CardDAV probing — those live on their own dedicated
/// account types now. Empty fields = not found.
///
/// `imapHostHint` / `smtpHostHint` are the host values already typed
/// in the form. When autoconfig has nothing to say for that service,
/// the backend TCP-probes the hint's standard ports so the user can
/// have the port + TLS flag filled in for a host they already know.
export async function discoverMailServers(
  email: string,
  imapHostHint?: string,
  smtpHostHint?: string,
): Promise<AutoconfigResult> {
  return invoke("discover_mail_servers", {
    email,
    imapHostHint: imapHostHint || null,
    smtpHostHint: smtpHostHint || null,
  });
}

export async function listFolders(accountId: string): Promise<Folder[]> {
  return invoke("list_folders", { accountId });
}

export async function getMessages(
  accountId: string,
  folderPath: string,
  page: number,
  perPage: number,
  sortColumn?: string,
  sortAsc?: boolean,
  filter?: QuickFilter,
): Promise<MessagePage> {
  return invoke("get_messages", {
    accountId,
    folderPath,
    page,
    perPage,
    sortColumn,
    sortAsc,
    filter,
  });
}

export async function getMessageBody(
  accountId: string,
  messageId: string,
): Promise<MessageBody> {
  return invoke("get_message_body", { accountId, messageId });
}

export async function searchMessagesServer(
  accountId: string,
  query: SearchQuery,
): Promise<SearchHit[]> {
  return invoke("search_messages_server", { accountId, query });
}

export async function importSearchHit(
  accountId: string,
  hit: SearchHit,
): Promise<string> {
  return invoke("import_search_hit", { accountId, hit });
}

export async function getMessageHtmlWithImages(
  accountId: string,
  messageId: string,
): Promise<string> {
  return invoke("get_message_html_with_images", { accountId, messageId });
}

export async function createFolder(
  accountId: string,
  folderPath: string,
): Promise<void> {
  return invoke("create_folder", { accountId, folderPath });
}

export async function deleteFolder(
  accountId: string,
  folderPath: string,
): Promise<void> {
  return invoke("delete_folder", { accountId, folderPath });
}

export async function saveAttachment(
  accountId: string,
  messageId: string,
  attachmentIndex: number,
  suggestedFilename: string,
): Promise<void> {
  return invoke("save_attachment", {
    accountId,
    messageId,
    attachmentIndex,
    suggestedFilename,
  });
}

export async function saveMessageAsEml(
  accountId: string,
  messageId: string,
  suggestedFilename: string,
): Promise<void> {
  return invoke("save_message_as_eml", {
    accountId,
    messageId,
    suggestedFilename,
  });
}

export async function syncFolder(
  accountId: string,
  folderPath: string,
): Promise<number> {
  return invoke("sync_folder", { accountId, folderPath });
}

export async function triggerSync(
  accountId: string,
  currentFolder?: string,
): Promise<void> {
  return invoke("trigger_sync", {
    accountId,
    currentFolder: currentFolder ?? null,
  });
}

export async function prefetchBodies(accountId: string): Promise<number> {
  return invoke("prefetch_bodies", { accountId });
}

export async function sendMessage(
  accountId: string,
  message: import("./types").ComposeMessage,
): Promise<void> {
  return invoke("send_message", { accountId, message });
}

export async function saveDraft(
  accountId: string,
  message: import("./types").ComposeMessage,
): Promise<import("./types").DraftSaveOutcome> {
  return invoke("save_draft", { accountId, message });
}

/**
 * Open a backend-owned native file picker and register the chosen files.
 * The renderer receives opaque tokens, never the raw paths, so a
 * compromised renderer cannot ask the backend to read arbitrary files
 * when composing a message.
 */
export async function pickAttachments(): Promise<
  Array<{ token: string; name: string }>
> {
  return invoke("pick_attachments");
}

/**
 * Release a previously-issued attachment token so the backend forgets
 * the path. Called when the user removes an attachment chip or when
 * the compose window unmounts without sending.
 */
export async function releaseAttachment(token: string): Promise<void> {
  return invoke("release_attachment", { token });
}

export async function moveMessages(
  accountId: string,
  messageIds: string[],
  targetFolder: string,
): Promise<void> {
  return invoke("move_messages", { accountId, messageIds, targetFolder });
}

export async function moveMessagesCrossAccount(
  sourceAccountId: string,
  messageIds: string[],
  targetAccountId: string,
  targetFolder: string,
): Promise<void> {
  return invoke("move_messages_cross_account", {
    sourceAccountId,
    messageIds,
    targetAccountId,
    targetFolder,
  });
}

export async function deleteMessages(
  accountId: string,
  messageIds: string[],
): Promise<void> {
  return invoke("delete_messages", { accountId, messageIds });
}

export async function setMessageFlags(
  accountId: string,
  messageIds: string[],
  flags: string[],
  add: boolean,
): Promise<void> {
  return invoke("set_message_flags", { accountId, messageIds, flags, add });
}

export async function copyMessages(
  accountId: string,
  messageIds: string[],
  targetFolder: string,
): Promise<void> {
  return invoke("copy_messages", { accountId, messageIds, targetFolder });
}

export async function markAccountRead(accountId: string): Promise<number> {
  return invoke("mark_account_read", { accountId });
}

// Threading
export async function getThreadedMessages(
  accountId: string,
  folderPath: string,
  page: number,
  perPage: number,
  sortColumn?: string,
  sortAsc?: boolean,
  filter?: QuickFilter,
): Promise<import("./types").ThreadedPage> {
  return invoke("get_threaded_messages", {
    accountId,
    folderPath,
    page,
    perPage,
    sortColumn,
    sortAsc,
    filter,
  });
}

export async function getThreadMessages(
  accountId: string,
  folderPath: string,
  threadId: string,
): Promise<import("./types").MessageSummary[]> {
  return invoke("get_thread_messages", { accountId, folderPath, threadId });
}

export async function unthreadMessage(messageId: string): Promise<void> {
  return invoke("unthread_message", { messageId });
}

// Calendar
export async function listCalendars(
  accountId: string,
): Promise<import("./types").Calendar[]> {
  return invoke("list_calendars", { accountId });
}

export async function createCalendar(
  calendar: { account_id: string; name: string; color: string; is_default: boolean },
): Promise<string> {
  return invoke("create_calendar", { calendar });
}

export async function updateCalendar(
  calendarId: string,
  name: string,
  color: string,
): Promise<void> {
  return invoke("update_calendar", { calendarId, name, color });
}

export async function deleteCalendar(calendarId: string): Promise<void> {
  return invoke("delete_calendar", { calendarId });
}

export async function getEvents(
  accountId: string,
  start: string,
  end: string,
  calendarId?: string,
): Promise<import("./types").CalendarEvent[]> {
  return invoke("get_events", { accountId, start, end, calendarId: calendarId ?? null });
}

export async function listRoomSuggestions(
  accountId: string,
): Promise<import("./types").RoomSuggestion[]> {
  return invoke("list_room_suggestions", { accountId });
}

export async function checkRoomAvailability(
  accountId: string,
  roomAddress: string,
  startTime: string,
  endTime: string,
): Promise<import("./types").RoomAvailability> {
  return invoke("check_room_availability", {
    accountId,
    roomAddress,
    startTime,
    endTime,
  });
}

export async function getParticipantSchedules(
  accountId: string,
  emails: string[],
  startTime: string,
  endTime: string,
): Promise<import("./types").ParticipantSchedule[]> {
  return invoke("get_participant_schedules", {
    accountId,
    emails,
    startTime,
    endTime,
  });
}

export async function createEvent(
  event: import("./types").NewEventInput,
): Promise<string> {
  return invoke("create_event", { event });
}

export async function updateEvent(
  eventId: string,
  event: Partial<import("./types").NewEventInput>,
): Promise<void> {
  return invoke("update_event", { eventId, event });
}

export async function deleteEvent(eventId: string): Promise<void> {
  return invoke("delete_event", { eventId });
}

export async function unsubscribeCalendar(calendarId: string): Promise<void> {
  return invoke("unsubscribe_calendar", { calendarId });
}

export async function syncCalendars(
  accountId: string,
  forceFullSync?: boolean,
): Promise<void> {
  return invoke("sync_calendars", { accountId, forceFullSync });
}

export async function getEmailInvites(
  accountId: string,
  messageId: string,
): Promise<import("./types").ParsedInvite[]> {
  return invoke("get_email_invites", { accountId, messageId });
}

export async function getEventByUid(
  accountId: string,
  uid: string,
): Promise<import("./types").CalendarEvent | null> {
  return invoke("get_event_by_uid", { accountId, uid });
}

export async function sendInvites(
  accountId: string,
  eventId: string,
  attendeeEmails: string[],
): Promise<void> {
  return invoke("send_invites", { accountId, eventId, attendeeEmails });
}

export async function processInviteReply(
  accountId: string,
  messageId: string,
): Promise<void> {
  return invoke("process_invite_reply", { accountId, messageId });
}

export async function processCancelledInvite(
  accountId: string,
  messageId: string,
): Promise<void> {
  return invoke("process_cancelled_invite", { accountId, messageId });
}

export async function getInviteStatus(
  accountId: string,
  inviteUid: string,
): Promise<string | null> {
  return invoke("get_invite_status", { accountId, inviteUid });
}

export async function respondToInvite(
  accountId: string,
  messageId: string,
  inviteUid: string,
  response: string,
): Promise<void> {
  return invoke("respond_to_invite", { accountId, messageId, inviteUid, response });
}

/** List all calendar invites for an account (Invites view). */
export async function listInvites(
  accountId: string,
): Promise<import("./types").Invite[]> {
  return invoke("list_invites", { accountId });
}

/** Mark a stored invitation as handled locally without sending an RSVP. */
export async function markInviteManaged(
  accountId: string,
  eventId: string,
): Promise<void> {
  return invoke("mark_invite_managed", { accountId, eventId });
}

/**
 * Change the RSVP for a stored calendar event. Unlike `respondToInvite`,
 * this needs no original invite email — the iTIP REPLY is rebuilt from the
 * persisted event. `response` is "accepted", "tentative", or "declined".
 */
export async function respondToEvent(
  accountId: string,
  eventId: string,
  response: string,
): Promise<void> {
  return invoke("respond_to_event", { accountId, eventId, response });
}

// Filter rules
export async function listFilters(
  accountId?: string,
): Promise<import("./types").FilterRule[]> {
  return invoke("list_filters", { accountId: accountId ?? null });
}

export async function saveFilter(
  rule: import("./types").FilterRule,
): Promise<void> {
  return invoke("save_filter", { rule });
}

export async function deleteFilter(filterId: string): Promise<void> {
  return invoke("delete_filter", { filterId });
}

export async function applyFiltersToFolder(
  accountId: string,
  folderPath: string,
): Promise<number> {
  return invoke("apply_filters_to_folder", { accountId, folderPath });
}

// Contacts
export async function listContactBooks(
  accountId: string,
): Promise<import("./types").ContactBook[]> {
  return invoke("list_contact_books", { accountId });
}

export async function listContacts(
  bookId: string,
): Promise<import("./types").Contact[]> {
  return invoke("list_contacts", { bookId });
}

export async function getContact(
  contactId: string,
): Promise<import("./types").Contact> {
  return invoke("get_contact", { contactId });
}

export async function createContact(contact: {
  book_id: string;
  display_name: string;
  emails_json: string;
  phones_json: string;
  addresses_json: string;
  organization?: string | null;
  title?: string | null;
  notes?: string | null;
}): Promise<string> {
  return invoke("create_contact", { contact });
}

export async function updateContact(
  contact: import("./types").Contact,
): Promise<void> {
  return invoke("update_contact", { contact });
}

export async function deleteContact(contactId: string): Promise<void> {
  return invoke("delete_contact", { contactId });
}

export async function searchContacts(
  query: string,
): Promise<import("./types").Contact[]> {
  return invoke("search_contacts", { query });
}

/// Cross-account search that, when the (accountId, service) binding
/// has a default contact book set, ranks matches in that book first.
/// `service` is "mail" for compose recipients, "calendar" for event
/// attendees. Pass undefined for either to fall back to plain
/// alphabetical ordering — same shape as searchContacts.
export async function searchContactsForAccount(
  query: string,
  accountId: string | null,
  service: "mail" | "calendar" | null,
): Promise<import("./types").Contact[]> {
  return invoke("search_contacts_for_account", {
    query,
    accountId: accountId ?? null,
    service: service ?? null,
  });
}

export async function getDefaultContactBook(
  accountId: string,
  service: "mail" | "calendar",
): Promise<string | null> {
  return invoke("get_default_contact_book", { accountId, service });
}

export async function setDefaultContactBook(
  accountId: string,
  service: "mail" | "calendar",
  bookId: string | null,
): Promise<void> {
  return invoke("set_default_contact_book", {
    accountId,
    service,
    bookId,
  });
}

export async function syncContacts(accountId: string): Promise<void> {
  return invoke("sync_contacts", { accountId });
}

// Meet (video conferencing — #148). One browser-assisted login
// flow per provider (Talk / Matrix / Zoom today; the set is
// driven by `meet::registry()` on the backend) plus one
// provider-agnostic create-URL. Talk uses Nextcloud Login Flow
// v2 (poll-based), Matrix uses SSO redirect to a local listener,
// Zoom uses OAuth 2.0 Authorization Code + PKCE. All end with a
// stored account row + keyring entry.

export interface TalkLoginStart {
  login_url: string;
  session_id: string;
}

export async function meetTalkLoginStart(
  serverUrl: string,
): Promise<TalkLoginStart> {
  return invoke("meet_talk_login_start", { serverUrl });
}

export async function meetTalkLoginComplete(
  sessionId: string,
  displayName?: string,
): Promise<string> {
  return invoke("meet_talk_login_complete", {
    sessionId,
    displayName: displayName ?? null,
  });
}

export interface MatrixLoginStart {
  login_url: string;
  port: number;
}

export async function meetMatrixLoginStart(
  homeserverUrl: string,
): Promise<MatrixLoginStart> {
  return invoke("meet_matrix_login_start", { homeserverUrl });
}

export async function meetMatrixLoginComplete(
  port: number,
  displayName?: string,
): Promise<string> {
  return invoke("meet_matrix_login_complete", {
    port,
    displayName: displayName ?? null,
  });
}

/// Zoom OAuth (#148). Hosted by Zoom — no per-user server URL,
/// just an OAuth Authorization Code + PKCE round-trip against
/// the Marketplace-registered app.
export interface ZoomLoginStart {
  login_url: string;
  port: number;
}

export async function meetZoomLoginStart(
  accountId?: string,
): Promise<ZoomLoginStart> {
  return invoke("meet_zoom_login_start", {
    accountId: accountId ?? null,
  });
}

export async function meetZoomLoginComplete(
  port: number,
  displayName?: string,
): Promise<string> {
  return invoke("meet_zoom_login_complete", {
    port,
    displayName: displayName ?? null,
  });
}

/// La Suite Visio add-on exchange. The backend opens a restricted auth
/// webview itself so the one-time transit token never enters renderer IPC.
export interface VisioLoginStart {
  session_id: string;
}

export async function meetVisioLoginStart(
  serverUrl: string,
  accountId?: string,
): Promise<VisioLoginStart> {
  return invoke("meet_visio_login_start", {
    serverUrl,
    accountId: accountId ?? null,
  });
}

export async function meetVisioLoginComplete(
  sessionId: string,
  displayName?: string,
): Promise<string> {
  return invoke("meet_visio_login_complete", {
    sessionId,
    displayName: displayName ?? null,
  });
}

export async function meetVisioLoginCancel(sessionId: string): Promise<void> {
  return invoke("meet_visio_login_cancel", { sessionId });
}

/// Provider-agnostic create — picks the registry entry matching
/// the account's meet binding. Returns the join URL plus the
/// provider-specific meeting id and the account/protocol used, so
/// the caller can store the binding alongside its calendar event
/// and later trigger a reschedule or delete on the same remote
/// meeting.
///
/// `startTime` (ISO 8601 UTC) and `durationMinutes` are passed
/// through to time-bound providers like Zoom so the meeting lands
/// on the event's day rather than defaulting to "today". Persistent
/// room providers (Talk, Matrix) ignore them.
export async function meetCreateUrl(
  accountId: string,
  name: string,
  startTime?: string,
  durationMinutes?: number,
): Promise<import("./types").MeetBinding> {
  return invoke("meet_create_url", {
    accountId,
    name,
    startTime: startTime ?? null,
    durationMinutes: durationMinutes ?? null,
  });
}

/** Idempotently discard a backend-owned unbound remote meeting. */
export async function meetDiscardPending(lifecycleId: string): Promise<void> {
  return invoke("meet_discard_pending", { lifecycleId });
}

// IDLE
export async function startIdle(): Promise<void> {
  return invoke("start_idle");
}

export async function stopIdle(): Promise<void> {
  return invoke("stop_idle");
}

// OAuth
export async function oauthStart(
  provider: string,
): Promise<{ url: string; port: number }> {
  return invoke("oauth_start", { provider });
}

export async function oauthComplete(
  provider: string,
  port: number,
  accountId: string,
): Promise<void> {
  return invoke("oauth_complete", { provider, port, accountId });
}

export async function oauthHasTokens(
  accountId: string,
): Promise<boolean> {
  return invoke("oauth_has_tokens", { accountId });
}

export async function oauthGetMsProfile(
  accountId: string,
): Promise<{ display_name: string; email: string }> {
  return invoke("oauth_get_ms_profile", { accountId });
}

export async function searchCollectedContacts(
  query: string,
): Promise<import("./types").CollectedContact[]> {
  return invoke("search_collected_contacts", { query });
}

// JMAP OIDC (Device Authorization Flow)
export async function jmapOidcStart(
  jmapUrl: string,
  email: string,
  clientId: string,
): Promise<{
  verification_uri: string;
  verification_uri_complete: string | null;
  user_code: string;
  device_code: string;
  interval: number;
  expires_in: number;
  token_endpoint: string;
  client_id: string;
}> {
  return invoke("jmap_oidc_start", { jmapUrl, email, clientId });
}

export async function jmapOidcComplete(
  deviceCode: string,
  tokenEndpoint: string,
  interval: number,
  expiresIn: number,
  accountId: string,
  clientId: string,
): Promise<void> {
  return invoke("jmap_oidc_complete", {
    deviceCode,
    tokenEndpoint,
    interval,
    expiresIn,
    accountId,
    clientId,
  });
}

export async function openOauthUrl(url: string): Promise<void> {
  return invoke("open_oauth_url", { url });
}

export async function cleanUrl(url: string): Promise<string> {
  return invoke("clean_url", { url });
}

export async function openLink(url: string): Promise<void> {
  return invoke("open_link", { url });
}

export async function listTimezones(): Promise<string[]> {
  return invoke("list_timezones");
}

export async function getDefaultTimezone(): Promise<string> {
  return invoke("get_default_timezone");
}

// Outbox
export async function listOutbox(
  accountId: string,
): Promise<import("./types").OutboxRow[]> {
  return invoke("list_outbox", { accountId });
}

export async function retryOutboxOp(
  accountId: string,
  outboxId: number,
): Promise<void> {
  return invoke("retry_outbox_op", { accountId, outboxId });
}

export async function discardOutboxOp(
  accountId: string,
  outboxId: number,
): Promise<void> {
  return invoke("discard_outbox_op", { accountId, outboxId });
}

// OpenPGP
export async function pgpListKeys(): Promise<import("./types").PgpKeySummary[]> {
  return invoke("pgp_list_keys");
}

export async function pgpGetKey(
  fingerprint: string,
): Promise<import("./types").PgpKeySummary> {
  return invoke("pgp_get_key", { fingerprint });
}

export async function pgpImportKey(
  data: Uint8Array,
): Promise<import("./types").PgpImportResult> {
  // Tauri serialises Uint8Array → JSON array of numbers — backend command
  // takes Vec<u8>.
  return invoke("pgp_import_key", { data: Array.from(data) });
}

/** Opens the native file dialog server-side and imports the picked file.
 *  Returns null if the user cancels. */
export async function pgpPickAndImportKey(): Promise<
  import("./types").PgpImportResult | null
> {
  return invoke("pgp_pick_and_import_key");
}

export async function pgpDeleteKey(fingerprint: string): Promise<void> {
  return invoke("pgp_delete_key", { fingerprint });
}

export async function pgpExportPublic(fingerprint: string): Promise<string> {
  return invoke("pgp_export_public", { fingerprint });
}

/** Returns the fingerprint of the imported key. */
export async function pgpWkdFetch(email: string): Promise<string> {
  return invoke("pgp_wkd_fetch", { email });
}

export async function pgpListCards(): Promise<import("./types").PgpCardSummary[]> {
  return invoke("pgp_list_cards");
}

export async function pgpCardDetails(
  ident: string,
): Promise<import("./types").PgpCardDetails> {
  return invoke("pgp_card_details", { ident });
}

export async function pgpAutoLinkCards(): Promise<
  import("./types").PgpCardDetection[]
> {
  return invoke("pgp_auto_link_cards");
}

export async function pgpDecryptMessage(
  accountId: string,
  messageId: string,
): Promise<import("./types").PgpDecryptedMessage> {
  return invoke("pgp_decrypt_message", { accountId, messageId });
}

export async function pgpVerifyMessage(
  accountId: string,
  messageId: string,
): Promise<import("./types").PgpVerifyOutcome> {
  return invoke("pgp_verify_message", { accountId, messageId });
}

export async function pgpCheckRecipients(
  recipients: string[],
): Promise<import("./types").PgpRecipientStatus[]> {
  return invoke("pgp_check_recipients", { recipients });
}
