// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount, type VueWrapper } from "@vue/test-utils";
import { defineComponent } from "vue";
import { createPinia, setActivePinia } from "pinia";
import { CloseRequestedEvent, type Window as TauriWindow } from "@tauri-apps/api/window";
import type { ask } from "@tauri-apps/plugin-dialog";
import type { LocationQuery } from "vue-router";
import type { usePgpPromptsStore } from "@/stores/pgp-prompts";
import type {
  Account,
  AccountConfig,
  ComposeMessage,
  Contact,
  MessageBody,
  PgpRecipientStatus,
} from "@/lib/types";

type Api = typeof import("@/lib/tauri");
type CloseHandler = Parameters<TauriWindow["onCloseRequested"]>[0];
type PgpPromptsStore = ReturnType<typeof usePgpPromptsStore>;
type RecipientField = "to" | "cc" | "bcc";

const native = vi.hoisted(() => ({
  query: {} as LocationQuery,
  closeHandler: undefined as CloseHandler | undefined,
  window: {
    close: vi.fn<TauriWindow["close"]>(),
    destroy: vi.fn<TauriWindow["destroy"]>(),
    onCloseRequested: vi.fn<TauriWindow["onCloseRequested"]>(),
  },
  unlisten: vi.fn<() => void>(),
  ask: vi.fn<typeof ask>(),
  pgpStart: vi.fn<PgpPromptsStore["start"]>(),
  pgpStop: vi.fn<PgpPromptsStore["stop"]>(),
}));

vi.mock("vue-router", () => ({
  useRoute: () => ({ query: native.query }),
}));
vi.mock("@tauri-apps/api/window", async (importOriginal) => ({
  ...await importOriginal<typeof import("@tauri-apps/api/window")>(),
  getCurrentWindow: () => native.window,
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ ask: native.ask }));
vi.mock("@/stores/pgp-prompts", () => ({
  usePgpPromptsStore: () => ({
    start: native.pgpStart,
    stop: native.pgpStop,
  }),
}));
vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn<Api["listAccounts"]>(),
  getAccountConfig: vi.fn<Api["getAccountConfig"]>(),
  getMessageBody: vi.fn<Api["getMessageBody"]>(),
  sendMessage: vi.fn<Api["sendMessage"]>(),
  saveDraft: vi.fn<Api["saveDraft"]>(),
  triggerSync: vi.fn<Api["triggerSync"]>(),
  setMessageFlags: vi.fn<Api["setMessageFlags"]>(),
  pickAttachments: vi.fn<Api["pickAttachments"]>(),
  releaseAttachment: vi.fn<Api["releaseAttachment"]>(),
  searchContacts: vi.fn<Api["searchContacts"]>(),
  searchContactsForAccount: vi.fn<Api["searchContactsForAccount"]>(),
  searchCollectedContacts: vi.fn<Api["searchCollectedContacts"]>(),
  pgpListKeys: vi.fn<Api["pgpListKeys"]>(),
  pgpCheckRecipients: vi.fn<Api["pgpCheckRecipients"]>(),
  pgpDecryptMessage: vi.fn<Api["pgpDecryptMessage"]>(),
}));

import ComposeView from "@/views/ComposeView.vue";
import { useAccountsStore } from "@/stores/accounts";
import * as api from "@/lib/tauri";

const ComposeMenuBarStub = defineComponent({
  name: "ComposeMenuBar",
  props: { showCc: Boolean, showBcc: Boolean },
  emits: ["send", "saveDraft", "toggleCc", "toggleBcc"],
  template: `
    <nav>
      <button data-testid="menu-send" @click="$emit('send')">Send</button>
      <button data-testid="menu-save-draft" @click="$emit('saveDraft')">
        Save Draft
      </button>
      <button data-testid="menu-toggle-cc" @click="$emit('toggleCc')">Cc</button>
      <button data-testid="menu-toggle-bcc" @click="$emit('toggleBcc')">Bcc</button>
    </nav>
  `,
});

