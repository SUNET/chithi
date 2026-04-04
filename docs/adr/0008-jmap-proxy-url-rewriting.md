# ADR 0008: JMAP session URL rewriting for reverse proxy deployments

## Status

Accepted

## Date

2026-04-04

## Context

Stalwart JMAP server (and potentially other JMAP servers) is commonly deployed behind an nginx reverse proxy. The HTTPS proxy terminates TLS and forwards to the internal Stalwart instance on HTTP port 8080. The JMAP session document (`.well-known/jmap`) is served correctly through the proxy, but the URLs it advertises (`apiUrl`, `downloadUrl`, `uploadUrl`) point to the **internal** address (`http://mail.example.com:8080/jmap/`) which is not accessible from external clients.

We initially used the `jmap-client` Rust crate for JMAP protocol support, but encountered multiple issues:

1. **HTTPS→HTTP redirect stripping Authorization headers**: Standard HTTP behavior strips the `Authorization` header when redirecting across schemes (HTTPS→HTTP), causing 401 errors on the session fetch.
2. **Internal URLs in session**: Even after successful session fetch, the library's API calls went to `http://host:8080/jmap/` which is not externally accessible, causing requests to hang or fail.
3. **No URL override support**: The `jmap-client` library stores `apiUrl` as a private field with no setter, and doesn't support rewriting session URLs.

## Decision

Replaced `jmap-client`'s HTTP layer with direct `reqwest` calls for all JMAP operations. The `JmapConnection` struct manages its own HTTP client and implements the JMAP protocol using raw JSON-RPC over HTTP:

1. **Session fetch**: Fetched manually via `reqwest` with explicit `basic_auth()`, bypassing redirect issues since reqwest handles auth persistence across redirects.

2. **URL rewriting**: After parsing the session JSON, all URLs (`apiUrl`, `downloadUrl`, `uploadUrl`) are rewritten using `rewrite_url()` — a simple string function that replaces the scheme+host+port prefix with the HTTPS proxy base URL while preserving the path and template placeholders (`{accountId}`, `{blobId}`, etc.).

   Example: `http://mail.example.com:8080/jmap/download/{accountId}/{blobId}` → `https://mail.example.com/jmap/download/{accountId}/{blobId}`

3. **All API calls** go through the HTTPS proxy with `basic_auth()` on every request, since the proxy forwards to Stalwart which performs authentication.

4. **Auto-discovery** tries `https://<domain>`, `https://mail.<domain>`, and `https://jmap.<domain>` in order, accepting either 200 or 401 (which confirms the endpoint exists).

5. **User-provided URL cleanup**: If the user enters `https://host/.well-known/jmap`, the suffix is stripped since the code appends it automatically.

## Consequences

- Works reliably with Stalwart behind nginx reverse proxy without any server-side configuration changes.
- The `jmap-client` crate is still in `Cargo.toml` but unused for HTTP operations — it could be removed in the future.
- Each JMAP API call includes explicit authentication, which is slightly more verbose but ensures credentials are always sent regardless of redirect behavior.
- The URL rewriting is simple string manipulation (not URL parsing) to preserve template placeholders with curly braces that `url::Url` would percent-encode.
