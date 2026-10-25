import { invoke } from "@tauri-apps/api/core";
import type * as Types from "./types";

export function listAccounts(): Promise<Types.Account[]> {
  return invoke("list_accounts");
}

export function getLastView(): Promise<Types.LastView> {
  return invoke("get_last_view");
}

export function saveLastView(
  accountId: string,
  folderPath: string,
): Promise<void> {
  return invoke("save_last_view", { accountId, folderPath });
}

export function addAccount(config: Types.AccountConfig): Promise<string> {
  return invoke("add_account", { config });
}

export function getAccountConfig(
  accountId: string,
): Promise<Types.AccountConfig> {
  return invoke("get_account_config", { accountId });
}

export function updateAccount(
  accountId: string,
  config: Types.AccountConfig,
): Promise<void> {
  return invoke("update_account", { accountId, config });
}

export function deleteAccount(accountId: string): Promise<void> {
  return invoke("delete_account", { accountId });
}

export function abandonZoomAccount(
  accountId: string,
  confirmation: string,
): Promise<void> {
  return invoke("abandon_zoom_account", { accountId, confirmation });
}

/** Probes provided IMAP/SMTP host hints when automatic discovery has no result. */
export function discoverMailServers(
  email: string,
  imapHostHint?: string,
  smtpHostHint?: string,
): Promise<Types.AutoconfigResult> {
  return invoke("discover_mail_servers", {
    email,
    imapHostHint: imapHostHint || null,
    smtpHostHint: smtpHostHint || null,
  });
}

export function listFolders(accountId: string): Promise<Types.Folder[]> {
  return invoke("list_folders", { accountId });
}

