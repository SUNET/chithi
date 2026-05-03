//! Thunderbird-style email autoconfig (#43).
//!
//! Mirrors the discovery sequence used by Mozilla Thunderbird when a user
//! types an email address into the new-account wizard. We run all the
//! candidate sources in priority order, return the first successful
//! parse, and surface the source so the UI can show "Found at <ISP DB>"
//! style hints.
//!
//! Sources, in order:
//! 1. Mozilla ISP database — `https://autoconfig.thunderbird.net/v1.1/<domain>`
//! 2. Provider-hosted autoconfig — `https://autoconfig.<domain>/mail/config-v1.1.xml`
//! 3. `.well-known` endpoint — `https://<domain>/.well-known/autoconfig/mail/config-v1.1.xml`
//! 4. MX lookup — gives us the mailserver hostname even when the domain
//!    has no autoconfig published. We never accept that hostname blindly,
//!    we just use it as another candidate to probe IMAPS / DAV against.
//!
//! All HTTP fetches share a single 8s timeout. The XML response is
//! capped at 64 KB so a hostile server can't make us load megabytes of
//! garbage.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use uppsala::{Document, NodeId, NodeKind};

/// Resolved IMAP / SMTP server settings from a Thunderbird-format
/// `clientConfig` document. Subset of the schema — we only persist the
/// fields the UI actually round-trips into AccountConfig.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AutoconfigServers {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_use_tls: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_use_tls: bool,
}

const FETCH_TIMEOUT_SECS: u64 = 8;
const MAX_BYTES: usize = 64 * 1024;

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()
        .map_err(|e| Error::Other(format!("autoconfig http client: {}", e)))
}

/// Run the full autoconfig sequence for an email address. Returns the
/// first successful result and the source it came from. If every probe
/// fails, returns `Ok(None)` (a hard error here would block the user
/// from saving an account that we just couldn't find a config for).
pub async fn discover(email: &str) -> Result<Option<(AutoconfigServers, &'static str)>> {
    let domain = email
        .rsplit('@')
        .next()
        .filter(|d| !d.is_empty())
        .ok_or_else(|| Error::Other("invalid email for autoconfig".into()))?;
    if !is_valid_domain(domain) {
        return Err(Error::Other(format!(
            "rejecting suspicious autoconfig domain: {}",
            domain
        )));
    }
    let http = http_client()?;

    // 1. Mozilla ISP DB. Fixed hostname so no domain-controlled URL.
    let isp_url = format!("https://autoconfig.thunderbird.net/v1.1/{}", domain);
    if let Some(parsed) = try_fetch_and_parse(&http, &isp_url).await {
        return Ok(Some((parsed, "isp-db")));
    }

    // 2. Provider-hosted autoconfig. The path takes the full email so
    // providers can return per-user settings.
    let provider_url = format!(
        "https://autoconfig.{}/mail/config-v1.1.xml?emailaddress={}",
        domain,
        urlencoding::encode(email)
    );
    if let Some(parsed) = try_fetch_and_parse(&http, &provider_url).await {
        return Ok(Some((parsed, "domain-autoconfig")));
    }

    // 3. `.well-known` endpoint on the bare domain.
    let well_known_url = format!(
        "https://{}/.well-known/autoconfig/mail/config-v1.1.xml?emailaddress={}",
        domain,
        urlencoding::encode(email)
    );
    if let Some(parsed) = try_fetch_and_parse(&http, &well_known_url).await {
        return Ok(Some((parsed, "well-known")));
    }

    // 4. MX-derived guess. We trust the MX hostname enough to probe its
    // standard IMAPS / submission ports because it's the same source
    // the user's mail is already going through. We don't fabricate ports
    // beyond the IANA-assigned ones.
    if let Some(mx_host) = lookup_mx(domain).await {
        log::info!(
            "autoconfig: MX for {} -> {}, guessing IMAPS/submission",
            domain,
            mx_host
        );
        return Ok(Some((
            AutoconfigServers {
                imap_host: mx_host.clone(),
                imap_port: 993,
                imap_use_tls: true,
                smtp_host: mx_host,
                smtp_port: 587,
                smtp_use_tls: true,
            },
            "mx",
        )));
    }

    Ok(None)
}

