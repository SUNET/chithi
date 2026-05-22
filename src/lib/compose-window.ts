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
  /** chithi message id of a saved draft to resume. When set, ComposeView
   *  fetches the draft (decrypting it first if it's an encrypted draft)
   *  and pre-fills the form. Mutually exclusive with the reply/prefill
   *  params above in practice. */
  draftId?: string;
}

export function openComposeWindow(params: ComposeParams = {}) {
  composeCounter++;
  const label = `compose-${composeCounter}`;

  const query = new URLSearchParams();
  if (params.accountId) query.set("accountId", params.accountId);
  if (params.replyTo) query.set("replyTo", params.replyTo);
  if (params.to) query.set("to", params.to);
  if (params.cc) query.set("cc", params.cc);
  if (params.bcc) query.set("bcc", params.bcc);
  if (params.subject) query.set("subject", params.subject);
  if (params.body) query.set("body", params.body);
  if (params.draftId) query.set("draftId", params.draftId);

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