const account: Account = {
  id: "acc-compose",
  display_name: "Compose account",
  email: "sender@example.com",
  username: "sender@example.com",
  provider: "generic",
  mail_protocol: "imap",
  enabled: true,
  mail_sync_interval_seconds: null,
  calendar_sync_interval_seconds: null,
  contacts_sync_interval_seconds: null,
  has_calendar_binding: false,
  has_contacts_binding: false,
  meet_protocol: "",
};

const accountConfig: AccountConfig = {
  display_name: account.display_name,
  sender_name: "Sender",
  email: account.email,
  username: account.username,
  provider: "generic",
  mail_protocol: "imap",
  imap_host: "imap.example.com",
  imap_port: 993,
  smtp_host: "smtp.example.com",
  smtp_port: 465,
  jmap_url: "",
  caldav_url: "",
  meet_url: "",
  meet_protocol: "",
  password: "",
  use_tls: true,
  signature: "",
  jmap_auth_method: "basic",
  oidc_token_endpoint: "",
  oidc_client_id: "",
  calendar_sync_enabled: false,
  mail_sync_enabled: true,
  contacts_sync_enabled: false,
  mail_sync_interval_seconds: null,
  calendar_sync_interval_seconds: null,
  contacts_sync_interval_seconds: null,
  has_calendar_binding: false,
  has_contacts_binding: false,
  pgp_attach_pubkey_on_sign: true,
  pgp_autocrypt_header: true,
  pgp_encrypt_subject: true,
  pgp_encrypt_drafts: true,
};

const contact: Contact = {
  id: "contact-west",
  book_id: "book-compose",
  uid: null,
  display_name: String.raw`West, "Team" \ Ops`,
  emails_json: JSON.stringify([{ email: "west@example.com", label: "work" }]),
  phones_json: "[]",
  addresses_json: "[]",
  organization: null,
  title: null,
  notes: null,
  vcard_data: null,
  remote_id: null,
  etag: null,
};

const draft: MessageBody = {
  id: "draft-compose",
  subject: "Saved subject",
  from: { name: "Sender", email: account.email },
  to: [
    { name: contact.display_name, email: "west@example.com" },
    { name: null, email: "bare@example.com" },
  ],
  cc: [{ name: 'Copy; "Board"', email: "copy@example.com" }],
  date: "2026-09-12T12:00:00Z",
  flags: ["draft"],
  body_html: null,
  body_text: "Saved body",
  attachments: [],
  is_encrypted: false,
  is_signed: false,
  list_id: null,
  has_remote_images: false,
};

const recipientText: Record<RecipientField, string> = {
  to: String.raw`  "Doe, \"Jane\" \\ Ops" <jane@example.com>; bare@example.com, `,
  cc: String.raw`"team; \"west\""@example.com,  Plain Person <plain@example.com>;`,
  bcc: String.raw`"Hidden; Copy" <hidden@example.com>; "comma,quote\"slash\\"@example.com,`,
};

const recipientArrays: Pick<ComposeMessage, RecipientField> = {
  to: ["jane@example.com", "bare@example.com"],
  cc: [String.raw`"team; \"west\""@example.com`, "plain@example.com"],
  bcc: ["hidden@example.com", String.raw`"comma,quote\"slash\\"@example.com`],
};
const allRecipients = [
  ...recipientArrays.to,
  ...recipientArrays.cc,
  ...recipientArrays.bcc,
];
const recipientFields: RecipientField[] = ["to", "cc", "bcc"];
const wrappers: VueWrapper[] = [];
const settlePending: Array<() => void> = [];

function deferred<T>(cleanupValue: T) {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  // Also settle requests when an assertion fails before their explicit resolution.
  settlePending.push(() => resolve(cleanupValue));
  return { promise, resolve };
}

function keyStatuses(emails: string[], hasKey = true): PgpRecipientStatus[] {
  return emails.map((email) => ({
    email,
    hasKey,
    fingerprint: hasKey ? "AAAA1111BBBB2222CCCC3333DDDD4444EEEE5555" : null,
  }));
}