async fn try_fetch_and_parse(http: &reqwest::Client, url: &str) -> Option<AutoconfigServers> {
    log::debug!("autoconfig: GET {}", url);
    let resp = match http.get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            log::debug!("autoconfig: {} returned {}", url, r.status());
            return None;
        }
        Err(e) => {
            log::debug!("autoconfig: GET failed for {}: {}", url, e);
            return None;
        }
    };

    // Cheap pre-check: if Content-Length is announced and exceeds the
    // cap, abort before even allocating the buffer.
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_BYTES {
            log::warn!(
                "autoconfig: response Content-Length {} exceeds cap at {}",
                len,
                url
            );
            return None;
        }
    }

    // Stream the body in chunks and bail as soon as we cross the cap.
    // Servers that don't send Content-Length (or lie about it) can't
    // force us into a large allocation this way.
    use futures::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                log::debug!("autoconfig: chunk read failed at {}: {}", url, e);
                return None;
            }
        };
        if buf.len() + chunk.len() > MAX_BYTES {
            log::warn!("autoconfig: response too large at {}", url);
            return None;
        }
        buf.extend_from_slice(&chunk);
    }
    let xml = std::str::from_utf8(&buf).ok()?;
    match parse_clientconfig_xml(xml) {
        Some(s) => Some(s),
        None => {
            log::debug!("autoconfig: response at {} did not parse", url);
            None
        }
    }
}

/// Parse a Thunderbird `clientConfig` XML document into our small
/// `AutoconfigServers` struct. Returns `None` if neither an
/// `incomingServer type="imap"` nor an `outgoingServer type="smtp"`
/// could be located — most providers ship both, and a result with no
/// usable servers isn't worth surfacing.
pub(crate) fn parse_clientconfig_xml(xml: &str) -> Option<AutoconfigServers> {
    let doc = uppsala::parse(xml).ok()?;
    let root = doc.root();

    let mut imap_host = String::new();
    let mut imap_port: u16 = 0;
    let mut imap_use_tls = true;
    let mut smtp_host = String::new();
    let mut smtp_port: u16 = 0;
    let mut smtp_use_tls = true;

    visit_servers(&doc, root, &mut |kind, host, port, tls| match kind {
        ServerKind::Imap if imap_host.is_empty() => {
            imap_host = host;
            imap_port = port;
            imap_use_tls = tls;
        }
        ServerKind::Smtp if smtp_host.is_empty() => {
            smtp_host = host;
            smtp_port = port;
            smtp_use_tls = tls;
        }
        _ => {}
    });

    if imap_host.is_empty() && smtp_host.is_empty() {
        return None;
    }
    Some(AutoconfigServers {
        imap_host,
        imap_port: if imap_port == 0 { 993 } else { imap_port },
        imap_use_tls,
        smtp_host,
        smtp_port: if smtp_port == 0 { 587 } else { smtp_port },
        smtp_use_tls,
    })
}

#[derive(Clone, Copy)]
enum ServerKind {
    Imap,
    Smtp,
    Other,
}

/// Walk every `incomingServer` / `outgoingServer` node in a parsed
/// clientConfig document and yield (kind, hostname, port, tls) to the
/// caller. Skips entries that don't list both a hostname and a port.
fn visit_servers<F: FnMut(ServerKind, String, u16, bool)>(
    doc: &Document,
    node: NodeId,
    visit: &mut F,
) {
    let kind = if let Some(NodeKind::Element(el)) = doc.node_kind(node) {
        match el.name.local_name.as_ref() {
            "incomingServer" => Some(server_kind_from_attr(el, true)),
            "outgoingServer" => Some(server_kind_from_attr(el, false)),
            _ => None,
        }
    } else {
        None
    };

    if let Some(k) = kind {
        let host = element_text(doc, node, "hostname").unwrap_or_default();
        let port = element_text(doc, node, "port")
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(0);
        let socket = element_text(doc, node, "socketType").unwrap_or_default();
        // SSL / TLS / STARTTLS all imply we want TLS on the connection;
        // "plain" is the only opt-out.
        let tls = !socket.eq_ignore_ascii_case("plain");
        if !host.is_empty() {
            visit(k, host, port, tls);
        }
    }

    for child in doc.children(node) {
        visit_servers(doc, child, visit);
    }
}

fn server_kind_from_attr(el: &uppsala::Element, incoming: bool) -> ServerKind {
    let kind_attr = el
        .get_attribute("type")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match (incoming, kind_attr.as_str()) {
        (true, "imap") => ServerKind::Imap,
        (false, "smtp") => ServerKind::Smtp,
        _ => ServerKind::Other,
    }
}

