import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount, type VueWrapper } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

vi.mock("@/lib/tauri", () => ({
  getEmailInvites: vi.fn().mockResolvedValue([]),
  getMessageHtmlWithImages: vi.fn(),
}));
vi.mock("@/lib/compose-window", () => ({
  openComposeWindow: vi.fn(),
}));

import MessageReader from "@/components/mail/MessageReader.vue";
import { useAccountsStore } from "@/stores/accounts";
import { useMessagesStore } from "@/stores/messages";
import { useUiStore } from "@/stores/ui";
import * as api from "@/lib/tauri";
import type { MessageBody } from "@/lib/types";
import tauriConfig from "../../src-tauri/tauri.conf.json";

const message: MessageBody = {
  id: "m1",
  subject: "Newsletter",
  from: { email: "news@example.org", name: "News" },
  to: [{ email: "reader@example.org", name: "Reader" }],
  cc: [],
  date: "2026-08-24T10:00:00Z",
  flags: [],
  body_html: '<table style="height: 900px"><tr><td><a href="https://example.org/story">Newsletter</a></td></tr></table>',
  body_text: null,
  attachments: [],
  is_encrypted: false,
  is_signed: false,
  list_id: null,
  has_remote_images: true,
};

let wrapper: VueWrapper | null = null;

function mountReader() {
  const accountsStore = useAccountsStore();
  accountsStore.accounts = [
    {
      id: "acc1",
      display_name: "Account",
      email: "reader@example.org",
      username: "reader@example.org",
      provider: "generic",
      mail_protocol: "jmap",
      enabled: true,
      mail_sync_interval_seconds: null,
      calendar_sync_interval_seconds: null,
      contacts_sync_interval_seconds: null,
      has_calendar_binding: false,
      has_contacts_binding: false,
      meet_protocol: "",
    },
  ];
  accountsStore.activeAccountId = "acc1";

  const messagesStore = useMessagesStore();
  messagesStore.activeMessageId = message.id;
  messagesStore.activeMessage = { ...message };
  useUiStore().setPreferHtmlBody(true);

  wrapper = mount(MessageReader, { attachTo: document.body });
  return wrapper;
}

function parseIframeDocument(reader: VueWrapper): Document {
  const srcdoc = reader.get('[data-testid="reader-body-iframe"]').attributes("srcdoc");
  if (srcdoc === undefined) throw new Error("iframe srcdoc is missing");
  return new DOMParser().parseFromString(srcdoc, "text/html");
}

function runIframeBootstrap(doc: Document) {
  const source = doc.querySelector("script")?.textContent;
  if (!source) throw new Error("iframe bootstrap script is missing");

  // Happy DOM's DOMParser omits Document.images; real browser documents
  // expose an empty HTMLCollection when the message has no images.
  Object.defineProperty(doc, "images", { value: [], configurable: true });
  const postMessage = vi.fn();
  const iframeWindow = { addEventListener: vi.fn() };
  const execute = new Function("document", "parent", "ResizeObserver", "window", source);
  execute(doc, { postMessage }, undefined, iframeWindow);
  postMessage.mockClear();
  return postMessage;
}

async function sha256Source(source: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(source));
  const binary = Array.from(new Uint8Array(digest), (byte) => String.fromCharCode(byte)).join("");
  return `sha256-${btoa(binary)}`;
}

beforeEach(() => {
  localStorage.clear();
  setActivePinia(createPinia());
  vi.clearAllMocks();
  vi.mocked(api.getMessageHtmlWithImages).mockResolvedValue(
    '<img src="https://example.org/banner.png" height="1200" alt="Banner">',
  );
});

afterEach(() => {
  wrapper?.unmount();
  wrapper = null;
  document.body.innerHTML = "";
});

