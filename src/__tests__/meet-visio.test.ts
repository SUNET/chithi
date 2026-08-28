import { beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { reactive } from "vue";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

import MeetAccountSection from "@/components/settings/MeetAccountSection.vue";
import {
  meetVisioLoginCancel,
  meetVisioLoginComplete,
  meetVisioLoginStart,
} from "@/lib/tauri";
import type { AccountConfig } from "@/lib/types";

function makeForm(meetUrl = "https://visio.example.org"): AccountConfig {
  return reactive({
    display_name: "Work Visio",
    sender_name: "",
    email: "",
    provider: "generic",
    mail_protocol: "",
    imap_host: "",
    imap_port: 0,
    smtp_host: "",
    smtp_port: 0,
    jmap_url: "",
    caldav_url: "",
    meet_url: meetUrl,
    meet_protocol: "visio",
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

describe("MeetAccountSection Visio", () => {
  it("requires a non-whitespace instance root", async () => {
    const wrapper = mount(MeetAccountSection, {
      props: {
        form: makeForm("   "),
        accountType: "visio",
        editing: false,
        signingIn: false,
      },
    });

    expect(wrapper.text()).toContain("Visio instance URL");
    expect(wrapper.text()).toContain("short-lived room-creation token");
    expect(
      (wrapper.get('[data-testid="visio-signin-btn"]').element as HTMLButtonElement)
        .disabled,
    ).toBe(true);
  });

  it("supports reauthentication without changing the instance", async () => {
    const wrapper = mount(MeetAccountSection, {
      props: {
        form: makeForm(),
        accountType: "visio",
        editing: true,
        signingIn: false,
      },
    });

    expect(
      (wrapper.get('[data-testid="visio-url"]').element as HTMLInputElement).disabled,
    ).toBe(true);
    const button = wrapper.get('[data-testid="visio-signin-btn"]');
    expect(button.text()).toContain("Sign in again with Visio");
    await button.trigger("click");
    expect(wrapper.emitted("signIn")).toHaveLength(1);
  });
});

describe("Visio login Tauri wrappers", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    invokeMock.mockResolvedValue("visio-account");
  });

  it("pins optional reauthentication to an account", async () => {
    await meetVisioLoginStart("https://visio.example.org", "account-123");

    expect(invokeMock).toHaveBeenCalledWith("meet_visio_login_start", {
      serverUrl: "https://visio.example.org",
      accountId: "account-123",
    });
  });

  it("starts new-account authentication without an account", async () => {
    await meetVisioLoginStart("https://visio.example.org");

    expect(invokeMock).toHaveBeenCalledWith("meet_visio_login_start", {
      serverUrl: "https://visio.example.org",
      accountId: null,
    });
  });

  it("completes with only the opaque session and display name", async () => {
    await meetVisioLoginComplete("session-123", "Work Visio");

    expect(invokeMock).toHaveBeenCalledWith("meet_visio_login_complete", {
      sessionId: "session-123",
      displayName: "Work Visio",
    });
  });

  it("cancels by opaque session id", async () => {
    await meetVisioLoginCancel("session-123");

    expect(invokeMock).toHaveBeenCalledWith("meet_visio_login_cancel", {
      sessionId: "session-123",
    });
  });
});
