import { WebviewWindow } from "@tauri-apps/api/webviewWindow";

let composeCounter = 0;

export interface ComposeParams {
  accountId?: string;
  replyTo?: string;
  to?: string;
  cc?: string;
  subject?: string;
  body?: string;
}

export function openComposeWindow(params: ComposeParams = {}) {
  composeCounter++;
  const label = `compose-${composeCounter}`;

  const query = new URLSearchParams();
  if (params.accountId) query.set("accountId", params.accountId);
  if (params.replyTo) query.set("replyTo", params.replyTo);
  if (params.to) query.set("to", params.to);
  if (params.cc) query.set("cc", params.cc);
  if (params.subject) query.set("subject", params.subject);
  if (params.body) query.set("body", params.body);

  const queryStr = query.toString();
  const url = queryStr ? `/compose?${queryStr}` : "/compose";

  const titleSuffix = params.subject ? params.subject : "(no subject)";
  const win = new WebviewWindow(label, {
    url,
    title: `Write ${titleSuffix} - Chithi`,
    width: 1024,
    height: 700,
    center: true,
    resizable: true,
    focus: true,
  });

  win.once("tauri://error", (e) => {
    console.error("Failed to create compose window:", e);
  });
}