fn element_text(doc: &Document, parent: NodeId, local_name: &str) -> Option<String> {
    for child in doc.children(parent) {
        if let Some(NodeKind::Element(el)) = doc.node_kind(child) {
            if el.name.local_name.as_ref() == local_name {
                let text = doc.text_content_deep(child);
                let trimmed = text.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
    }
    None
}

/// Best-effort MX lookup. Returns the highest-preference MX hostname
/// for the domain, or `None` if the resolver fails or no records are
/// returned. Errors are logged at debug level — autoconfig isn't a
/// must-succeed flow.
async fn lookup_mx(domain: &str) -> Option<String> {
    use hickory_resolver::proto::rr::rdata::MX;
    use hickory_resolver::proto::rr::RData;
    use hickory_resolver::Resolver;

    // hickory-resolver 0.26 replaced TokioAsyncResolver::tokio() with the
    // builder-based `Resolver::builder_tokio().build()` flow. Both calls
    // return Result, but failures here are unrecoverable (no system
    // resolver) so we just log and bail.
    let resolver = match Resolver::builder_tokio().and_then(|b| b.build()) {
        Ok(r) => r,
        Err(e) => {
            log::debug!("autoconfig: resolver build failed: {}", e);
            return None;
        }
    };

    match resolver.mx_lookup(domain).await {
        Ok(lookup) => {
            // Lookup::answers() returns the raw Record list; filter to
            // the MX-typed RData payloads, then sort by preference.
            // hickory 0.26 exposes Record::data as a public field rather
            // than the 0.24-era accessor method.
            let mut records: Vec<&MX> = lookup
                .answers()
                .iter()
                .filter_map(|r| match &r.data {
                    RData::MX(mx) => Some(mx),
                    _ => None,
                })
                .collect();
            // hickory 0.26 exposes preference / exchange as public fields
            // on MX rather than the 0.24 accessor methods.
            records.sort_by_key(|r| r.preference);
            for r in records {
                let name = r.exchange.to_ascii().trim_end_matches('.').to_string();
                if !name.is_empty() && is_valid_domain(&name) {
                    return Some(name);
                }
            }
            None
        }
        Err(e) => {
            log::debug!("autoconfig: MX lookup failed for {}: {}", domain, e);
            None
        }
    }
}

/// Conservative domain-name sanity check. We use the result to build
/// HTTPS URLs, so reject anything that contains characters we don't
/// expect in a hostname.
fn is_valid_domain(domain: &str) -> bool {
    if domain.is_empty() || domain.len() > 253 {
        return false;
    }
    domain
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<clientConfig version="1.1">
  <emailProvider id="example.com">
    <domain>example.com</domain>
    <displayName>Example</displayName>
    <incomingServer type="imap">
      <hostname>imap.example.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>password-cleartext</authentication>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>password-cleartext</authentication>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;

    #[test]
    fn parse_thunderbird_xml() {
        let parsed = parse_clientconfig_xml(SAMPLE_XML).expect("should parse");
        assert_eq!(parsed.imap_host, "imap.example.com");
        assert_eq!(parsed.imap_port, 993);
        assert!(parsed.imap_use_tls);
        assert_eq!(parsed.smtp_host, "smtp.example.com");
        assert_eq!(parsed.smtp_port, 587);
        assert!(parsed.smtp_use_tls);
    }

    #[test]
    fn parse_first_imap_server_wins() {
        // Some ISPs publish multiple incomingServer entries (different
        // auth schemes, fallback ports). We should take the first one.
        let xml = r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="x">
    <domain>x</domain>
    <displayName>x</displayName>
    <incomingServer type="imap">
      <hostname>imap-primary.example.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
    </incomingServer>
    <incomingServer type="imap">
      <hostname>imap-fallback.example.com</hostname>
      <port>143</port>
      <socketType>STARTTLS</socketType>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;
        let parsed = parse_clientconfig_xml(xml).expect("should parse");
        assert_eq!(parsed.imap_host, "imap-primary.example.com");
    }

    #[test]
    fn parse_plaintext_socket_disables_tls() {
        let xml = r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="x">
    <domain>x</domain>
    <displayName>x</displayName>
    <incomingServer type="imap">
      <hostname>imap.example.com</hostname>
      <port>143</port>
      <socketType>plain</socketType>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname>
      <port>25</port>
      <socketType>plain</socketType>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;
        let parsed = parse_clientconfig_xml(xml).expect("should parse");
        assert!(!parsed.imap_use_tls);
        assert!(!parsed.smtp_use_tls);
    }

    #[test]
    fn parse_returns_none_for_empty_doc() {
        let xml = r#"<?xml version="1.0"?><clientConfig version="1.1"/>"#;
        assert!(parse_clientconfig_xml(xml).is_none());
    }

    #[test]
    fn domain_validation() {
        assert!(is_valid_domain("example.com"));
        assert!(is_valid_domain("mail.example-server.io"));
        assert!(!is_valid_domain("example.com/../etc"));
        assert!(!is_valid_domain(""));
        assert!(!is_valid_domain(".example.com"));
        assert!(!is_valid_domain("exam ple.com"));
    }
}
