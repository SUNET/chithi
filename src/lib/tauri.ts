import { invoke } from "@tauri-apps/api/core";
import type {
  Account,
  AccountConfig,
  Folder,
  MessagePage,
  MessageBody,
  SyncStatus,
  QuickFilter,
  SearchQuery,
  SearchHit,
} from "./types";

export async function listAccounts(): Promise<Account[]> {
  return invoke("list_accounts");
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

export async function getSyncStatus(accountId: string): Promise<SyncStatus> {
  return invoke("get_sync_status", { accountId });
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
): Promise<void> {
  return invoke("save_draft", { accountId, message });
}

/**
 * Open a backend-owned native file picker and register the chosen files.
 * The renderer receives opaque tokens, never the raw paths, so a
 * compromised renderer cannot ask the backend to read arbitrary files
 * when composing a message.
 */
export async function pickAttachments(): Promise<
  Array<{ token: string; name: string; size: number }>
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

export async function syncContacts(accountId: string): Promise<void> {
  return invoke("sync_contacts", { accountId });
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

export async function oauthGetToken(
  provider: string,
  accountId: string,
): Promise<string> {
  return invoke("oauth_get_token", { provider, accountId });
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

export async function listTimezones(): Promise<string[]> {
  return invoke("list_timezones");
}

export async function getDefaultTimezone(): Promise<string> {
  return invoke("get_default_timezone");
}
