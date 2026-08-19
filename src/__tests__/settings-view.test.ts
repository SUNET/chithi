import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";

vi.mock("@/lib/tauri", () => ({
  listAccounts: vi.fn().mockResolvedValue([]),
  getLastView: vi.fn().mockResolvedValue({
    account_id: null,
    folder_path: null,
  }),
  addAccount: vi.fn().mockResolvedValue("new-id"),
  updateAccount: vi.fn().mockResolvedValue(undefined),
  deleteAccount: vi.fn().mockResolvedValue(undefined),
  abandonZoomAccount: vi.fn().mockResolvedValue(undefined),
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
import { usePlatformStore } from "@/stores/platform";
import * as api from "@/lib/tauri";
import type { Account, AccountConfig } from "@/lib/types";

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
  vi.mocked(api.getAccountConfig).mockRejectedValue(
    new Error("config unavailable"),
  );
  vi.mocked(api.listAccounts).mockResolvedValue([]);
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

const zoomConfig: AccountConfig = {
  ...gmailConfig,
  display_name: "Work Zoom",
  email: "",
  provider: "generic",
  mail_protocol: "",
  imap_host: "",
  imap_port: 0,
  smtp_host: "",
  smtp_port: 0,
  meet_protocol: "zoom",
  username: "zoom-user",
  calendar_sync_enabled: false,
  mail_sync_enabled: false,
  contacts_sync_enabled: false,
  has_calendar_binding: false,
  has_contacts_binding: false,
};

function zoomAccount(id = "zoom-1"): Account {
  return {
    id,
    display_name: "Work Zoom",
    email: "",
    username: "zoom-user",
    provider: "generic",
    mail_protocol: "",
    enabled: true,
    mail_sync_interval_seconds: null,
    calendar_sync_interval_seconds: null,
    contacts_sync_interval_seconds: null,
    has_calendar_binding: false,
    has_contacts_binding: false,
    meet_protocol: "zoom",
  };
}

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

  it("keeps normal deletion as the default for a Zoom account", async () => {
    const store = useAccountsStore();
    store.accounts = [zoomAccount()];
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });

    await wrapper.find('[title="Delete"]').trigger("click");
    await flushPromises();

    expect(bodyEl('[data-testid="zoom-abandon-warning"]')?.textContent)
      .toContain("Remote Zoom meetings may remain");
    const checkbox = bodyEl(
      '[data-testid="zoom-abandon-checkbox"]',
    ) as HTMLInputElement;
    expect(checkbox.checked).toBe(false);
    expect(bodyEl('[data-testid="delete-account-confirm"]')?.textContent)
      .toContain("Delete");

    bodyEl('[data-testid="delete-account-confirm"]')!.click();
    await flushPromises();

    expect(api.deleteAccount).toHaveBeenCalledWith("zoom-1");
    expect(api.abandonZoomAccount).not.toHaveBeenCalled();
  });

  it("does not offer local abandonment for non-Zoom accounts", async () => {
    const store = useAccountsStore();
    store.accounts = [{
      ...zoomAccount("talk-1"),
      display_name: "Work Talk",
      meet_protocol: "talk",
    }];
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });

    await wrapper.find('[title="Delete"]').trigger("click");
    await flushPromises();

    expect(bodyEl('[data-testid="zoom-abandon-warning"]')).toBeNull();
    expect(bodyEl('[data-testid="zoom-abandon-checkbox"]')).toBeNull();
  });

  it("uses account config to detect Zoom when the summary binding is disabled", async () => {
    const store = useAccountsStore();
    store.accounts = [{
      ...zoomAccount(),
      meet_protocol: "",
    }];
    vi.mocked(api.getAccountConfig).mockResolvedValue(zoomConfig);
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });

    await wrapper.find('[title="Delete"]').trigger("click");
    await flushPromises();

    expect(api.getAccountConfig).toHaveBeenCalledWith("zoom-1");
    expect(bodyEl('[data-testid="zoom-abandon-warning"]')).not.toBeNull();
  });

  it("offers separate accessible edit and Zoom abandonment actions on mobile", async () => {
    const store = useAccountsStore();
    store.accounts = [zoomAccount()];
    usePlatformStore().width = 600;
    vi.mocked(api.getAccountConfig).mockResolvedValue(zoomConfig);
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });

    const edit = wrapper.get('[data-testid="mobile-account-edit"]');
    const remove = wrapper.get('[data-testid="mobile-account-delete"]');
    expect(edit.attributes("aria-label")).toBe("Edit Work Zoom");
    expect(remove.attributes("aria-label")).toBe("Delete Work Zoom");
    expect(edit.element.parentElement).toBe(remove.element.parentElement);
    expect(edit.find("button").exists()).toBe(false);

    await edit.trigger("click");
    await flushPromises();
    expect(api.getAccountConfig).toHaveBeenCalledWith("zoom-1");
    bodyEl(".modal-close")!.click();
    await flushPromises();

    await remove.trigger("click");
    await flushPromises();
    bodyEl('[data-testid="zoom-abandon-checkbox"]')!.click();
    await flushPromises();
    bodyEl('[data-testid="delete-account-confirm"]')!.click();
    await flushPromises();

    expect(api.abandonZoomAccount).toHaveBeenCalledWith(
      "zoom-1",
      "ABANDON REMOTE ZOOM MEETINGS",
    );
  });

  it("abandons an acknowledged Zoom account locally and resets the choice", async () => {
    const store = useAccountsStore();
    store.accounts = [zoomAccount()];
    store.activeAccountId = "zoom-1";
    vi.mocked(api.listAccounts).mockResolvedValueOnce([zoomAccount("zoom-2")]);
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });

    await wrapper.find('[title="Delete"]').trigger("click");
    await flushPromises();
    const checkbox = bodyEl(
      '[data-testid="zoom-abandon-checkbox"]',
    ) as HTMLInputElement;
    checkbox.click();
    await flushPromises();

    expect(bodyEl('[data-testid="delete-account-confirm"]')?.textContent)
      .toContain("Delete locally");
    bodyEl('[data-testid="delete-account-confirm"]')!.click();
    await flushPromises();

    expect(api.abandonZoomAccount).toHaveBeenCalledWith(
      "zoom-1",
      "ABANDON REMOTE ZOOM MEETINGS",
    );
    expect(api.deleteAccount).not.toHaveBeenCalled();
    expect(api.listAccounts).toHaveBeenCalled();
    expect(store.activeAccountId).toBe("zoom-2");

    await wrapper.find('[title="Delete"]').trigger("click");
    expect((bodyEl(
      '[data-testid="zoom-abandon-checkbox"]',
    ) as HTMLInputElement).checked).toBe(false);
  });
});
