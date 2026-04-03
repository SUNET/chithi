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
  list_id: string | null;
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

export interface ComposeMessage {
  to: string[];
  cc: string[];
  bcc: string[];
  subject: string;
  body_text: string;
  body_html: string | null;
}
