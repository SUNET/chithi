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

export async function triggerSync(accountId: string): Promise<void> {
  return invoke("trigger_sync", { accountId });
}

export async function getSyncStatus(accountId: string): Promise<SyncStatus> {
  return invoke("get_sync_status", { accountId });
}

export async function prefetchBodies(accountId: string): Promise<number> {
  return invoke("prefetch_bodies", { accountId });
}
