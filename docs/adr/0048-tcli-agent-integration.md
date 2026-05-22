# ADR 0048: tcli Agent Integration for OpenPGP Secrets

## Status
Accepted

## Context
Chithi obtains every OpenPGP passphrase and smartcard PIN through its own
prompt: `acquire_secret` (`src-tauri/src/commands/pgp.rs`) emits a
`pgp-secret-needed` event, the frontend's `PassphraseDialog` / `PinDialog`
collect the secret, and it is held in a process-local
`libtumpa::cache::CredentialCache` for the lifetime of the app (ADR 0047).

Chithi is part of the wider "tumpa" tool family. `tcli` (tumpa-cli) ships a
gpg-agent-style daemon — `tcli agent`, a Unix socket at
`~/.tumpa/agent.sock` — that caches passphrases and PINs across every Tumpa
tool with a TTL (default 30 minutes). Without integration, a user who
unlocked a key in `tcli` or the Tumpa desktop app still has to re-enter the
same secret in chithi, and vice versa.

This ADR records how chithi reuses that agent so a key is unlocked once per
machine, while remaining fully functional when the agent is not running.

## Decision

### The agent is a shared cache, not a prompt
Chithi talks to the agent purely as a **shared credential cache**. It reads
cached secrets and writes dialog-collected secrets back, but it never
invokes the agent's own pinentry (`GET_OR_PROMPT`). Chithi's
`PassphraseDialog` / `PinDialog` remain the only prompt UI — they are
window-routed (ADR 0047) and integrated with chithi's look and memory
hygiene; a separate native pinentry window would not be.

### Protocol
`mail/pgp_agent.rs` implements a ~120-line client for the agent's
line-based protocol (verified against tumpa-cli `src/agent/protocol.rs`):

```text
GET_PASSPHRASE   <key>          -> PASSPHRASE <b64> | NOT_FOUND
PUT_PASSPHRASE   <key> <b64>    -> OK
CLEAR_PASSPHRASE <key>          -> OK
```

Requests and responses are single `\n`-terminated lines; values are base64
(`STANDARD`, padded); there is no connect banner. The cache key is
`passphrase:<FP>` for software keys and `pin:<FP>` for card PINs, `<FP>` an
**upper-case** OpenPGP fingerprint — byte-identical to what `tcli` itself
writes, so the caches interoperate. `libtumpa` ships no client for this
protocol, so chithi implements its own.

For smartcard PINs, `tcli` and `tpass` key the agent cache by the card key's
primary fingerprint — `libtumpa`'s `DecryptionCard.key_info.fingerprint` on
the decrypt path, the resolved signer `KeyInfo.fingerprint` on the sign path.
Chithi reads the same field from the same `libtumpa` calls, so the
`pin:<FP>` keys match byte-for-byte with no dependency on the `card_keys`
link table being populated. (Chithi's *in-process* cache still keys PINs by
card ident; only the agent key uses the fingerprint.)

### Discovery and fallback
Discovery is pure auto-detect, zero configuration. Every secret request
tries to connect to the socket with a **1-second timeout**. Success ⇒ use
the agent; any failure (missing/stale socket, connection refused, timeout,
protocol error) ⇒ fall back. The agent is Unix-only; on other platforms the
client is a stub that always reports the agent unavailable.

### Cache authority
When the agent is reachable it is the **sole** cache: chithi does *not* also
populate its in-process `CredentialCache`, so the agent's TTL governs
expiry. When the agent is down, chithi caches in-process exactly as before
the integration (held for the app lifetime, no TTL). The decision is
per-request, so an agent that dies mid-session simply falls to the local
path on the next request. `acquire_secret` owns this whole policy;
`pgp_provide_secret` is now a pure relay.

### Eviction
- **Manual `pgp_forget_all` / `pgp_forget_card`** clear only chithi's
  in-process cache. They deliberately leave the shared agent untouched —
  clearing it would forget secrets for every other Tumpa tool.
- **Automatic auth-failure eviction** (`evict_cached_secret`, called when a
  cached secret produces a sign/encrypt/decrypt failure) *does* send a
  targeted `CLEAR_PASSPHRASE` for that one key/card to the agent, in
  addition to clearing the in-process cache. Without it a wrong
  agent-supplied secret would be served straight back by the next
  `GET_PASSPHRASE` — the "wrong PIN loops forever" regression. The CLEAR is
  fire-and-forget and never `CLEAR_ALL`.

## Trust boundary
The agent socket is a `0600` Unix socket owned by the user; only processes
running as the same UID can connect. base64 is an encoding, not encryption.
This is exactly the trust model of `gpg-agent` and `ssh-agent`, and the same
boundary `tcli` and the Tumpa desktop app already rely on. Reading a secret
from, and writing one to, the user's own agent does not widen chithi's
existing exposure.

## Consequences
- A key unlocked in any Tumpa tool is reused by chithi, and vice versa.
- Secrets now respect the agent's TTL when the agent is running.
- Chithi gains a small Unix-socket client and a per-request connect attempt
  (a local socket; the 1 s timeout only bites a wedged agent — a missing
  agent fails instantly).
- No behaviour change when the agent is not running.

## See also
- ADR 0047 — OpenPGP integration (keystore, prompts, smartcards).
- `src-tauri/src/mail/pgp_agent.rs` — the protocol client.
- tumpa-cli `src/agent/` — the agent implementation and protocol.
