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

const message: MessageBody = {
  id: "m1",
  subject: "Newsletter",
  from: { email: "news@example.org", name: "News" },
  to: [{ email: "reader@example.org", name: "Reader" }],
  cc: [],
  date: "2026-08-24T10:00:00Z",
  flags: [],
  body_html: '<table style="height: 900px"><tr><td>Newsletter</td></tr></table>',
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
