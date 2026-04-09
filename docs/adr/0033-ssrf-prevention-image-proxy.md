# ADR 0033: SSRF Prevention in Image Proxy

## Status
Accepted

## Context
The "Load images" feature (ADR 0032) downloads remote images via the Rust backend and embeds them as base64 data URIs. The URLs come from `<img src>` attributes in untrusted email HTML. An attacker can craft an email with images pointing to internal services (`https://192.168.1.1/admin`, `https://localhost:8080/metrics`), and when the user clicks "Load images", the backend fetches those URLs — probing the user's internal network from the server side.

This is a Server-Side Request Forgery (SSRF) vulnerability. Even with the existing mitigations (HTTPS-only, image/* content-type check, 5MB limit, 10s timeout), the backend would still make the request and reveal information via timing (service exists vs connection refused) or error messages.

## Decision
Resolve DNS before fetching and block all private/reserved IP ranges.

### Implementation (`src-tauri/src/commands/mail.rs`)

**Step 1: Block private hostnames**
```rust
let h = host.to_lowercase();
if h == "localhost" || h.ends_with(".local") || h.ends_with(".internal") {
    return None;
}
```

**Step 2: Resolve DNS and check all addresses**
```rust
let addrs = tokio::net::lookup_host(format!("{}:{}", host, port)).await;
for addr in addrs {
    if addr.ip().is_loopback() || addr.ip().is_unspecified() || is_private_ip(&addr.ip()) {
        return None; // Block
    }
}
```

**Step 3: `is_private_ip` covers all reserved ranges**

| Range | Type |
|-------|------|
| `127.0.0.0/8` | Loopback |
| `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` | RFC 1918 private |
| `169.254.0.0/16` | Link-local |
| `100.64.0.0/10` | CGNAT (Carrier-grade NAT) |
| `255.255.255.255` | Broadcast |
| `0.0.0.0` | Unspecified |
| `::1` | IPv6 loopback |
| `::` | IPv6 unspecified |
| `fc00::/7` | IPv6 ULA (Unique Local Address) |
| `fe80::/10` | IPv6 link-local |

### Why DNS resolution matters
Checking the hostname string alone is insufficient:
- Numeric IPs: `https://192.168.1.1/` has no hostname to block
- Hex IPs: `https://0x7f000001/` resolves to `127.0.0.1`
- DNS rebinding: `attacker.com` can resolve to `192.168.1.1`
- IPv6 shorthand: `https://[::1]/` bypasses string checks

By resolving the hostname to IP addresses and checking every resolved address, all these vectors are blocked.

### What this prevents
- Probing internal HTTP/HTTPS services on the user's network
- Accessing cloud metadata endpoints (e.g., `https://169.254.169.254/`)
- Timing-based service discovery (private IPs are rejected before any connection)
- DNS rebinding attacks (the resolved IP is checked, not the hostname)

### What remains allowed
- Public HTTPS URLs on the internet (legitimate image hosting)
- CDN-hosted email images (the normal case)

## Consequences
- Remote image loading is safe against SSRF — private network probing is blocked before any connection is made
- DNS resolution adds a small latency per image (~1-5ms) but runs in parallel across all images
- Edge case: if a legitimate image CDN resolves to a private IP (unusual), it would be blocked. This is an acceptable trade-off — CDNs should have public IPs.
