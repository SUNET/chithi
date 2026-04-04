import { invoke } from "@tauri-apps/api/core";
import type {
  Account,
  AccountConfig,
  Folder,
  MessagePage,
  MessageBody,
  SyncStatus,
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
): Promise<MessagePage> {
  return invoke("get_messages", {
    accountId,
    folderPath,
    page,
    perPage,
    sortColumn,
    sortAsc,
  });
}

export async function getMessageBody(
  accountId: string,
  messageId: string,
): Promise<MessageBody> {
  return invoke("get_message_body", { accountId, messageId });
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

export async function moveMessages(
  accountId: string,
  messageIds: string[],
  targetFolder: string,
): Promise<void> {
  return invoke("move_messages", { accountId, messageIds, targetFolder });
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

// Threading
export async function getThreadedMessages(
  accountId: string,
  folderPath: string,
  page: number,
  perPage: number,
  sortColumn?: string,
  sortAsc?: boolean,
): Promise<import("./types").ThreadedPage> {
  return invoke("get_threaded_messages", {
    accountId,
    folderPath,
    page,
    perPage,
    sortColumn,
    sortAsc,
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

export async function syncCalendars(accountId: string): Promise<void> {
  return invoke("sync_calendars", { accountId });
}

export async function getEmailInvites(
  accountId: string,
  messageId: string,
): Promise<import("./types").ParsedInvite[]> {
  return invoke("get_email_invites", { accountId, messageId });
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
