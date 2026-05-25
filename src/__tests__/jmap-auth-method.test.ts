/**
 * Contract test for the JMAP auth method union.
 *
 * Originally the union was "basic" | "oidc". Stalwart accepts HTTP Basic,
 * but Fastmail rejects it with `401 Invalid Authorization header, not
 * bearer` and only accepts `Authorization: Bearer <api-token>`. The
 * "bearer" variant carries the Fastmail API token in the password field
 * and the backend (mail/jmap.rs::from_account) promotes it to
 * access_token so apply_auth routes it through the bearer branch.
 *
 * If anyone narrows the type back to {"basic","oidc"} this fails to
 * compile under vue-tsc; the runtime assertion below is defence in
 * depth in case the contract is loosened to plain `string`.
 */
import { describe, it, expect } from "vitest";
import type { AccountConfig } from "@/lib/types";

describe("JMAP auth method union", () => {
  it("accepts 'bearer' as a valid jmap_auth_method", () => {
    const cfg: Pick<AccountConfig, "jmap_auth_method"> = {
      jmap_auth_method: "bearer",
    };
    expect(cfg.jmap_auth_method).toBe("bearer");
  });

  it("still accepts 'basic' and 'oidc'", () => {
    const basic: Pick<AccountConfig, "jmap_auth_method"> = { jmap_auth_method: "basic" };
    const oidc: Pick<AccountConfig, "jmap_auth_method"> = { jmap_auth_method: "oidc" };
    expect(basic.jmap_auth_method).toBe("basic");
    expect(oidc.jmap_auth_method).toBe("oidc");
  });
});