async function mountCompose(query: LocationQuery = {}) {
  native.query = { accountId: account.id, ...query };
  const pinia = createPinia();
  setActivePinia(pinia);
  const accounts = useAccountsStore();
  accounts.accounts = [{ ...account }];
  accounts.activeAccountId = account.id;
  const wrapper = mount(ComposeView, {
    attachTo: document.body,
    global: { plugins: [pinia], stubs: { ComposeMenuBar: ComposeMenuBarStub } },
  });
  wrappers.push(wrapper);
  await flushPromises();
  expect(native.pgpStart).toHaveBeenCalled();
  expect(native.closeHandler).toBeDefined();
  return wrapper;
}

function recipientInput(wrapper: VueWrapper, field: RecipientField) {
  return wrapper.get<HTMLInputElement>(`[data-testid="compose-${field}"]`);
}

async function setRecipient(wrapper: VueWrapper, field: RecipientField, value: string) {
  if (!wrapper.find(`[data-testid="compose-${field}"]`).exists()) {
    await wrapper.get(`[data-testid="compose-${field}-toggle"]`).trigger("click");
  }
  await recipientInput(wrapper, field).setValue(value);
}

async function fillRecipients(wrapper: VueWrapper) {
  for (const field of recipientFields) {
    await setRecipient(wrapper, field, recipientText[field]);
  }
}

async function settleDebounces() {
  await vi.advanceTimersByTimeAsync(350);
  await flushPromises();
}

function expectRecipientError(
  wrapper: VueWrapper,
  field: RecipientField,
  index: number,
  reason: RegExp,
) {
  const banner = wrapper.get('[data-testid="compose-error"]');
  expect(banner.attributes("role")).toBe("alert");
  expect(banner.text()).toMatch(new RegExp(`^${field} recipient ${index}:\\s*\\S`, "i"));
  expect(banner.text()).toMatch(reason);
}

async function requestClose() {
  if (!native.closeHandler) throw new Error("Compose did not register its close handler");
  const event = new CloseRequestedEvent({ event: "tauri://close-requested", id: 1, payload: null });
  const preventDefault = vi.spyOn(event, "preventDefault");
  await native.closeHandler(event);
  await flushPromises();
  return preventDefault;
}

beforeEach(() => {
  vi.resetAllMocks();
  // Keep flushPromises' scheduler real while controlling both compose debounces.
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  native.query = {};
  native.closeHandler = undefined;
  native.window.close.mockResolvedValue(undefined);
  native.window.destroy.mockResolvedValue(undefined);
  native.window.onCloseRequested.mockImplementation(async (handler) => {
    native.closeHandler = handler;
    return native.unlisten;
  });
  native.ask.mockResolvedValue(true);
  native.pgpStart.mockResolvedValue(undefined);
  vi.mocked(api.listAccounts).mockResolvedValue([account]);
  vi.mocked(api.getAccountConfig).mockResolvedValue(accountConfig);
  vi.mocked(api.getMessageBody).mockResolvedValue(draft);
  vi.mocked(api.sendMessage).mockResolvedValue(undefined);
  vi.mocked(api.saveDraft).mockResolvedValue({ plaintext_fallback: false });
  vi.mocked(api.triggerSync).mockResolvedValue(undefined);
  vi.mocked(api.setMessageFlags).mockResolvedValue(undefined);
  vi.mocked(api.pickAttachments).mockResolvedValue([]);
  vi.mocked(api.releaseAttachment).mockResolvedValue(undefined);
  vi.mocked(api.searchContacts).mockResolvedValue([]);
  vi.mocked(api.searchContactsForAccount).mockResolvedValue([]);
  vi.mocked(api.searchCollectedContacts).mockResolvedValue([]);
  vi.mocked(api.pgpListKeys).mockResolvedValue([]);
  vi.mocked(api.pgpCheckRecipients).mockImplementation(async (emails) => keyStatuses(emails));
  vi.mocked(api.pgpDecryptMessage).mockResolvedValue({
    plaintextBody: draft,
    verifyOutcome: { kind: "unsigned" },
  });
});