describe("MessageReader HTML iframe sizing", () => {
  it("allows only the exact trusted bootstrap in both inherited CSP policies", async () => {
    const reader = mountReader();
    const doc = parseIframeDocument(reader);
    const source = doc.querySelector("script")?.textContent;
    const iframeCsp = doc.querySelector<HTMLMetaElement>(
      'meta[http-equiv="Content-Security-Policy"]',
    )?.content;

    expect(source).toBeTruthy();
    const hash = await sha256Source(source!);
    expect(iframeCsp).toContain(`script-src '${hash}'`);
    expect(tauriConfig.app.security.csp).toContain(`script-src 'self' '${hash}'`);
  });

  it("handles WebKit text-node links and suppresses the context menu", () => {
    const reader = mountReader();
    const doc = parseIframeDocument(reader);
    const postMessage = runIframeBootstrap(doc);
    const anchor = doc.querySelector<HTMLAnchorElement>('a[href="https://example.org/story"]');
    const text = anchor?.firstChild;

    expect(text?.nodeType).toBe(Node.TEXT_NODE);

    const hover = new MouseEvent("mouseover", { bubbles: true, cancelable: true });
    text!.dispatchEvent(hover);
    expect(postMessage).toHaveBeenLastCalledWith(
      { type: "link-hover", href: "https://example.org/story" },
      "*",
    );

    const click = new MouseEvent("click", { bubbles: true, cancelable: true });
    text!.dispatchEvent(click);
    expect(click.defaultPrevented).toBe(true);
    expect(postMessage).toHaveBeenLastCalledWith(
      { type: "link-click", href: "https://example.org/story" },
      "*",
    );

    const contextMenu = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
    text!.dispatchEvent(contextMenu);
    expect(contextMenu.defaultPrevented).toBe(true);
  });

  it("routes link-click messages from its sandbox to the link popup", () => {
    const reader = mountReader();
    const iframe = reader.get('[data-testid="reader-body-iframe"]')
      .element as HTMLIFrameElement;

    window.dispatchEvent(new MessageEvent("message", {
      data: { type: "link-click", href: "https://example.org/story" },
      source: iframe.contentWindow,
    }));

    expect(useUiStore().linkPopupUrl).toBe("https://example.org/story");
  });

  it("reports initial and delayed body and image height changes", () => {
    const reader = mountReader();
    const srcdoc = reader.get('[data-testid="reader-body-iframe"]').attributes("srcdoc");

    expect(srcdoc).toContain("function reportHeight()");
    expect(srcdoc).toContain("body ? body.scrollHeight : 0");
    expect(srcdoc).toContain("ro.observe(document.body)");
    expect(srcdoc).toContain("img.addEventListener('load', reportHeight)");
    expect(srcdoc).toContain("window.addEventListener('load', reportHeight)");
    expect(srcdoc).toContain("reportHeight();");
  });

  it("applies resize messages only from its own sandbox", () => {
    const reader = mountReader();
    const iframe = reader.get('[data-testid="reader-body-iframe"]')
      .element as HTMLIFrameElement;

    window.dispatchEvent(new MessageEvent("message", {
      data: { type: "resize", height: 640.2 },
      source: window,
    }));
    expect(iframe.style.height).toBe("");

    window.dispatchEvent(new MessageEvent("message", {
      data: { type: "resize", height: 640.2 },
      source: iframe.contentWindow,
    }));
    expect(iframe.style.height).toBe("641px");
  });

  it("starts a fresh sizing lifecycle after remote images are loaded", async () => {
    const reader = mountReader();
    const oldIframe = reader.get('[data-testid="reader-body-iframe"]')
      .element as HTMLIFrameElement;

    await reader.get('[data-testid="reader-load-images"]').trigger("click");
    await flushPromises();

    expect(api.getMessageHtmlWithImages).toHaveBeenCalledWith("acc1", "m1");
    const newIframe = reader.get('[data-testid="reader-body-iframe"]')
      .element as HTMLIFrameElement;
    expect(newIframe).not.toBe(oldIframe);
    expect(newIframe.srcdoc).toContain("https://example.org/banner.png");
    expect(newIframe.srcdoc).toContain("img.addEventListener('load', reportHeight)");
  });
});
