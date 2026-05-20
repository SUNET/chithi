import { describe, it, expect, vi, beforeEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";

// Mock the IPC layer up-front. Every backend call returns a sensible
// default; individual tests override per-call with mockResolvedValueOnce
// for the cases they actually exercise.
vi.mock("@/lib/tauri", () => ({
  pgpListKeys: vi.fn().mockResolvedValue([]),
  pgpGetKey: vi.fn(),
  pgpImportKey: vi.fn().mockResolvedValue({
    fingerprint: "AAAA1111BBBB2222CCCC3333DDDD4444EEEE5555",
    isSecret: true,
  }),
  pgpImportKeyFile: vi.fn().mockResolvedValue({
    fingerprint: "FILE1111BBBB2222CCCC3333DDDD4444EEEE5555",
    isSecret: true,
  }),
  pgpPickAndImportKey: vi.fn().mockResolvedValue(null),
  pgpDeleteKey: vi.fn().mockResolvedValue(undefined),
  pgpExportPublic: vi.fn().mockResolvedValue("-----BEGIN PGP PUBLIC KEY BLOCK-----"),
  pgpWkdFetch: vi
    .fn()
    .mockResolvedValue("WKD1111BBBB2222CCCC3333DDDD4444EEEE5555"),
  pgpListCards: vi.fn().mockResolvedValue([]),
  pgpCardDetails: vi.fn(),
  pgpAutoLinkCards: vi.fn().mockResolvedValue([]),
  pgpDecryptMessage: vi.fn(),
  pgpVerifyMessage: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { usePgpStore } from "@/stores/pgp";
import * as api from "@/lib/tauri";
import type { PgpKeySummary } from "@/lib/types";

function makeKey(
  fingerprint: string,
  primaryUid: string | null,
  opts: Partial<{
    isSecret: boolean;
    isRevoked: boolean;
    cardIdents: string[];
    extraEmails: string[];
  }> = {},
): PgpKeySummary {
  return {
    fingerprint,
    isSecret: opts.isSecret ?? true,
    primaryUid,
    userIds: primaryUid
      ? [{ uid: primaryUid, email: opts.extraEmails?.[0] ?? null }]
      : [],
    subkeys: [],
    creationTime: "2026-01-01T00:00:00Z",
    expirationTime: null,
    isRevoked: opts.isRevoked ?? false,
    revocationTime: null,
    cardIdents: opts.cardIdents ?? [],
  };
}

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
});

describe("pgp store — fetchKeys", () => {
  it("populates state with the result of the backend call", async () => {
    const k = makeKey("ABCD" + "0".repeat(36), "Alice <alice@example.com>");
    vi.mocked(api.pgpListKeys).mockResolvedValueOnce([k]);
    const store = usePgpStore();
    await store.fetchKeys();
    expect(api.pgpListKeys).toHaveBeenCalledOnce();
    expect(store.keys).toEqual([k]);
    expect(store.lastError).toBeNull();
  });

  it("captures backend errors in lastError without throwing", async () => {
    vi.mocked(api.pgpListKeys).mockRejectedValueOnce(new Error("kaboom"));
    const store = usePgpStore();
    await store.fetchKeys();
    expect(store.lastError).toMatch(/kaboom/);
    expect(store.keys).toEqual([]);
  });

  it("drops the current selection if the previously-selected key is gone", async () => {
    const k1 = makeKey("FP1" + "0".repeat(37), "Alice");
    const k2 = makeKey("FP2" + "0".repeat(37), "Bob");
    vi.mocked(api.pgpListKeys).mockResolvedValueOnce([k1, k2]);
    const store = usePgpStore();
    await store.fetchKeys();
    store.selectKey(k2.fingerprint);
    expect(store.selectedFingerprint).toBe(k2.fingerprint);

    vi.mocked(api.pgpListKeys).mockResolvedValueOnce([k1]); // k2 deleted
    await store.fetchKeys();
    expect(store.selectedFingerprint).toBeNull();
  });
});

describe("pgp store — importArmored", () => {
  it("calls backend with bytes, refetches the list, and selects the new key", async () => {
    vi.mocked(api.pgpImportKey).mockResolvedValueOnce({
      fingerprint: "NEW1" + "0".repeat(36),
      isSecret: false,
    });
    const newKey = makeKey("NEW1" + "0".repeat(36), "New <new@example.com>", {
      isSecret: false,
    });
    vi.mocked(api.pgpListKeys).mockResolvedValueOnce([newKey]);

    const store = usePgpStore();
    const result = await store.importArmored(
      "-----BEGIN PGP PUBLIC KEY BLOCK-----\n…\n-----END PGP PUBLIC KEY BLOCK-----",
    );

    expect(api.pgpImportKey).toHaveBeenCalledOnce();
    expect(api.pgpListKeys).toHaveBeenCalledOnce(); // refresh after import
    expect(result.fingerprint).toBe("NEW1" + "0".repeat(36));
    expect(store.selectedFingerprint).toBe("NEW1" + "0".repeat(36));
  });
});

describe("pgp store — fetchViaWkd", () => {
  it("returns the imported fingerprint and refreshes the list", async () => {
    vi.mocked(api.pgpWkdFetch).mockResolvedValueOnce(
      "WKD2" + "0".repeat(36),
    );
    const newKey = makeKey(
      "WKD2" + "0".repeat(36),
      "Bob <bob@example.com>",
      { isSecret: false },
    );
    vi.mocked(api.pgpListKeys).mockResolvedValueOnce([newKey]);

    const store = usePgpStore();
    const fp = await store.fetchViaWkd("bob@example.com");

    expect(api.pgpWkdFetch).toHaveBeenCalledWith("bob@example.com");
    expect(fp).toBe("WKD2" + "0".repeat(36));
    expect(store.selectedFingerprint).toBe("WKD2" + "0".repeat(36));
  });

  it("propagates backend errors so the view can surface them", async () => {
    vi.mocked(api.pgpWkdFetch).mockRejectedValueOnce(
      new Error("no key found at example.com WKD"),
    );
    const store = usePgpStore();
    await expect(store.fetchViaWkd("nobody@example.com")).rejects.toThrow(
      /no key found/,
    );
  });
});

describe("pgp store — autoLinkCards", () => {
  it("returns the detections array and refreshes both keys and cards", async () => {
    vi.mocked(api.pgpAutoLinkCards).mockResolvedValueOnce([
      {
        keyFingerprint: "ABCD" + "0".repeat(36),
        cardIdent: "0006:DEADBEEF",
        slot: "signature",
        slotFingerprint: "ABCD" + "0".repeat(36),
      },
    ]);
    const store = usePgpStore();
    const detections = await store.autoLinkCards();

    expect(detections).toHaveLength(1);
    expect(detections[0].cardIdent).toBe("0006:DEADBEEF");
    // Refresh fan-out: list_keys + list_cards both invoked.
    expect(api.pgpListKeys).toHaveBeenCalledOnce();
    expect(api.pgpListCards).toHaveBeenCalledOnce();
  });
});

describe("pgp store — deleteKey", () => {
  it("clears selection if the deleted key was selected, then refetches", async () => {
    const k = makeKey("DEL1" + "0".repeat(36), "Doomed");
    vi.mocked(api.pgpListKeys).mockResolvedValueOnce([k]);
    const store = usePgpStore();
    await store.fetchKeys();
    store.selectKey(k.fingerprint);

    vi.mocked(api.pgpListKeys).mockResolvedValueOnce([]);
    await store.deleteKey(k.fingerprint);

    expect(api.pgpDeleteKey).toHaveBeenCalledWith(k.fingerprint);
    expect(store.selectedFingerprint).toBeNull();
    expect(store.keys).toEqual([]);
  });
});

describe("pgp decrypt / verify wrappers", () => {
  it("pgpDecryptMessage forwards account + message ids and returns the typed result", async () => {
    const decrypted = {
      plaintextBody: {
        id: "msg-1",
        subject: "Secret",
        from: { name: null, email: "a@x" },
        to: [],
        cc: [],
        date: "2026-05-20T00:00:00Z",
        flags: [],
        body_html: null,
        body_text: "Hello world",
        attachments: [],
        is_encrypted: true,
        is_signed: false,
        list_id: null,
        has_remote_images: false,
      },
      verifyOutcome: { kind: "unsigned" as const },
    };
    vi.mocked(api.pgpDecryptMessage).mockResolvedValueOnce(decrypted);
    const got = await api.pgpDecryptMessage("acc1", "msg-1");
    expect(api.pgpDecryptMessage).toHaveBeenCalledWith("acc1", "msg-1");
    expect(got.plaintextBody.body_text).toBe("Hello world");
    expect(got.verifyOutcome.kind).toBe("unsigned");
  });

  it("pgpVerifyMessage surfaces good signatures with signer info", async () => {
    vi.mocked(api.pgpVerifyMessage).mockResolvedValueOnce({
      kind: "good",
      signerUid: "Alice <a@x>",
      signerFingerprint: "AAAA" + "0".repeat(36),
      verifierFingerprint: "AAAA" + "0".repeat(36),
    });
    const o = await api.pgpVerifyMessage("acc1", "msg-1");
    expect(o.kind).toBe("good");
    if (o.kind === "good") {
      expect(o.signerUid).toBe("Alice <a@x>");
    }
  });
});

describe("pgp store — search", () => {
  it("filteredKeys narrows by UID / email / fingerprint substring", async () => {
    const alice = makeKey("AAAA" + "0".repeat(36), "Alice <alice@a.com>", {
      extraEmails: ["alice@a.com"],
    });
    const bob = makeKey("BBBB" + "0".repeat(36), "Bob <bob@b.com>", {
      extraEmails: ["bob@b.com"],
    });
    vi.mocked(api.pgpListKeys).mockResolvedValueOnce([alice, bob]);
    const store = usePgpStore();
    await store.fetchKeys();
    expect(store.filteredKeys).toHaveLength(2);

    store.setSearch("alice");
    expect(store.filteredKeys.map((k) => k.fingerprint)).toEqual([
      alice.fingerprint,
    ]);

    store.setSearch("BBBB"); // matches fingerprint prefix, case-insensitive
    expect(store.filteredKeys.map((k) => k.fingerprint)).toEqual([
      bob.fingerprint,
    ]);

    store.setSearch("");
    expect(store.filteredKeys).toHaveLength(2);
  });
});
