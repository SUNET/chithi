import { beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { reactive } from "vue";

vi.mock("@/lib/tauri", () => ({
  discoverMailServers: vi.fn(),
}));

import OauthSignInSection from "@/components/settings/OauthSignInSection.vue";
import JmapSection from "@/components/settings/JmapSection.vue";
import ImapServerSection from "@/components/settings/ImapServerSection.vue";
import SyncBindingsSection from "@/components/settings/SyncBindingsSection.vue";
import * as api from "@/lib/tauri";
import type { AccountConfig } from "@/lib/types";

function makeForm(overrides: Partial<AccountConfig> = {}): AccountConfig {
  return reactive({
    display_name: "",
    email: "",
    provider: "generic",
    mail_protocol: "imap",
    imap_host: "",
    imap_port: 993,
    smtp_host: "",
    smtp_port: 587,
    jmap_url: "",
    caldav_url: "",
    meet_url: "",
    meet_protocol: "",
    username: "",
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
    has_calendar_binding: false,
    has_contacts_binding: false,
    pgp_attach_pubkey_on_sign: true,
    pgp_autocrypt_header: true,
    pgp_encrypt_subject: true,
    pgp_encrypt_drafts: true,
    ...overrides,
  });
}

beforeEach(() => {
  vi.clearAllMocks();
});

describe("OauthSignInSection", () => {
  it("shows the sign-in button and emits signIn", async () => {
    const wrapper = mount(OauthSignInSection, {
      props: { provider: "google", status: null, inProgress: false },
    });
    expect(wrapper.text()).toContain("Sign in with Google");
    await wrapper.find(".btn-oauth").trigger("click");
    expect(wrapper.emitted("signIn")).toHaveLength(1);
  });

  it("renders the status row and emits reauth from Sign in again", async () => {
    const wrapper = mount(OauthSignInSection, {
      props: { provider: "microsoft", status: "Signed in with Microsoft", inProgress: false },
    });
    expect(wrapper.find(".oauth-status").text()).toContain("Signed in with Microsoft");
    await wrapper.find(".btn-reauth").trigger("click");
    expect(wrapper.emitted("reauth")).toHaveLength(1);
  });
});

describe("JmapSection", () => {
  it("OIDC mode renders the sign-in button and the device code", async () => {
    const form = makeForm({ jmap_auth_method: "oidc", email: "u@x.org" });
    const wrapper = mount(JmapSection, {
      props: {
        form,
        editing: false,
        oauthStatus: null,
        oidcUserCode: null,
        oauthInProgress: false,
      },
    });
    expect(wrapper.text()).toContain("Sign in with OIDC");
    await wrapper.find(".btn-oauth").trigger("click");
    expect(wrapper.emitted("oidcSignIn")).toHaveLength(1);

    await wrapper.setProps({ oidcUserCode: "ABCD-1234" });
    expect(wrapper.find(".device-code-value").text()).toBe("ABCD-1234");
  });

  it("switching back to Basic mutates the form and emits reauth", async () => {
    const form = makeForm({ jmap_auth_method: "oidc" });
    const wrapper = mount(JmapSection, {
      props: {
        form,
        editing: false,
        oauthStatus: "Signed in via OIDC",
        oidcUserCode: null,
        oauthInProgress: false,
      },
    });
    const basicBtn = wrapper
      .findAll(".type-btn")
      .find((b) => b.text() === "Password")!;
    await basicBtn.trigger("click");
    expect(form.jmap_auth_method).toBe("basic");
    expect(wrapper.emitted("reauth")).toHaveLength(1);
  });
});

describe("ImapServerSection", () => {
  it("discovery never overwrites a prefilled host", async () => {
    vi.mocked(api.discoverMailServers).mockResolvedValue({
      imap_host: "imap.found.org",
      imap_port: 993,
      imap_use_tls: true,
      smtp_host: "smtp.found.org",
      smtp_port: 587,
      smtp_use_tls: true,
      source: "autoconfig",
    });
    const form = makeForm({
      email: "u@found.org",
      imap_host: "imap.mine.org",
      smtp_host: "smtp.mine.org",
    });
    const wrapper = mount(ImapServerSection, {
      props: { form, editing: false },
    });
    await wrapper.find('[data-testid="mail-discover-btn"]').trigger("click");
    await flushPromises();

    expect(form.imap_host).toBe("imap.mine.org");
    expect(form.smtp_host).toBe("smtp.mine.org");
    expect(
      wrapper.find('[data-testid="mail-discovery-note"]').text(),
    ).toContain("Kept your existing");
  });

  it("discovery fills empty hosts and applies TLS", async () => {
    vi.mocked(api.discoverMailServers).mockResolvedValue({
      imap_host: "imap.found.org",
      imap_port: 143,
      imap_use_tls: false,
      smtp_host: "smtp.found.org",
      smtp_port: 587,
      smtp_use_tls: false,
      source: "autoconfig",
    });
    const form = makeForm({
      email: "u@found.org",
      imap_host: "",
      imap_port: 0,
      smtp_host: "",
      smtp_port: 0,
    });
    const wrapper = mount(ImapServerSection, {
      props: { form, editing: false },
    });
    await wrapper.find('[data-testid="mail-discover-btn"]').trigger("click");
    await flushPromises();

    expect(form.imap_host).toBe("imap.found.org");
    expect(form.imap_port).toBe(143);
    expect(form.smtp_host).toBe("smtp.found.org");
    expect(form.use_tls).toBe(false);
    expect(
      wrapper.find('[data-testid="mail-discovery-note"]').text(),
    ).toContain("Filled IMAP + SMTP");
  });
});

describe("SyncBindingsSection", () => {
  function mountSection(form: AccountConfig) {
    return mount(SyncBindingsSection, {
      props: {
        form,
        hasCalendarBinding: true,
        hasContactsBinding: true,
        availableBooks: [{ id: "b1", label: "Acc / Personal" }],
        mailBookId: null,
        calendarBookId: null,
        "onUpdate:mailBookId": () => {},
        "onUpdate:calendarBookId": () => {},
      },
    });
  }

  it("interval input converts minutes to seconds and clamps to 1 minute", async () => {
    const form = makeForm();
    const wrapper = mountSection(form);

    await wrapper.find('[data-testid="calendar-sync-interval"]').setValue("7");
    expect(form.calendar_sync_interval_seconds).toBe(420);

    // Sub-minute values clamp up to 1 minute (60s).
    await wrapper.find('[data-testid="calendar-sync-interval"]').setValue("0");
    expect(form.calendar_sync_interval_seconds).toBe(60);
  });

  it("renders the seconds value back as minutes", () => {
    const form = makeForm({ contacts_sync_interval_seconds: 1800 });
    const wrapper = mountSection(form);
    const input = wrapper.find('[data-testid="contacts-sync-interval"]')
      .element as HTMLInputElement;
    expect(input.value).toBe("30");
  });
});
