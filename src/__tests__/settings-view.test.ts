import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";

vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  addAccount: vi.fn().mockResolvedValue("new-id"),
  updateAccount: vi.fn().mockResolvedValue(undefined),
  deleteAccount: vi.fn().mockResolvedValue(undefined),
  getAccountConfig: vi.fn(),
  oauthHasTokens: vi.fn().mockResolvedValue(true),
  listContactBooks: vi.fn().mockResolvedValue([]),
  getDefaultContactBook: vi.fn().mockResolvedValue(null),
  setDefaultContactBook: vi.fn().mockResolvedValue(undefined),
  discoverMailServers: vi.fn(),
}));

vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: vi.fn().mockResolvedValue(undefined),
}));

import SettingsView from "@/views/SettingsView.vue";
import { useAccountsStore } from "@/stores/accounts";
import * as api from "@/lib/tauri";
import type { AccountConfig } from "@/lib/types";

function makeRouter() {
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/", component: { template: "<div/>" } },
      { path: "/settings", component: SettingsView },
    ],
  });
}

// Both modals teleport to <body>, so queries go through document.body.
function bodyEl(selector: string): HTMLElement | null {
  return document.body.querySelector(selector);
}

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
});

afterEach(() => {
  document.body.innerHTML = "";
});

const gmailConfig: AccountConfig = {
  display_name: "Work",
  email: "w@example.org",
  provider: "gmail",
  mail_protocol: "imap",
  imap_host: "imap.gmail.com",
  imap_port: 993,
  smtp_host: "smtp.gmail.com",
  smtp_port: 587,
  jmap_url: "",
  caldav_url: "",
  meet_url: "",
  meet_protocol: "",
  username: "w@example.org",
  password: "",
  use_tls: true,
  signature: "",
  jmap_auth_method: "basic",
  oidc_token_endpoint: "",
  oidc_client_id: "",
  calendar_sync_enabled: true,
  mail_sync_enabled: true,
  contacts_sync_enabled: true,
  mail_sync_interval_seconds: null,
  calendar_sync_interval_seconds: null,
  contacts_sync_interval_seconds: null,
  has_calendar_binding: true,
  has_contacts_binding: true,
  pgp_attach_pubkey_on_sign: true,
  pgp_autocrypt_header: true,
  pgp_encrypt_subject: true,
  pgp_encrypt_drafts: true,
};

describe("SettingsView", () => {
  it("picker pick opens the form pre-set to the picked type", async () => {
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });
    await wrapper.find(".btn-add").trigger("click");
    expect(bodyEl('[data-testid="account-type-picker"]')).toBeTruthy();

    bodyEl('[data-testid="picker-imap"]')!.click();
    await flushPromises();

    expect(bodyEl('[data-testid="account-type-picker"]')).toBeNull();
    expect(bodyEl('[data-testid="account-type-readonly"]')!.textContent).toContain("IMAP");
    expect(bodyEl('[data-testid="account-email"]')).toBeTruthy();
  });

  it("editing a gmail account shows the signed-in OAuth status", async () => {
    vi.mocked(api.getAccountConfig).mockResolvedValue(gmailConfig);
    const store = useAccountsStore();
    store.accounts = [
      {
        id: "acc1",
        display_name: "Work",
        email: "w@example.org",
        username: "w@example.org",
        provider: "gmail",
        mail_protocol: "imap",
        enabled: true,
        mail_sync_interval_seconds: null,
        calendar_sync_interval_seconds: null,
        contacts_sync_interval_seconds: null,
        has_calendar_binding: true,
        has_contacts_binding: true,
        meet_protocol: "",
      },
    ];

    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });
    await wrapper.find('[title="Edit"]').trigger("click");
    await flushPromises();

    expect(api.getAccountConfig).toHaveBeenCalledWith("acc1");
    expect(bodyEl('[data-testid="account-type-readonly"]')!.textContent).toContain("Gmail");
    expect(document.body.textContent).toContain("Signed in with Google");
  });

  it("?addAccount deep link skips the picker and opens the form", async () => {
    // Note: the onboarding map deliberately has no "fastmail" entry;
    // only the providers onboarding offers are deep-linkable.
    const router = makeRouter();
    await router.push("/settings?addAccount=jmap");
    await router.isReady();
    mount(SettingsView, {
      global: { plugins: [router] },
      attachTo: document.body,
    });
    await flushPromises();

    expect(bodyEl('[data-testid="account-type-picker"]')).toBeNull();
    expect(bodyEl('[data-testid="account-type-readonly"]')!.textContent).toContain("JMAP");
  });

  it("saves distinct JMAP email and username values", async () => {
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });
    await wrapper.find(".btn-add").trigger("click");
    bodyEl('[data-testid="picker-jmap"]')!.click();
    await flushPromises();

    const email = bodyEl('[data-testid="account-email"]') as HTMLInputElement;
    const username = bodyEl('[data-testid="jmap-username"]') as HTMLInputElement;
    email.value = "user@example.org";
    email.dispatchEvent(new Event("input"));
    username.value = "user";
    username.dispatchEvent(new Event("input"));
    const save = Array.from(document.body.querySelectorAll(".modal-footer button"))
      .find((button) => button.textContent?.includes("Add Account")) as HTMLElement;
    save.click();
    await flushPromises();

    expect(api.addAccount).toHaveBeenCalledWith(expect.objectContaining({
      email: "user@example.org",
      username: "user",
    }));
  });

  it("defaults a blank JMAP username to the email address", async () => {
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });
    await wrapper.find(".btn-add").trigger("click");
    bodyEl('[data-testid="picker-jmap"]')!.click();
    await flushPromises();

    const email = bodyEl('[data-testid="account-email"]') as HTMLInputElement;
    email.value = "user@example.org";
    email.dispatchEvent(new Event("input"));
    const save = Array.from(document.body.querySelectorAll(".modal-footer button"))
      .find((button) => button.textContent?.includes("Add Account")) as HTMLElement;
    save.click();
    await flushPromises();

    expect(api.addAccount).toHaveBeenCalledWith(expect.objectContaining({
      email: "user@example.org",
      username: "user@example.org",
    }));
  });

  it("fastmail save without an API token shows an error and does not save", async () => {
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });
    await wrapper.find(".btn-add").trigger("click");
    bodyEl('[data-testid="picker-fastmail"]')!.click();
    await flushPromises();

    const buttons = Array.from(document.body.querySelectorAll(".modal-footer button"));
    const save = buttons.find((b) => b.textContent?.includes("Add Account")) as HTMLElement;
    save.click();
    await flushPromises();

    expect(document.body.querySelector(".form-error")!.textContent).toContain("API token");
    expect(api.addAccount).not.toHaveBeenCalled();
  });
});
