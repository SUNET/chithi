# ADR 0030: Google OAuth PKCE Public Client

## Status
Accepted

## Context
The Google OAuth client secret was hardcoded in the source code (`oauth.rs`). Anyone who reads the binary or source could impersonate the app's OAuth identity. This was item 5 in the pre-public-preview security audit (security0.md).

The Google OAuth client was already registered as a Desktop application type in Google Cloud Console. Desktop apps support PKCE (Proof Key for Code Exchange) for the authorization code flow, eliminating the need for a client secret. Microsoft OAuth was already using this pattern.

## Decision
Switch Google OAuth from confidential client flow (with embedded `client_secret`) to public client flow with PKCE, matching the existing Microsoft implementation.

### Changes
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
3. **Refresh**: When `client_secret` is empty, it is omitted from refresh requests. Google accepts this for Desktop clients.

### Why this is secure
- The `code_verifier` is generated per-session and never leaves the app process — it cannot be extracted from the binary
- A client secret in a desktop app binary is not actually secret (anyone can decompile it), so it provides no real security. PKCE provides proof-of-possession without a static secret
- Both Google and Microsoft recommend PKCE for installed/desktop applications

### Token exchange logic (unchanged)
The existing code in `exchange_code_for_tokens()` and `refresh_access_token()` already handled both patterns:
- When `use_pkce` is true: sends `code_verifier` instead of `client_secret`
- When `client_secret` is empty: omits it from refresh requests

Only the `GOOGLE` provider config needed updating.

## Consequences
- No secrets in the source code — both Google and Microsoft use public-client PKCE
- Existing Google OAuth tokens may require re-authentication since the flow parameters changed
- The same PKCE infrastructure (verifier generation, challenge computation, verifier storage) is shared between Google and Microsoft