afterEach(async () => {
  for (const settle of settlePending.splice(0)) settle();
  await flushPromises();
  for (const wrapper of wrappers.splice(0)) wrapper.unmount();
  vi.clearAllTimers();
  await flushPromises();
  vi.clearAllTimers();
  native.closeHandler = undefined;
  document.body.replaceChildren();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("ComposeView recipient validation", () => {
  it("sends exact bare To/Cc/Bcc arrays with quoted names, escaped local parts and mixed separators", async () => {
    const wrapper = await mountCompose();
    await fillRecipients(wrapper);
    await wrapper.get('[data-testid="compose-send"]').trigger("click");
    await flushPromises();

    expect(api.sendMessage).toHaveBeenCalledTimes(1);
    expect(api.sendMessage).toHaveBeenCalledWith(
      account.id,
      expect.objectContaining(recipientArrays),
    );
    expect(native.window.close).toHaveBeenCalledTimes(1);
  });

  const invalidFields: Array<{
    field: RecipientField;
    input: string;
    index: number;
    reason: RegExp;
  }> = [
    {
      field: "to",
      input: ',; first@example.com,, "Valid, Name" <second@example.com>; , broken@',
      index: 3,
      reason: /domain/i,
    },
    {
      field: "cc",
      input: 'first@example.com; , "Unclosed, Name <second@example.com>',
      index: 2,
      reason: /quote/i,
    },
    {
      field: "bcc",
      input: 'first@example.com,,; Other <second@example.com> trailing',
      index: 2,
      reason: /after|trailing/i,
    },
  ];

  describe.each(["send", "save-draft"] as const)("menu %s", (action) => {
    it.each(invalidFields)("rejects malformed $field, preserves text and reveals/focuses the field", async ({ field, input, index, reason }) => {
      const wrapper = await mountCompose({ to: "primary@example.com" });
      await setRecipient(wrapper, field, input);
      if (field !== "to") {
        await wrapper.get(`[data-testid="menu-toggle-${field}"]`).trigger("click");
        expect(wrapper.find(`[data-testid="compose-${field}"]`).exists()).toBe(false);
      }
      await wrapper.get('[data-testid="compose-body"]').setValue("Please see the attachment.");
      await wrapper.get('[data-testid="compose-pgp-encrypt"]').trigger("click");
      await wrapper.get(`[data-testid="menu-${action}"]`).trigger("click");
      await flushPromises();

      expectRecipientError(wrapper, field, index, reason);
      expect(recipientInput(wrapper, field).element.value).toBe(input);
      expect(document.activeElement).toBe(recipientInput(wrapper, field).element);
      await settleDebounces();
      expect(api.sendMessage).not.toHaveBeenCalled();
      expect(api.saveDraft).not.toHaveBeenCalled();
      expect(api.triggerSync).not.toHaveBeenCalled();
      expect(api.pgpCheckRecipients).not.toHaveBeenCalled();
      expect(api.pickAttachments).not.toHaveBeenCalled();
      expect(native.ask).not.toHaveBeenCalled();
      expect(native.window.close).not.toHaveBeenCalled();
      expect(native.window.destroy).not.toHaveBeenCalled();
    });
  });

  it("still requires To when the menu sends past the disabled toolbar with Cc/Bcc populated", async () => {
    const wrapper = await mountCompose();
    await setRecipient(wrapper, "cc", "copy@example.com");
    await setRecipient(wrapper, "bcc", "hidden@example.com");
    expect(wrapper.get<HTMLButtonElement>('[data-testid="compose-send"]').element.disabled).toBe(true);
    await wrapper.get('[data-testid="menu-send"]').trigger("click");
    await flushPromises();

    const banner = wrapper.get('[data-testid="compose-error"]');
    expect(banner.attributes("role")).toBe("alert");
    expect(banner.text()).toMatch(/recipient|\bto\b/i);
    expect(api.sendMessage).not.toHaveBeenCalled();
    expect(native.window.close).not.toHaveBeenCalled();
  });

  it("allows a draft with all recipient fields empty", async () => {
    const wrapper = await mountCompose();
    await wrapper.get('[data-testid="compose-body"]').setValue("Work in progress");
    await wrapper.get('[data-testid="compose-save-draft"]').trigger("click");
    await flushPromises();

    expect(api.saveDraft).toHaveBeenCalledTimes(1);
    expect(api.saveDraft).toHaveBeenCalledWith(account.id, expect.objectContaining({
      to: [], cc: [], bcc: [], body_text: "Work in progress",
    }));
    expect(wrapper.find('[data-testid="compose-error"]').exists()).toBe(false);
    expect(native.window.close).not.toHaveBeenCalled();
  });

  it("saves the same quoted-aware bare arrays through the menu", async () => {
    const wrapper = await mountCompose();
    await fillRecipients(wrapper);
    await wrapper.get('[data-testid="menu-save-draft"]').trigger("click");
    await flushPromises();

    expect(api.saveDraft).toHaveBeenCalledTimes(1);
    expect(api.saveDraft).toHaveBeenCalledWith(
      account.id,
      expect.objectContaining(recipientArrays),
    );
    for (const field of recipientFields) {
      expect(recipientInput(wrapper, field).element.value).toBe(recipientText[field]);
    }
  });

  it("keeps an invalid save-on-close open and dirty on the next close request", async () => {
    const wrapper = await mountCompose();
    const input = "valid@example.com; broken@";
    await recipientInput(wrapper, "to").setValue(input);

    expect(await requestClose()).toHaveBeenCalledTimes(1);
    expectRecipientError(wrapper, "to", 2, /domain/i);
    expect(await requestClose()).toHaveBeenCalledTimes(1);
    expect(native.ask).toHaveBeenCalledTimes(2);
    expect(native.ask).toHaveBeenNthCalledWith(2, expect.any(String), expect.objectContaining({
      okLabel: "Save Draft",
    }));
    expect(api.saveDraft).not.toHaveBeenCalled();
    expect(api.triggerSync).not.toHaveBeenCalled();
    expect(native.window.close).not.toHaveBeenCalled();
    expect(native.window.destroy).not.toHaveBeenCalled();
    expect(recipientInput(wrapper, "to").element.value).toBe(input);
  });

  it("keeps the composer and recipient text after a normal send failure", async () => {
    vi.mocked(api.sendMessage).mockRejectedValueOnce(new Error("Network unavailable"));
    const wrapper = await mountCompose();
    await fillRecipients(wrapper);
    await wrapper.get('[data-testid="compose-send"]').trigger("click");
    await flushPromises();

    expect(api.sendMessage).toHaveBeenCalledWith(account.id, expect.objectContaining(recipientArrays));
    expect(wrapper.get('[data-testid="compose-error"]').text()).toMatch(/network unavailable/i);
    expect(native.window.close).not.toHaveBeenCalled();
    expect(native.window.destroy).not.toHaveBeenCalled();
    for (const field of recipientFields) {
      expect(recipientInput(wrapper, field).element.value).toBe(recipientText[field]);
    }
  });

  it("does not discard recipient edits made while save-on-close is pending", async () => {
    const pending = deferred<Awaited<ReturnType<Api["saveDraft"]>>>({ plaintext_fallback: false });
    vi.mocked(api.saveDraft).mockReturnValueOnce(pending.promise);
    const wrapper = await mountCompose();
    await recipientInput(wrapper, "to").setValue("first@example.com");
    const closing = requestClose();
    await flushPromises();
    expect(api.saveDraft).toHaveBeenCalledWith(account.id, expect.objectContaining({
      to: ["first@example.com"],
    }));
    await recipientInput(wrapper, "to").setValue('first@example.com, "Unclosed');
    pending.resolve({ plaintext_fallback: false });
    await closing;
    expect(native.window.destroy).not.toHaveBeenCalled();
    expect(recipientInput(wrapper, "to").element.value).toBe('first@example.com, "Unclosed');
    await requestClose();
    expectRecipientError(wrapper, "to", 2, /quote/i);
    expect(api.saveDraft).toHaveBeenCalledTimes(1);
  });

  it("does not send after unmount while the attachment prompt is pending", async () => {
    const pending = deferred<boolean>(false);
    native.ask.mockReturnValueOnce(pending.promise);
    const wrapper = await mountCompose({ to: "recipient@example.com" });
    await wrapper.get('[data-testid="compose-body"]').setValue("Please see the attachment.");
    await wrapper.get('[data-testid="compose-send"]').trigger("click");
    await flushPromises();
    expect(native.ask).toHaveBeenCalledTimes(1);
    wrapper.unmount();
    wrappers.splice(wrappers.indexOf(wrapper), 1);
    pending.resolve(true);
    await flushPromises();
    expect(api.sendMessage).not.toHaveBeenCalled();
  });
});

describe("ComposeView recipient formatting", () => {
  it("does not expand a malformed contact email into multiple recipients", async () => {
    const invalidEmail = "bad@example.com, extra@example.com";
    vi.mocked(api.searchContactsForAccount).mockResolvedValue([{
      ...contact,
      display_name: invalidEmail,
      emails_json: JSON.stringify([{ email: invalidEmail, label: "work" }]),
    }]);
    const wrapper = await mountCompose();
    const input = recipientInput(wrapper, "to");
    await input.setValue("bad");
    await settleDebounces();
    await wrapper.get('[data-testid="compose-ac-item"]').trigger("mousedown");
    expect(input.element.value).toBe("bad");
    expect(wrapper.get('[data-testid="compose-error"]').text()).toMatch(/single.*address/i);
    await wrapper.get('[data-testid="menu-send"]').trigger("click");
    await flushPromises();
    expect(api.sendMessage).not.toHaveBeenCalled();
  });

  const multiEmailContact: Contact = {
    ...contact,
    display_name: "Alice",
    emails_json: JSON.stringify([
      { email: "home@example.com", label: "home" },
      { email: "Work@example.com", label: "case-distinct" },
      { email: "other-work@example.com", label: "alias" },
      { email: "work@example.com", label: "work" },
    ]),
  };

  it("does not autocomplete an unchanged complete recipient on focus and Tab", async () => {
    vi.mocked(api.searchContactsForAccount).mockResolvedValue([multiEmailContact]);
    const text = "Alice <work@example.com>";
    const wrapper = await mountCompose({ to: text });
    const input = recipientInput(wrapper, "to");
    await input.trigger("focus");
    await settleDebounces();
    await input.trigger("keydown", { key: "Tab" });
    expect(input.element.value).toBe(text);
    expect(api.searchContactsForAccount).not.toHaveBeenCalled();
  });

  it.each(["work@example.com", "work@EXAMPLE.com", "work@EXAM"])("keeps the intended local part when completing %s", async (email) => {
    vi.mocked(api.searchContactsForAccount).mockResolvedValue([multiEmailContact]);
    const wrapper = await mountCompose();
    const input = recipientInput(wrapper, "to");
    await input.setValue(`Alice <${email}>`);
    await settleDebounces();
    expect(api.searchContactsForAccount).toHaveBeenLastCalledWith(email, account.id, "mail");
    const results = wrapper.findAll('[data-testid="compose-ac-item"]');
    expect(results.map((result) => result.get(".ac-email").text())).toEqual([
      "<work@example.com>", "<other-work@example.com>", "<Work@example.com>",
    ]);
    await input.trigger("keydown", { key: "Tab" });
    expect(input.element.value).toBe("Alice <work@example.com>, ");
    await wrapper.get('[data-testid="compose-send"]').trigger("click");
    await flushPromises();
    expect(api.sendMessage).toHaveBeenCalledWith(account.id, expect.objectContaining({
      to: ["work@example.com"],
    }));
  });

  it.each(recipientFields)("autocompletes the protected last %s token without rewriting its prefix", async (field) => {
    vi.mocked(api.searchContactsForAccount).mockImplementation(async (query) =>
      contact.display_name.toLowerCase().includes(query.toLowerCase()) ? [contact] : [],
    );
    const wrapper = await mountCompose({ to: "primary@example.com" });
    const prefix = String.raw`  "Doe, \"Jane\" \\ Ops" <jane@example.com>;  bare@example.com,   `;
    const term = String.raw`"West, \"Te`;
    await setRecipient(wrapper, field, prefix + term);
    await settleDebounces();

    expect(api.searchContactsForAccount).toHaveBeenLastCalledWith('West, "Te', account.id, "mail");
    expect(api.searchCollectedContacts).toHaveBeenLastCalledWith('West, "Te');
    const item = wrapper.get('[data-testid="compose-ac-item"]');
    expect(item.text()).toContain(contact.display_name);
    await item.trigger("mousedown");
    expect(recipientInput(wrapper, field).element.value).toBe(
      prefix + String.raw`"West, \"Team\" \\ Ops" <west@example.com>, `,
    );

    await wrapper.get('[data-testid="compose-send"]').trigger("click");
    await flushPromises();
    expect(api.sendMessage).toHaveBeenCalledTimes(1);
    expect(api.sendMessage).toHaveBeenCalledWith(account.id, expect.objectContaining({
      to: ["primary@example.com"],
      cc: [],
      bcc: [],
      [field]: ["jane@example.com", "bare@example.com", "west@example.com"],
    }));
  });

  it("quotes and escapes resumed draft names and saves their original address arrays", async () => {
    const wrapper = await mountCompose({ draftId: draft.id });

    expect(api.getMessageBody).toHaveBeenCalledWith(account.id, draft.id);
    expect(recipientInput(wrapper, "to").element.value).toBe(
      String.raw`"West, \"Team\" \\ Ops" <west@example.com>, bare@example.com`,
    );
    expect(recipientInput(wrapper, "cc").element.value).toBe(
      String.raw`"Copy; \"Board\"" <copy@example.com>`,
    );
    await wrapper.get('[data-testid="compose-save-draft"]').trigger("click");
    await flushPromises();

    expect(api.saveDraft).toHaveBeenCalledTimes(1);
    expect(api.saveDraft).toHaveBeenCalledWith(account.id, expect.objectContaining({
      to: ["west@example.com", "bare@example.com"],
      cc: ["copy@example.com"],
      bcc: [],
      subject: draft.subject,
      body_text: draft.body_text,
    }));
  });

  it("shows prefilled Bcc so it can be corrected", async () => {
    const wrapper = await mountCompose({ bcc: recipientText.bcc });
    expect(recipientInput(wrapper, "bcc").element.value).toBe(recipientText.bcc);
  });

  it("normalizes tab-folded draft display names without changing the mailbox", async () => {
    vi.mocked(api.getMessageBody).mockResolvedValueOnce({
      ...draft,
      to: [{ name: "Alice\tSmith", email: "alice@example.com" }],
      cc: [],
    });
    const wrapper = await mountCompose({ draftId: draft.id });
    expect(recipientInput(wrapper, "to").element.value).toBe("Alice Smith <alice@example.com>");
    await wrapper.get('[data-testid="compose-save-draft"]').trigger("click");
    await flushPromises();
    expect(api.saveDraft).toHaveBeenCalledWith(account.id, expect.objectContaining({
      to: ["alice@example.com"],
    }));
  });
});

describe("ComposeView PGP recipient snapshots", () => {
  it("uses the same bare arrays for debounced key lookup, explicit preflight and encrypted send", async () => {
    const wrapper = await mountCompose();
    await fillRecipients(wrapper);
    await wrapper.get('[data-testid="compose-pgp-encrypt"]').trigger("click");
    await settleDebounces();
    expect(api.pgpCheckRecipients).toHaveBeenLastCalledWith(allRecipients);

    vi.mocked(api.pgpCheckRecipients).mockClear();
    await wrapper.get('[data-testid="compose-send"]').trigger("click");
    await flushPromises();

    expect(api.pgpCheckRecipients).toHaveBeenCalledWith(allRecipients);
    expect(api.sendMessage).toHaveBeenCalledTimes(1);
    expect(api.sendMessage).toHaveBeenCalledWith(account.id, expect.objectContaining({
      ...recipientArrays,
      pgp_encrypt: true,
    }));
  });

  it.each(["invalid edit", "empty field", "encryption off"] as const)(
    "clears missing-key status and ignores an in-flight result after %s",
    async (change) => {
      vi.mocked(api.pgpCheckRecipients).mockResolvedValueOnce(
        keyStatuses(["first@example.com"], false),
      );
      const wrapper = await mountCompose({ to: "first@example.com" });
      const encrypt = wrapper.get('[data-testid="compose-pgp-encrypt"]');
      await encrypt.trigger("click");
      await settleDebounces();
      expect(wrapper.find(".pgp-missing-badge").exists()).toBe(true);

      const pending = deferred<PgpRecipientStatus[]>([]);
      vi.mocked(api.pgpCheckRecipients).mockReturnValueOnce(pending.promise);
      await recipientInput(wrapper, "to").setValue("next@example.com");
      await settleDebounces();
      expect(api.pgpCheckRecipients).toHaveBeenLastCalledWith(["next@example.com"]);
      const callsBeforeChange = vi.mocked(api.pgpCheckRecipients).mock.calls.length;

      if (change === "encryption off") {
        await encrypt.trigger("click");
      } else {
        await recipientInput(wrapper, "to").setValue(
          change === "empty field" ? "" : 'next@example.com; "Unclosed',
        );
      }
      expect(wrapper.find(".pgp-missing-badge").exists()).toBe(false);
      expect(encrypt.classes()).not.toContain("warn");
      pending.resolve(keyStatuses(["next@example.com"], false));
      await flushPromises();
      expect(wrapper.find(".pgp-missing-badge").exists()).toBe(false);

      if (change === "encryption off") {
        // Re-enable before the debounce: hidden stale state must not reappear.
        const fresh = deferred<PgpRecipientStatus[]>([]);
        vi.mocked(api.pgpCheckRecipients).mockReturnValueOnce(fresh.promise);
        await encrypt.trigger("click");
        expect(wrapper.find(".pgp-missing-badge").exists()).toBe(false);
        await settleDebounces();
        expect(wrapper.find(".pgp-missing-badge").exists()).toBe(false);
        fresh.resolve(keyStatuses(["next@example.com"]));
        await flushPromises();
      } else {
        await settleDebounces();
        expect(api.pgpCheckRecipients).toHaveBeenCalledTimes(callsBeforeChange);
      }
      expect(wrapper.find(".pgp-missing-badge").exists()).toBe(false);
      expect(encrypt.classes()).not.toContain("warn");
      expect(api.sendMessage).not.toHaveBeenCalled();
    },
  );

  it("fails closed when the explicit encrypted-send key check rejects", async () => {
    vi.mocked(api.pgpCheckRecipients).mockRejectedValueOnce(new Error("Key lookup offline"));
    const wrapper = await mountCompose({ to: "recipient@example.com" });
    await wrapper.get('[data-testid="compose-pgp-encrypt"]').trigger("click");
    await wrapper.get('[data-testid="menu-send"]').trigger("click");
    await flushPromises();

    expect(api.pgpCheckRecipients).toHaveBeenCalledWith(["recipient@example.com"]);
    expect(api.sendMessage).not.toHaveBeenCalled();
    const banner = wrapper.get('[data-testid="compose-error"]');
    expect(banner.attributes("role")).toBe("alert");
    expect(banner.text()).toMatch(/key|encrypt|check/i);
    expect(native.window.close).not.toHaveBeenCalled();
  });

  it("does not send a mixture of old and edited recipients when encrypted preflight becomes stale", async () => {
    const pending = deferred<PgpRecipientStatus[]>([]);
    vi.mocked(api.pgpCheckRecipients).mockReturnValueOnce(pending.promise);
    const wrapper = await mountCompose();
    await fillRecipients(wrapper);
    await wrapper.get('[data-testid="compose-pgp-encrypt"]').trigger("click");
    await wrapper.get('[data-testid="menu-send"]').trigger("click");
    await flushPromises();
    expect(api.pgpCheckRecipients).toHaveBeenCalledWith(allRecipients);

    await recipientInput(wrapper, "cc").setValue('"New, Copy" <new-copy@example.com>');
    pending.resolve(keyStatuses(allRecipients));
    await flushPromises();

    expect(api.sendMessage).not.toHaveBeenCalled();
    const banner = wrapper.get('[data-testid="compose-error"]');
    expect(banner.attributes("role")).toBe("alert");
    expect(banner.text()).toMatch(/chang|stale|again/i);
    expect(recipientInput(wrapper, "cc").element.value).toBe('"New, Copy" <new-copy@example.com>');
    await settleDebounces();
    expect(api.sendMessage).not.toHaveBeenCalled();
    expect(native.window.close).not.toHaveBeenCalled();
  });
});
