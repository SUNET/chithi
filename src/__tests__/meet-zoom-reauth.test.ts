import { beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { reactive } from "vue";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

import MeetAccountSection from "@/components/settings/MeetAccountSection.vue";
import {
  abandonZoomAccount,
  meetZoomLoginComplete,
  meetZoomLoginStart,
} from "@/lib/tauri";
import type { AccountConfig } from "@/lib/types";

function makeForm(): AccountConfig {
  return reactive({
    display_name: "Work Zoom",
    email: "",
    provider: "generic",
    mail_protocol: "",
    imap_host: "",
    imap_port: 0,
    smtp_host: "",
    smtp_port: 0,
    jmap_url: "",
    caldav_url: "",
    meet_url: "",
    meet_protocol: "zoom",
    username: "",
    password: "",
    use_tls: true,
    signature: "",
    jmap_auth_method: "basic",
    oidc_token_endpoint: "",
    oidc_client_id: "",
    calendar_sync_enabled: false,
    mail_sync_enabled: false,
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
  });
}

describe("MeetAccountSection Zoom reauthentication", () => {
  it("shows an enabled reauthentication button and emits signIn", async () => {
    const wrapper = mount(MeetAccountSection, {
      props: {
        form: makeForm(),
        accountType: "zoom",
        editing: true,
        signingIn: false,
      },
    });

    const button = wrapper.get('[data-testid="zoom-signin-btn"]');
    expect(button.text()).toContain("Sign in again with Zoom");
    expect((button.element as HTMLButtonElement).disabled).toBe(false);
    expect(wrapper.text()).not.toContain("delete this account");

    await button.trigger("click");
    expect(wrapper.emitted("signIn")).toHaveLength(1);
  });

  it.each(["talk", "matrix"] as const)(
    "keeps %s edit behavior unchanged",
    (accountType) => {
      const wrapper = mount(MeetAccountSection, {
        props: {
          form: makeForm(),
          accountType,
          editing: true,
          signingIn: false,
        },
      });

      expect(wrapper.find(`[data-testid="${accountType}-signin-btn"]`).exists())
        .toBe(false);
      expect(wrapper.text()).toContain("delete this account");
    },
  );
});

describe("Zoom login Tauri wrappers", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    invokeMock.mockResolvedValue("zoom-account");
  });

  it("passes an existing account ID to the start command", async () => {
    await meetZoomLoginStart("account-123");

    expect(invokeMock).toHaveBeenCalledWith("meet_zoom_login_start", {
      accountId: "account-123",
    });
  });

  it("sends null to the start command for a new account", async () => {
    await meetZoomLoginStart();

    expect(invokeMock).toHaveBeenCalledWith("meet_zoom_login_start", {
      accountId: null,
    });
  });

  it("sends only port and display name to the completion command", async () => {
    await meetZoomLoginComplete(43123, "Work Zoom");

    expect(invokeMock).toHaveBeenCalledWith("meet_zoom_login_complete", {
      port: 43123,
      displayName: "Work Zoom",
    });
  });
});

describe("abandonZoomAccount", () => {
  it("sends the account and exact confirmation to the Tauri command", async () => {
    await abandonZoomAccount(
      "account-123",
      "ABANDON REMOTE ZOOM MEETINGS",
    );

    expect(invokeMock).toHaveBeenCalledWith("abandon_zoom_account", {
      accountId: "account-123",
      confirmation: "ABANDON REMOTE ZOOM MEETINGS",
    });
  });
});
