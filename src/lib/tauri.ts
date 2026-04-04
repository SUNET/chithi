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

export async function backfillThreads(accountId: string): Promise<number> {
  return invoke("backfill_threads", { accountId });
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
