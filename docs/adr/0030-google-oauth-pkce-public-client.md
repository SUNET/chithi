# ADR 0030: Google OAuth PKCE Public Client

## Status
Superseded — Google Desktop apps require `client_secret` even with PKCE

## Context
The Google OAuth client secret was hardcoded in the source code (`oauth.rs`). Anyone who reads the binary or source could impersonate the app's OAuth identity. This was item 5 in the pre-public-preview security audit (security0.md).

The Google OAuth client was already registered as a Desktop application type in Google Cloud Console. Desktop apps support PKCE (Proof Key for Code Exchange) for the authorization code flow. Microsoft OAuth was already using PKCE as a true public client (no secret required).

## Decision (original)
Switch Google OAuth from confidential client flow (with embedded `client_secret`) to public client flow with PKCE, matching the existing Microsoft implementation.

### Changes (original)
```rust
pub const GOOGLE: OAuthProvider = OAuthProvider {
    name: "google",
    client_id: "96507156934-...",
    client_secret: "",    // was: "GOCSPX-..."
    use_pkce: true,       // was: false
    // auth_url, token_url, scopes unchanged
};
```

### How PKCE replaces the client secret
1. **Authorization request**: The app generates a random `code_verifier` (128 bytes, base64url), computes `code_challenge = base64url(sha256(code_verifier))`, and includes it in the auth URL
2. **Token exchange**: Instead of `client_secret`, the app sends the original `code_verifier`. The server verifies `sha256(code_verifier) == code_challenge` to prove the same app that initiated the flow is completing it
3. **Refresh**: When `client_secret` is empty, it is omitted from refresh requests

### Why this seemed correct
- A client secret in a desktop app binary is not actually secret (anyone can decompile it), so it provides no real security
- Google's own documentation (https://developers.google.com/identity/protocols/oauth2/native-app) lists `client_secret` as **Optional** for both token exchange and refresh
- Microsoft's Desktop app OAuth works without `client_secret` using only PKCE

## What actually happened
Removing `client_secret` caused Google's token endpoint to return:
```json
{ "error": "invalid_request", "error_description": "client_secret is missing." }
```

Despite the documentation saying `client_secret` is optional for Desktop apps, Google's token endpoint **requires** it in practice. This differs from Microsoft, which is a true public client that accepts PKCE alone.

### Investigation
1. Verified the OAuth client is registered as "Desktop app" type in Google Cloud Console (not Web application)
2. Confirmed Google's documentation at https://developers.google.com/identity/protocols/oauth2/native-app says `client_secret` is "Optional" for token exchange and "not applicable to requests from clients registered as Android, iOS, or Chrome applications" for refresh — but Desktop apps are not in that exclusion list
3. Google Desktop app clients are assigned a `client_secret` (with `GOCSPX-` prefix) at creation time, and the token endpoint enforces its presence

### Additional bugs found during investigation
Two other issues were discovered and fixed alongside the secret restoration:

1. **`access_type=offline` was dead code**: The `access_type=offline&prompt=consent` parameters (needed for Google to return a refresh token) were only appended in the `else` branch of the PKCE check — since `use_pkce` was set to `true`, this branch was never taken. Without `access_type=offline`, Google would not return a refresh token.

2. **`code_verifier` and `client_secret` were mutually exclusive**: The `exchange_code` function used `if/else if` logic that sent either `code_verifier` or `client_secret`, but never both. Google Desktop apps need both PKCE `code_verifier` AND `client_secret` in the token exchange request.

## Revised decision
Restore `client_secret` for Google OAuth while keeping PKCE enabled. Google Desktop apps use both PKCE and `client_secret` together — PKCE provides proof-of-possession security, while the secret is a required parameter that is not truly confidential (embedded in the distributed binary).

### Final configuration
```rust
pub const GOOGLE: OAuthProvider = OAuthProvider {
    name: "google",
    client_id: "96507156934-...",
    client_secret: "GOCSPX-...",  // required by Google despite being "optional" in docs
    use_pkce: true,
    // auth_url, token_url, scopes unchanged
};
```

### Code fixes applied
1. `exchange_code()`: Changed `if/else if` to two independent `if` blocks so both `code_verifier` and `client_secret` are sent
2. `get_auth_url()`: Moved `access_type=offline&prompt=consent` to a separate `if provider.name == "google"` block outside the PKCE branch
3. `refresh_access_token()` and `refresh_with_scopes()`: Already had `if !provider.client_secret.is_empty()` guards — these now work correctly since the secret is non-empty
4. Removed `client_secret` error workarounds in `commands/calendar.rs` and `commands/contacts.rs` that were masking the root cause

## Consequences
- Google OAuth uses both PKCE and `client_secret` — the secret is in the source but is not truly confidential for desktop apps (Google's own docs acknowledge this)
- Microsoft OAuth remains a true public client with PKCE only (no secret)
- The `exchange_code` function now supports providers that need both PKCE and a secret
- Google will correctly return refresh tokens (via `access_type=offline`)
- The key difference between Google and Microsoft OAuth is documented: Google Desktop ≠ public client, Microsoft Desktop = public client