export function getMessages(
  accountId: string,
  folderPath: string,
  page: number,
  perPage: number,
  sortColumn?: string,
  sortAsc?: boolean,
  filter?: Types.QuickFilter,
): Promise<Types.MessagePage> {
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

export function getMessageBody(
  accountId: string,
  messageId: string,
): Promise<Types.MessageBody> {
  return invoke("get_message_body", { accountId, messageId });
}

export function searchMessagesServer(
  accountId: string,
  query: Types.SearchQuery,
): Promise<Types.SearchHit[]> {
  return invoke("search_messages_server", { accountId, query });
}

export function importSearchHit(
  accountId: string,
  hit: Types.SearchHit,
): Promise<string> {
  return invoke("import_search_hit", { accountId, hit });
}

export function getMessageHtmlWithImages(
  accountId: string,
  messageId: string,
): Promise<string> {
  return invoke("get_message_html_with_images", { accountId, messageId });
}

export function createFolder(
  accountId: string,
  folderPath: string,
): Promise<void> {
  return invoke("create_folder", { accountId, folderPath });
}

export function deleteFolder(
  accountId: string,
  folderPath: string,
): Promise<void> {
  return invoke("delete_folder", { accountId, folderPath });
}

export function saveAttachment(
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

export function saveMessageAsEml(
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

export function syncFolder(
  accountId: string,
  folderPath: string,
): Promise<number> {
  return invoke("sync_folder", { accountId, folderPath });
}

export function triggerSync(
  accountId: string,
  currentFolder?: string,
): Promise<void> {
  return invoke("trigger_sync", {
    accountId,
    currentFolder: currentFolder ?? null,
  });
}

export function prefetchBodies(accountId: string): Promise<number> {
  return invoke("prefetch_bodies", { accountId });
}

export function sendMessage(
  accountId: string,
  message: Types.ComposeMessage,
): Promise<void> {
  return invoke("send_message", { accountId, message });
}

export function saveDraft(
  accountId: string,
  message: Types.ComposeMessage,
): Promise<Types.DraftSaveOutcome> {
  return invoke("save_draft", { accountId, message });
}

/** Returns opaque attachment tokens so renderer code never receives file paths. */
export function pickAttachments(): Promise<Array<{ token: string; name: string }>> {
  return invoke("pick_attachments");
}

/** Releases the backend path associated with an attachment token. */
export function releaseAttachment(token: string): Promise<void> {
  return invoke("release_attachment", { token });
}

export function moveMessages(
  accountId: string,
  messageIds: string[],
  targetFolder: string,
): Promise<void> {
  return invoke("move_messages", { accountId, messageIds, targetFolder });
}

export function moveMessagesCrossAccount(
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

export function deleteMessages(
  accountId: string,
  messageIds: string[],
): Promise<void> {
  return invoke("delete_messages", { accountId, messageIds });
}

export function setMessageFlags(
  accountId: string,
  messageIds: string[],
  flags: string[],
  add: boolean,
): Promise<void> {
  return invoke("set_message_flags", { accountId, messageIds, flags, add });
}

export function copyMessages(
  accountId: string,
  messageIds: string[],
  targetFolder: string,
): Promise<void> {
  return invoke("copy_messages", { accountId, messageIds, targetFolder });
}

export function markAccountRead(accountId: string): Promise<number> {
  return invoke("mark_account_read", { accountId });
}

export function getThreadedMessages(
  accountId: string,
  folderPath: string,
  page: number,
  perPage: number,
  sortColumn?: string,
  sortAsc?: boolean,
  filter?: Types.QuickFilter,
): Promise<Types.ThreadedPage> {
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

export function getThreadMessages(
  accountId: string,
  folderPath: string,
  threadId: string,
): Promise<Types.MessageSummary[]> {
  return invoke("get_thread_messages", { accountId, folderPath, threadId });
}

export function unthreadMessage(messageId: string): Promise<void> {
  return invoke("unthread_message", { messageId });
}

export function listCalendars(
  accountId: string,
): Promise<Types.Calendar[]> {
  return invoke("list_calendars", { accountId });
}

export function listArchivedGraphCalendars(
  accountId: string,
): Promise<Types.ArchivedGraphCalendar[]> {
  return invoke("list_archived_graph_calendars", { accountId });
}

export function acknowledgeArchivedGraphCalendar(
  accountId: string,
  calendarId: string,
): Promise<void> {
  return invoke("acknowledge_archived_graph_calendar", { accountId, calendarId });
}

export function updateCalendar(
  calendarId: string,
  name: string,
  color: string,
): Promise<void> {
  return invoke("update_calendar", { calendarId, name, color });
}

export function getEvents(
  accountId: string,
  start: string,
  end: string,
  calendarId?: string,
): Promise<Types.CalendarEvent[]> {
  return invoke("get_events", {
    accountId,
    start,
    end,
    calendarId: calendarId ?? null,
  });
}

export function getCalendarEvent(
  eventId: string,
): Promise<Types.CalendarEvent> {
  return invoke("get_calendar_event", { eventId });
}

export function listRoomSuggestions(
  accountId: string,
): Promise<Types.RoomSuggestion[]> {
  return invoke("list_room_suggestions", { accountId });
}

export function checkRoomAvailability(
  accountId: string,
  roomAddress: string,
  startTime: string,
  endTime: string,
): Promise<Types.RoomAvailability> {
  return invoke("check_room_availability", {
    accountId,
    roomAddress,
    startTime,
    endTime,
  });
}

export function getParticipantSchedules(
  accountId: string,
  emails: string[],
  startTime: string,
  endTime: string,
): Promise<Types.ParticipantSchedule[]> {
  return invoke("get_participant_schedules", {
    accountId,
    emails,
    startTime,
    endTime,
  });
}

export function createEvent(
  event: Types.NewEventInput,
  operationId: string,
): Promise<string> {
  return invoke("create_event", { event, operationId });
}

export function updateEvent(
  eventId: string,
  event: Partial<Types.NewEventInput>,
): Promise<void> {
  return invoke("update_event", { eventId, event });
}

export function readCalendarEventSet(
  eventId: string,
  start: string,
  end: string,
  limit = 200,
): Promise<Types.CalendarEventSetView> {
  return invoke("read_calendar_event_set", { eventId, start, end, limit });
}

export function listCalendarOccurrences(
  accountId: string,
  calendarId: string | null,
  start: string,
  end: string,
  limit = 2000,
): Promise<Types.CalendarOccurrencePage> {
  return invoke("list_calendar_occurrences", {
    accountId,
    calendarId,
    start,
    end,
    limit,
  });
}

export function repairCalendarOccurrence(eventId: string): Promise<{
  repaired: boolean;
  retry_after_seconds: number | null;
}> {
  return invoke("repair_calendar_occurrence", { eventId });
}

export function planCalendarAction(
  input: Types.CalendarActionInput,
): Promise<Types.CalendarActionPlan> {
  return invoke("plan_calendar_action", { input });
}

export function planCalendarOccurrenceAction(
  eventId: string,
  originalStart: string,
  expected: Types.CalendarOccurrenceFields,
  edit: Types.CalendarEdit,
): Promise<Types.CalendarActionPlan> {
  return invoke("plan_calendar_occurrence_action", {
    eventId,
    originalStart,
    expected,
    edit,
  });
}

export function executeCalendarAction(
  operationId: string,
  confirmations: Types.CalendarConfirmations,
): Promise<Types.CalendarActionResult> {
  return invoke("execute_calendar_action", { operationId, confirmations });
}

export function listPendingCalendarActions(
  accountId: string,
): Promise<Types.CalendarActionResult[]> {
  return invoke("list_pending_calendar_actions", { accountId });
}

export function discardCalendarAction(operationId: string): Promise<void> {
  return invoke("discard_calendar_action", { operationId });
}

export function deleteEvent(eventId: string): Promise<void> {
  return invoke("delete_event", { eventId });
}

export function moveEventToCalendar(
  eventId: string,
  targetCalendarId: string,
  targetAccountId: string,
): Promise<string> {
  return invoke("move_event_to_calendar", {
    eventId,
    targetCalendarId,
    targetAccountId,
  });
}

export function unsubscribeCalendar(calendarId: string): Promise<void> {
  return invoke("unsubscribe_calendar", { calendarId });
}

export function syncCalendars(
  accountId: string,
  forceFullSync?: boolean,
): Promise<void> {
  return invoke("sync_calendars", { accountId, forceFullSync });
}

export function getEmailInvites(
  accountId: string,
  messageId: string,
): Promise<Types.ParsedInvite[]> {
  return invoke("get_email_invites", { accountId, messageId });
}

export function previewCalendarAttachment(
  sourceAccountId: string,
  messageId: string,
  attachmentIndex: number,
  calendarId: string,
): Promise<Types.CalendarImportPreview[]> {
  return invoke("preview_calendar_attachment", {
    sourceAccountId,
    messageId,
    attachmentIndex,
    calendarId,
  });
}

export function listCalendarImportTargets(): Promise<Types.Calendar[]> {
  return invoke("list_calendar_import_targets");
}

export function getDefaultImportCalendar(
  accountId: string,
): Promise<string | null> {
  return invoke("get_default_import_calendar", { accountId });
}

export function setDefaultImportCalendar(
  accountId: string,
  calendarId: string | null,
): Promise<void> {
  return invoke("set_default_import_calendar", { accountId, calendarId });
}

export function importCalendarAttachment(
  sourceAccountId: string,
  messageId: string,
  attachmentIndex: number,
  calendarId: string,
  selectedUids: string[],
): Promise<Types.CalendarImportResult> {
  return invoke("import_calendar_attachment", {
    sourceAccountId,
    messageId,
    attachmentIndex,
    calendarId,
    selectedUids,
  });
}

export function sendInvites(
  accountId: string,
  eventId: string,
  attendeeEmails: string[],
): Promise<void> {
  return invoke("send_invites", { accountId, eventId, attendeeEmails });
}

export function notifyCalendarEvent(
  eventId: string,
): Promise<void> {
  return invoke("notify_calendar_event", { eventId });
}

export function processInviteReply(
  accountId: string,
  messageId: string,
): Promise<void> {
  return invoke("process_invite_reply", { accountId, messageId });
}

export function processCancelledInvite(
  accountId: string,
  messageId: string,
): Promise<void> {
  return invoke("process_cancelled_invite", { accountId, messageId });
}

export function getInviteStatus(
  accountId: string,
  inviteUid: string,
): Promise<string | null> {
  return invoke("get_invite_status", { accountId, inviteUid });
}

export function respondToInvite(
  accountId: string,
  messageId: string,
  inviteUid: string,
  response: string,
): Promise<void> {
  return invoke("respond_to_invite", {
    accountId,
    messageId,
    inviteUid,
    response,
  });
}

export function listInvites(
  accountId: string,
): Promise<Types.Invite[]> {
  return invoke("list_invites", { accountId });
}

/** Marks an invitation as handled without sending an RSVP. */
export function markInviteManaged(
  accountId: string,
  eventId: string,
): Promise<void> {
  return invoke("mark_invite_managed", { accountId, eventId });
}

/** Rebuilds an iTIP RSVP from a persisted event instead of the original email. */
export function respondToEvent(
  accountId: string,
  eventId: string,
  response: string,
): Promise<void> {
  return invoke("respond_to_event", { accountId, eventId, response });
}

export function listFilters(
  accountId?: string,
): Promise<Types.FilterRule[]> {
  return invoke("list_filters", { accountId: accountId ?? null });
}

export function saveFilter(rule: Types.FilterRule): Promise<void> {
  return invoke("save_filter", { rule });
}

export function deleteFilter(filterId: string): Promise<void> {
  return invoke("delete_filter", { filterId });
}

export function applyFiltersToFolder(
  accountId: string,
  folderPath: string,
): Promise<number> {
  return invoke("apply_filters_to_folder", { accountId, folderPath });
}

export function listContactBooks(
  accountId: string,
): Promise<Types.ContactBook[]> {
  return invoke("list_contact_books", { accountId });
}

export function listContacts(
  bookId: string,
): Promise<Types.Contact[]> {
  return invoke("list_contacts", { bookId });
}

export function createContact(contact: {
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

export function updateContact(
  contact: Types.Contact,
): Promise<void> {
  return invoke("update_contact", { contact });
}

export function deleteContact(contactId: string): Promise<void> {
  return invoke("delete_contact", { contactId });
}

export function searchContacts(
  query: string,
): Promise<Types.Contact[]> {
  return invoke("search_contacts", { query });
}

/** Prioritizes the account/service default book; nulls use alphabetical order. */
export function searchContactsForAccount(
  query: string,
  accountId: string | null,
  service: "mail" | "calendar" | null,
): Promise<Types.Contact[]> {
  return invoke("search_contacts_for_account", {
    query,
    accountId: accountId ?? null,
    service: service ?? null,
  });
}

export function getDefaultContactBook(
  accountId: string,
  service: "mail" | "calendar",
): Promise<string | null> {
  return invoke("get_default_contact_book", { accountId, service });
}

export function setDefaultContactBook(
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

export function syncContacts(accountId: string): Promise<void> {
  return invoke("sync_contacts", { accountId });
}

interface TalkLoginStart {
  login_url: string;
  session_id: string;
}

export function meetTalkLoginStart(
  serverUrl: string,
): Promise<TalkLoginStart> {
  return invoke("meet_talk_login_start", { serverUrl });
}

export function meetTalkLoginComplete(
  sessionId: string,
  displayName?: string,
): Promise<string> {
  return invoke("meet_talk_login_complete", {
    sessionId,
    displayName: displayName ?? null,
  });
}

interface PortLoginStart {
  login_url: string;
  port: number;
}

export function meetMatrixLoginStart(
  homeserverUrl: string,
): Promise<PortLoginStart> {
  return invoke("meet_matrix_login_start", { homeserverUrl });
}

export function meetMatrixLoginComplete(
  port: number,
  displayName?: string,
): Promise<string> {
  return invoke("meet_matrix_login_complete", {
    port,
    displayName: displayName ?? null,
  });
}

export function meetZoomLoginStart(
  accountId?: string,
): Promise<PortLoginStart> {
  return invoke("meet_zoom_login_start", {
    accountId: accountId ?? null,
  });
}

export function meetZoomLoginComplete(
  port: number,
  displayName?: string,
): Promise<string> {
  return invoke("meet_zoom_login_complete", {
    port,
    displayName: displayName ?? null,
  });
}

/** The backend owns Visio's auth webview so its transit token never enters IPC. */
interface VisioLoginStart {
  session_id: string;
}

export function meetVisioLoginStart(
  serverUrl: string,
  accountId?: string,
): Promise<VisioLoginStart> {
  return invoke("meet_visio_login_start", {
    serverUrl,
    accountId: accountId ?? null,
  });
}

export function meetVisioLoginComplete(
  sessionId: string,
  displayName?: string,
): Promise<string> {
  return invoke("meet_visio_login_complete", {
    sessionId,
    displayName: displayName ?? null,
  });
}

export function meetVisioLoginCancel(sessionId: string): Promise<void> {
  return invoke("meet_visio_login_cancel", { sessionId });
}

/** Creates through the account's provider; persistent rooms ignore the schedule. */
export function meetCreateUrl(
  accountId: string,
  name: string,
  startTime?: string,
  durationMinutes?: number,
): Promise<Types.MeetBinding> {
  return invoke("meet_create_url", {
    accountId,
    name,
    startTime: startTime ?? null,
    durationMinutes: durationMinutes ?? null,
  });
}

/** Idempotently discard a backend-owned unbound remote meeting. */
export function meetDiscardPending(lifecycleId: string): Promise<void> {
  return invoke("meet_discard_pending", { lifecycleId });
}

export function startIdle(): Promise<void> {
  return invoke("start_idle");
}

export function stopIdle(): Promise<void> {
  return invoke("stop_idle");
}

export function oauthStart(
  provider: string,
): Promise<{ url: string; port: number }> {
  return invoke("oauth_start", { provider });
}

export function oauthComplete(
  provider: string,
  port: number,
  accountId: string,
): Promise<void> {
  return invoke("oauth_complete", { provider, port, accountId });
}

export function oauthHasTokens(
  accountId: string,
): Promise<boolean> {
  return invoke("oauth_has_tokens", { accountId });
}

export function oauthGetMsProfile(
  accountId: string,
): Promise<{ display_name: string; email: string }> {
  return invoke("oauth_get_ms_profile", { accountId });
}

export function searchCollectedContacts(
  query: string,
): Promise<Types.CollectedContact[]> {
  return invoke("search_collected_contacts", { query });
}

export function jmapOidcStart(
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

export function jmapOidcComplete(
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

export function openOauthUrl(url: string): Promise<void> {
  return invoke("open_oauth_url", { url });
}

export function cleanUrl(url: string): Promise<string> {
  return invoke("clean_url", { url });
}

export function openLink(url: string): Promise<void> {
  return invoke("open_link", { url });
}

export function listTimezones(): Promise<string[]> {
  return invoke("list_timezones");
}

export function getDefaultTimezone(): Promise<string> {
  return invoke("get_default_timezone");
}

export function listOutbox(
  accountId: string,
): Promise<Types.OutboxRow[]> {
  return invoke("list_outbox", { accountId });
}

export function retryOutboxOp(
  accountId: string,
  outboxId: number,
): Promise<void> {
  return invoke("retry_outbox_op", { accountId, outboxId });
}

export function discardOutboxOp(
  accountId: string,
  outboxId: number,
): Promise<void> {
  return invoke("discard_outbox_op", { accountId, outboxId });
}

export function pgpListKeys(): Promise<Types.PgpKeySummary[]> {
  return invoke("pgp_list_keys");
}

export function pgpImportKey(
  data: Uint8Array,
): Promise<Types.PgpImportResult> {
  // Tauri serializes Uint8Array as numbers for the backend Vec<u8>.
  return invoke("pgp_import_key", { data: Array.from(data) });
}

/** Returns null when the native file picker is cancelled. */
export function pgpPickAndImportKey(): Promise<Types.PgpImportResult | null> {
  return invoke("pgp_pick_and_import_key");
}

export function pgpDeleteKey(fingerprint: string): Promise<void> {
  return invoke("pgp_delete_key", { fingerprint });
}

export function pgpExportPublic(fingerprint: string): Promise<string> {
  return invoke("pgp_export_public", { fingerprint });
}

/** Returns the fingerprint of the imported key. */
export function pgpWkdFetch(email: string): Promise<string> {
  return invoke("pgp_wkd_fetch", { email });
}

export function pgpListCards(): Promise<Types.PgpCardSummary[]> {
  return invoke("pgp_list_cards");
}

export function pgpCardDetails(
  ident: string,
): Promise<Types.PgpCardDetails> {
  return invoke("pgp_card_details", { ident });
}

export function pgpAutoLinkCards(): Promise<Types.PgpCardDetection[]> {
  return invoke("pgp_auto_link_cards");
}

export function pgpDecryptMessage(
  accountId: string,
  messageId: string,
): Promise<Types.PgpDecryptedMessage> {
  return invoke("pgp_decrypt_message", { accountId, messageId });
}

export function pgpVerifyMessage(
  accountId: string,
  messageId: string,
): Promise<Types.PgpVerifyOutcome> {
  return invoke("pgp_verify_message", { accountId, messageId });
}

export function pgpCheckRecipients(
  recipients: string[],
): Promise<Types.PgpRecipientStatus[]> {
  return invoke("pgp_check_recipients", { recipients });
}
