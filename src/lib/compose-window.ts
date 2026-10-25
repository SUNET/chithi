import { WebviewWindow } from "@tauri-apps/api/webviewWindow";

let composeCounter = 0;

export interface ComposeParams {
  accountId?: string;
  replyTo?: string;
  to?: string;
  cc?: string;
  bcc?: string;
  subject?: string;
  body?: string;
  draftId?: string;
  pgpEncrypt?: boolean;
  /** Requested initial state; signing still requires a usable account key. */
  pgpSign?: boolean;
}

const TEXT_QUERY_KEYS = [
  "accountId",
  "replyTo",
  "to",
  "cc",
  "bcc",
  "subject",
  "body",
  "draftId",
] as const;

export function openComposeWindow(params: ComposeParams = {}) {
  composeCounter += 1;
  const label = `compose-${composeCounter}`;

  const query = new URLSearchParams();
  for (const key of TEXT_QUERY_KEYS) {
    const value = params[key];
    if (value) query.set(key, value);
  }
  if (params.pgpEncrypt) query.set("pgpEncrypt", "1");
  if (params.pgpSign) query.set("pgpSign", "1");

  const queryString = query.toString();
  const url = queryString ? `/compose?${queryString}` : "/compose";

  const titleSuffix = params.subject || "(no subject)";
  const composeWindow = new WebviewWindow(label, {
    url,
    title: `Write ${titleSuffix} - Chithi`,
    width: 1024,
    height: 700,
    center: true,
    resizable: true,
    focus: true,
  });

  composeWindow.once("tauri://error", (error) => {
    console.error("Failed to create compose window:", error);
  });
}
