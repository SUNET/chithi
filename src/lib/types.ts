export interface Account {
  id: string;
  display_name: string;
  email: string;
  provider: "generic" | "gmail" | "microsoft365";
  enabled: boolean;
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

export interface SyncStatus {
  account_id: string;
  is_syncing: boolean;
  last_sync: string | null;
  error: string | null;
}

export interface AccountConfig {
  display_name: string;
  email: string;
  provider: "generic" | "gmail" | "microsoft365";
  imap_host: string;
  imap_port: number;
  smtp_host: string;
  smtp_port: number;
  username: string;
  password: string;
  use_tls: boolean;
}
