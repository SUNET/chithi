import { WebviewWindow } from "@tauri-apps/api/webviewWindow";

let readerCounter = 0;

export function openReaderWindow(accountId: string, messageId: string, subject?: string) {
  readerCounter++;
  const label = `reader-${readerCounter}`;

  const query = new URLSearchParams();
  query.set("accountId", accountId);
  query.set("messageId", messageId);

  const url = `/reader?${query.toString()}`;
  const titleSuffix = subject || "(no subject)";

  const win = new WebviewWindow(label, {
    url,
    title: `${titleSuffix} - Chithi`,
    width: 900,
    height: 700,
    center: true,
    resizable: true,
    focus: true,
  });

  win.once("tauri://error", (e) => {
    console.error("Failed to create reader window:", e);
  });
}
