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
  meetVisioLoginStart: vi.fn().mockResolvedValue({ session_id: "visio-session" }),
  meetVisioLoginComplete: vi.fn().mockResolvedValue("visio-1"),
  meetVisioLoginCancel: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: vi.fn().mockResolvedValue(undefined),
}));

import SettingsView from "@/views/SettingsView.vue";
import AccountFormModal from "@/components/settings/AccountFormModal.vue";
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
  usePlatformStore().width = 1280;
  usePlatformStore().kind = "desktop";
  usePlatformStore().platformReady = true;
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

const visioConfig: AccountConfig = {
  ...zoomConfig,
  display_name: "Work Visio",
  meet_url: "https://visio.example.org",
  meet_protocol: "visio",
  username: "",
};

function visioAccount(id = "visio-1"): Account {
  return {
    ...zoomAccount(id),
    display_name: "Work Visio",
    username: "",
    meet_protocol: "visio",
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

  it("does not offer or deep-link desktop-only Visio on native mobile", async () => {
    usePlatformStore().kind = "android";
    const router = makeRouter();
    await router.push("/settings?addAccount=visio");
    await router.isReady();
    const wrapper = mount(SettingsView, {
      global: { plugins: [router] },
      attachTo: document.body,
    });
    await flushPromises();

    expect(bodyEl('[data-testid="account-type-readonly"]')).toBeNull();
    await wrapper.find(".btn-add").trigger("click");
    expect(bodyEl('[data-testid="picker-visio"]')).toBeNull();
    expect(bodyEl('[data-testid="picker-zoom"]')).toBeTruthy();
  });

  it("waits for platform detection before processing a Visio deep link", async () => {
    const platform = usePlatformStore();
    platform.kind = "desktop";
    platform.platformReady = false;
    const router = makeRouter();
    await router.push("/settings?addAccount=visio");
    await router.isReady();
    mount(SettingsView, {
      global: { plugins: [router] },
      attachTo: document.body,
    });
    await flushPromises();
    expect(bodyEl('[data-testid="account-type-readonly"]')).toBeNull();

    platform.kind = "android";
    platform.platformReady = true;
    await flushPromises();
    expect(bodyEl('[data-testid="account-type-readonly"]')).toBeNull();
  });

  it("does not offer Visio before platform detection completes", async () => {
    const platform = usePlatformStore();
    platform.kind = "desktop";
    platform.platformReady = false;
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });

    await wrapper.find(".btn-add").trigger("click");
    expect(bodyEl('[data-testid="picker-visio"]')).toBeNull();

    platform.platformReady = true;
    await flushPromises();
    expect(bodyEl('[data-testid="picker-visio"]')).not.toBeNull();
  });

  it("rejects an imperative Visio open until platform detection completes", async () => {
    const platform = usePlatformStore();
    platform.kind = "desktop";
    platform.platformReady = false;
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });
    const modal = wrapper.findComponent(AccountFormModal);
    const exposed = modal.vm as unknown as {
      openNew: (type: "visio") => void;
    };

    exposed.openNew("visio");
    await flushPromises();
    expect(bodyEl('[data-testid="account-type-readonly"]')).toBeNull();

    platform.platformReady = true;
    exposed.openNew("visio");
    await flushPromises();
    expect(bodyEl('[data-testid="account-type-readonly"]')?.textContent)
      .toContain("La Suite Visio");
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

  it("warns that deleting Visio does not delete remote rooms", async () => {
    const store = useAccountsStore();
    store.accounts = [visioAccount()];
    vi.mocked(api.getAccountConfig).mockResolvedValue(visioConfig);
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });

    await wrapper.find('[title="Delete"]').trigger("click");
    await flushPromises();

    expect(bodyEl('[data-testid="visio-delete-warning"]')?.textContent)
      .toContain("Remote Visio rooms will remain");
    expect(bodyEl('[data-testid="visio-delete-warning"]')?.textContent)
      .toContain("forget its associations");
    expect(bodyEl('[data-testid="zoom-abandon-checkbox"]')).toBeNull();

    bodyEl('[data-testid="delete-account-confirm"]')!.click();
    await flushPromises();
    expect(api.deleteAccount).toHaveBeenCalledWith("visio-1");
  });

  it("keeps Visio reauthentication open so name edits use Save", async () => {
    const store = useAccountsStore();
    store.accounts = [visioAccount()];
    vi.mocked(api.getAccountConfig).mockResolvedValue(visioConfig);
    vi.mocked(api.listAccounts).mockResolvedValue([visioAccount()]);
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });

    await wrapper.find('[title="Edit"]').trigger("click");
    await flushPromises();
    bodyEl('[data-testid="visio-signin-btn"]')!.click();
    await flushPromises();

    expect(api.meetVisioLoginStart).toHaveBeenCalledWith(
      "https://visio.example.org",
      "visio-1",
    );
    expect(bodyEl('[data-testid="meet-auth-status"]')?.textContent)
      .toContain("Select Save to keep account name changes");
    expect(bodyEl('[data-testid="account-type-readonly"]')).not.toBeNull();
    expect(api.updateAccount).not.toHaveBeenCalled();

    const name = bodyEl('.account-form input[type="text"]') as HTMLInputElement;
    name.value = "Renamed Visio";
    name.dispatchEvent(new Event("input"));
    await wrapper.vm.$nextTick();
    bodyEl(".account-form")!
      .closest(".modal")!
      .querySelector<HTMLButtonElement>(".btn-primary")!
      .click();
    await flushPromises();
    expect(api.updateAccount).toHaveBeenCalledWith(
      "visio-1",
      expect.objectContaining({ display_name: "Renamed Visio" }),
    );
  });

  it("completes Visio account creation when the account refresh fails", async () => {
    const warning = vi.spyOn(console, "warn").mockImplementation(() => {});
    vi.mocked(api.listAccounts).mockRejectedValueOnce(new Error("refresh failed"));
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });
    await wrapper.find(".btn-add").trigger("click");
    bodyEl('[data-testid="picker-visio"]')!.click();
    await flushPromises();
    const url = bodyEl('[data-testid="visio-url"]') as HTMLInputElement;
    url.value = "https://visio.example.org";
    url.dispatchEvent(new Event("input"));
    await wrapper.vm.$nextTick();

    bodyEl('[data-testid="visio-signin-btn"]')!.click();
    await flushPromises();

    expect(api.meetVisioLoginComplete).toHaveBeenCalledTimes(1);
    expect(bodyEl('[data-testid="account-type-readonly"]')).toBeNull();
    expect(bodyEl(".form-error")).toBeNull();
    expect(warning).toHaveBeenCalledWith(
      "signInWithVisio: account persisted but list refresh failed",
      expect.any(Error),
    );
    warning.mockRestore();
  });

  it("cancels an in-flight Visio login when the form closes", async () => {
    let rejectCompletion!: (reason: Error) => void;
    vi.mocked(api.meetVisioLoginComplete).mockImplementationOnce(
      () => new Promise((_resolve, reject) => { rejectCompletion = reject; }),
    );
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });
    await wrapper.find(".btn-add").trigger("click");
    bodyEl('[data-testid="picker-visio"]')!.click();
    await flushPromises();
    const url = bodyEl('[data-testid="visio-url"]') as HTMLInputElement;
    url.value = "https://visio.example.org";
    url.dispatchEvent(new Event("input"));
    await wrapper.vm.$nextTick();
    bodyEl('[data-testid="visio-signin-btn"]')!.click();
    await flushPromises();

    bodyEl(".modal-close")!.click();
    await flushPromises();
    expect(api.meetVisioLoginCancel).toHaveBeenCalledWith("visio-session");

    rejectCompletion(new Error("Visio sign-in was cancelled"));
    await flushPromises();
  });

  it("cancels a stale Visio start without mutating a reopened form", async () => {
    let resolveStart!: (value: { session_id: string }) => void;
    vi.mocked(api.meetVisioLoginStart).mockImplementationOnce(
      () => new Promise((resolve) => { resolveStart = resolve; }),
    );
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });
    await wrapper.find(".btn-add").trigger("click");
    bodyEl('[data-testid="picker-visio"]')!.click();
    await flushPromises();
    const url = bodyEl('[data-testid="visio-url"]') as HTMLInputElement;
    url.value = "https://visio.example.org";
    url.dispatchEvent(new Event("input"));
    await wrapper.vm.$nextTick();
    bodyEl('[data-testid="visio-signin-btn"]')!.click();
    await flushPromises();

    bodyEl(".modal-close")!.click();
    await wrapper.find(".btn-add").trigger("click");
    bodyEl('[data-testid="picker-imap"]')!.click();
    await flushPromises();
    resolveStart({ session_id: "stale-session" });
    await flushPromises();

    expect(api.meetVisioLoginCancel).toHaveBeenCalledWith("stale-session");
    expect(api.meetVisioLoginComplete).not.toHaveBeenCalled();
    expect(bodyEl('[data-testid="account-type-readonly"]')?.textContent).toContain("IMAP");
  });

  it("hides Visio reauthentication controls on native mobile", async () => {
    const store = useAccountsStore();
    store.accounts = [visioAccount()];
    usePlatformStore().kind = "android";
    vi.mocked(api.getAccountConfig).mockResolvedValue(visioConfig);
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });

    await wrapper.find('[title="Edit"]').trigger("click");
    await flushPromises();
    expect(bodyEl('[data-testid="visio-signin-btn"]')).toBeNull();
    expect(document.body.textContent).toContain("Visio sign-in is available in the desktop app");
  });

  it("hides Visio reauthentication until platform detection completes", async () => {
    const store = useAccountsStore();
    store.accounts = [visioAccount()];
    usePlatformStore().kind = "desktop";
    usePlatformStore().platformReady = false;
    vi.mocked(api.getAccountConfig).mockResolvedValue(visioConfig);
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });

    await wrapper.find('[title="Edit"]').trigger("click");
    await flushPromises();
    expect(bodyEl('[data-testid="visio-signin-btn"]')).toBeNull();
    expect(document.body.textContent).toContain("Visio sign-in is available in the desktop app");
  });

  it("does not start Visio sign-in if platform readiness is lost before the click", async () => {
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });
    await wrapper.find(".btn-add").trigger("click");
    bodyEl('[data-testid="picker-visio"]')!.click();
    await flushPromises();
    const url = bodyEl('[data-testid="visio-url"]') as HTMLInputElement;
    url.value = "https://visio.example.org";
    url.dispatchEvent(new Event("input"));
    await wrapper.vm.$nextTick();
    const signIn = bodyEl('[data-testid="visio-signin-btn"]')!;

    usePlatformStore().platformReady = false;
    signIn.click();
    await flushPromises();

    expect(api.meetVisioLoginStart).not.toHaveBeenCalled();
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

  it("disables deletion until a hidden meet binding is checked", async () => {
    const store = useAccountsStore();
    store.accounts = [{ ...visioAccount(), meet_protocol: "" }];
    let resolveConfig!: (value: AccountConfig) => void;
    vi.mocked(api.getAccountConfig).mockImplementationOnce(
      () => new Promise((resolve) => { resolveConfig = resolve; }),
    );
    const wrapper = mount(SettingsView, {
      global: { plugins: [makeRouter()] },
      attachTo: document.body,
    });

    wrapper.find('[title="Delete"]').element.dispatchEvent(new MouseEvent("click"));
    await wrapper.vm.$nextTick();
    const confirm = bodyEl('[data-testid="delete-account-confirm"]') as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    expect(confirm.textContent).toContain("Checking");

    resolveConfig(visioConfig);
    await flushPromises();
    expect(confirm.disabled).toBe(false);
    expect(bodyEl('[data-testid="visio-delete-warning"]')).not.toBeNull();
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
