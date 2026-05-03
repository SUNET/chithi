//! CardDAV client for syncing contacts with CardDAV servers.
//!
//! Uses the shared WebDAV infrastructure from `caldav.rs` (PROPFIND, XML parsing,
//! auth) and adds CardDAV-specific discovery, address book listing, contact
//! fetching, and vCard parsing/generation.

use crate::error::{Error, Result};
use crate::mail::caldav::{
    find_elements, find_text_in, has_descendant, parse_href_from_xml, DavAuth,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A CardDAV address book discovered from the server.
#[derive(Debug, Clone)]
pub struct CardDavAddressBook {
    pub href: String,
    pub name: String,
}

/// A contact fetched from a CardDAV address book.
#[derive(Debug, Clone)]
pub struct CardDavContact {
    pub href: String,
    pub etag: String,
    pub uid: String,
    pub vcard_data: String,
}

/// Parsed contact fields extracted from a vCard.
#[derive(Debug, Clone, Default)]
pub struct ParsedVCard {
    pub display_name: String,
    pub emails: Vec<VCardEmail>,
    pub phones: Vec<VCardPhone>,
    pub organization: Option<String>,
    pub title: Option<String>,
    pub note: Option<String>,
    pub uid: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VCardEmail {
    pub email: String,
    pub label: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VCardPhone {
    pub number: String,
    pub label: String,
}

// ---------------------------------------------------------------------------
// CardDAV client
// ---------------------------------------------------------------------------

/// A CardDAV client that reuses the WebDAV HTTP/auth pattern from CalDAV.
pub struct CardDavClient {
    http: reqwest::Client,
    base_url: String,
    auth: DavAuth,
}

impl CardDavClient {
    /// Create a new CardDAV client with Basic auth.
    pub async fn connect(
        carddav_url: &str,
        username: &str,
        password: &str,
        email: &str,
    ) -> Result<Self> {
        let http = crate::mail::dav_http::build_client()?;

        let auth = DavAuth::Basic {
            username: username.to_string(),
            password: password.to_string(),
        };

        let base_url = if carddav_url.is_empty() {
            log::info!("carddav: no URL configured, attempting auto-discovery");
            auto_discover(&http, &auth, email).await?
        } else {
            crate::mail::url_validation::require_https(carddav_url)?;
            carddav_url.to_string()
        };

        log::info!("carddav: connected to {}", base_url);
        Ok(Self {
            http,
            base_url,
            auth,
        })
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            DavAuth::Basic { username, password } => req.basic_auth(username, Some(password)),
            DavAuth::Bearer { token } => req.bearer_auth(token),
        }
    }

    async fn propfind(&self, url: &str, depth: &str, body: &str) -> Result<String> {
        let resp = self
            .apply_auth(
                self.http
                    .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), url),
            )
            .header("Depth", depth)
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| Error::Other(format!("CardDAV PROPFIND failed for {}: {}", url, e)))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("CardDAV PROPFIND read failed: {}", e)))?;

        if !status.is_success() && status.as_u16() != 207 {
            return Err(Error::Other(format!(
                "CardDAV PROPFIND {} returned {}: {}",
                url,
                status,
                text.chars().take(500).collect::<String>()
            )));
        }

        Ok(text)
    }

    /// Resolve a potentially relative URL against the base URL. Rejects
    /// cleartext schemes via `require_https` so a server can't downgrade
    /// subsequent auth-bearing requests by returning absolute `http://` hrefs.
    fn resolve_url(&self, href: &str) -> Result<String> {
        let resolved = if href.starts_with("http://") || href.starts_with("https://") {
            href.to_string()
        } else if let Ok(base) = url::Url::parse(&self.base_url) {
            let port_str = base.port().map(|p| format!(":{}", p)).unwrap_or_default();
            format!(
                "{}://{}{}{}",
                base.scheme(),
                base.host_str().unwrap_or(""),
                port_str,
                href
            )
        } else {
            format!("{}{}", self.base_url.trim_end_matches('/'), href)
        };
        crate::mail::url_validation::require_https(&resolved)?;
        Ok(resolved)
    }

    /// Discover the current user's principal URL.
    pub async fn discover_principal(&self) -> Result<String> {
        let resp = self
            .propfind(&self.base_url, "0", PROPFIND_PRINCIPAL)
            .await?;
        let principal = parse_href_from_xml(&resp, "current-user-principal")
            .ok_or_else(|| Error::Other("CardDAV: no current-user-principal".to_string()))?;
        self.resolve_url(&principal)
    }

    /// Discover the addressbook home set URL.
    pub async fn discover_addressbook_home(&self, principal_url: &str) -> Result<String> {
        let resp = self
            .propfind(principal_url, "0", PROPFIND_ADDRESSBOOK_HOME)
            .await?;
        let home = parse_href_from_xml(&resp, "addressbook-home-set")
            .ok_or_else(|| Error::Other("CardDAV: no addressbook-home-set".to_string()))?;
        self.resolve_url(&home)
    }

    /// List all address books.
    pub async fn list_addressbooks(&self) -> Result<Vec<CardDavAddressBook>> {
        let principal = self.discover_principal().await?;
        let home = self.discover_addressbook_home(&principal).await?;
        log::debug!("carddav: listing address books at {}", home);

        let resp = self
            .propfind(&home, "1", PROPFIND_LIST_ADDRESSBOOKS)
            .await?;
        let books = parse_addressbooks_from_xml(&resp);
        log::info!("carddav: found {} address books", books.len());
        Ok(books)
    }

    /// Fetch all contacts from an address book.
    pub async fn fetch_contacts(&self, book_href: &str) -> Result<Vec<CardDavContact>> {
        let url = self.resolve_url(book_href)?;
        log::debug!("carddav: fetching contacts from {}", url);

        let resp = self
            .apply_auth(
                self.http
                    .request(reqwest::Method::from_bytes(b"REPORT").unwrap(), &url),
            )
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(REPORT_ADDRESSBOOK_QUERY)
            .send()
            .await
            .map_err(|e| Error::Other(format!("CardDAV REPORT failed: {}", e)))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("CardDAV REPORT read failed: {}", e)))?;

        if !status.is_success() && status.as_u16() != 207 {
            return Err(Error::Other(format!(
                "CardDAV REPORT returned {}: {}",
                status,
                body.chars().take(500).collect::<String>()
            )));
        }

        let contacts = parse_contacts_from_xml(&body);
        log::info!(
            "carddav: fetched {} contacts from {}",
            contacts.len(),
            book_href
        );
        Ok(contacts)
    }

    /// PUT a vCard to the server. Returns the new etag.
    pub async fn put_contact(
        &self,
        book_href: &str,
        uid: &str,
        vcard_data: &str,
    ) -> Result<String> {
        let book_url = self.resolve_url(book_href)?;
        let url = format!("{}/{}.vcf", book_url.trim_end_matches('/'), uid);
        log::info!("carddav: PUT contact to {}", url);

        let resp = self
            .apply_auth(self.http.put(&url))
            .header("Content-Type", "text/vcard; charset=utf-8")
            .body(vcard_data.to_string())
            .send()
            .await
            .map_err(|e| Error::Other(format!("CardDAV PUT failed: {}", e)))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "CardDAV PUT returned {}: {}",
                status,
                body.chars().take(500).collect::<String>()
            )));
        }

        Ok(resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string())
    }

    /// DELETE a contact from the server.
    pub async fn delete_contact(&self, contact_href: &str) -> Result<()> {
        let url = self.resolve_url(contact_href)?;
        let resp = self
            .apply_auth(self.http.delete(&url))
            .send()
            .await
            .map_err(|e| Error::Other(format!("CardDAV DELETE failed: {}", e)))?;
        let status = resp.status();
        if !status.is_success() && status.as_u16() != 204 {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "CardDAV DELETE returned {}: {}",
                status,
                body.chars().take(500).collect::<String>()
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Auto-discovery
// ---------------------------------------------------------------------------

pub(crate) async fn auto_discover(
    http: &reqwest::Client,
    auth: &DavAuth,
    email: &str,
) -> Result<String> {
    let domain = email
        .rsplit('@')
        .next()
        .ok_or_else(|| Error::Other("Invalid email for CardDAV discovery".to_string()))?;
    auto_discover_hosts(http, auth, &[domain.to_string(), format!("mail.{}", domain)]).await
}

/// Same as `auto_discover`, but probes a caller-supplied list of
/// hostnames. Used by the Settings auto-discover command (#43) so it
/// can also try the IMAP and SMTP server hostnames the user already
/// entered.
pub(crate) async fn auto_discover_hosts(
    http: &reqwest::Client,
    auth: &DavAuth,
    hosts: &[String],
) -> Result<String> {
    for host in hosts {
        if host.is_empty() {
            continue;
        }
        let url = format!("https://{}/.well-known/carddav", host);
        log::debug!("carddav: trying auto-discovery at {}", url);
        let req = http.request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url);
        let req = match auth {
            DavAuth::Basic { username, password } => req.basic_auth(username, Some(password)),
            DavAuth::Bearer { token } => req.bearer_auth(token),
        };
        match req
            .header("Depth", "0")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(PROPFIND_PRINCIPAL)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                let final_url = resp.url().clone();
                if status.is_success() || status.as_u16() == 207 {
                    let port_str = final_url
                        .port()
                        .map(|p| format!(":{}", p))
                        .unwrap_or_default();
                    let discovered = format!(
                        "{}://{}{}{}",
                        final_url.scheme(),
                        final_url.host_str().unwrap_or(host),
                        port_str,
                        final_url.path().trim_end_matches('/')
                    );
                    crate::mail::url_validation::require_https(&discovered)?;
                    log::info!("carddav: auto-discovered URL: {}", discovered);
                    return Ok(discovered);
                }
            }
            Err(e) => log::debug!("carddav: discovery failed for {}: {}", url, e),
        }
    }

    Err(Error::Other("CardDAV auto-discovery failed".to_string()))
}

// ---------------------------------------------------------------------------
// XML request bodies
// ---------------------------------------------------------------------------

const PROPFIND_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:current-user-principal/>
  </d:prop>
</d:propfind>"#;

const PROPFIND_ADDRESSBOOK_HOME: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:cr="urn:ietf:params:xml:ns:carddav">
  <d:prop>
    <cr:addressbook-home-set/>
  </d:prop>
</d:propfind>"#;

const PROPFIND_LIST_ADDRESSBOOKS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:cr="urn:ietf:params:xml:ns:carddav">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
  </d:prop>
</d:propfind>"#;

const REPORT_ADDRESSBOOK_QUERY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<cr:addressbook-query xmlns:d="DAV:" xmlns:cr="urn:ietf:params:xml:ns:carddav">
  <d:prop>
    <d:getetag/>
    <cr:address-data/>
  </d:prop>
</cr:addressbook-query>"#;

// ---------------------------------------------------------------------------
// XML parsing
// ---------------------------------------------------------------------------

fn parse_addressbooks_from_xml(xml: &str) -> Vec<CardDavAddressBook> {
    let mut books = Vec::new();
    let doc = match uppsala::parse(xml) {
        Ok(d) => d,
        Err(e) => {
            log::error!("carddav: XML parse error: {:?}", e);
            return books;
        }
    };
    let root = doc.root();
    for response in &find_elements(&doc, root, "response") {
        let href = find_text_in(&doc, *response, "href").unwrap_or_default();
        if href.is_empty() {
            continue;
        }
        let resourcetypes = find_elements(&doc, *response, "resourcetype");
        let is_addressbook = resourcetypes
            .iter()
            .any(|rt| has_descendant(&doc, *rt, "addressbook"));
        if !is_addressbook {
            continue;
        }
        let name = find_text_in(&doc, *response, "displayname")
            .unwrap_or_else(|| "Address Book".to_string());
        books.push(CardDavAddressBook { href, name });
    }
    books
}

fn parse_contacts_from_xml(xml: &str) -> Vec<CardDavContact> {
    let mut contacts = Vec::new();
    let doc = match uppsala::parse(xml) {
        Ok(d) => d,
        Err(e) => {
            log::error!("carddav: XML parse error: {:?}", e);
            return contacts;
        }
    };
    let root = doc.root();
    for response in &find_elements(&doc, root, "response") {
        let href = find_text_in(&doc, *response, "href").unwrap_or_default();
        let etag = find_text_in(&doc, *response, "getetag")
            .map(|e| e.trim_matches('"').to_string())
            .unwrap_or_default();
        let vcard_data = find_text_in(&doc, *response, "address-data").unwrap_or_default();
        if vcard_data.is_empty() {
            continue;
        }
        let uid = extract_uid_from_vcard(&vcard_data).unwrap_or_else(|| href.clone());
        contacts.push(CardDavContact {
            href,
            etag,
            uid,
            vcard_data,
        });
    }
    contacts
}

fn extract_uid_from_vcard(vcard: &str) -> Option<String> {
    for line in vcard.lines() {
        if let Some(uid) = line.trim().strip_prefix("UID:") {
            return Some(uid.trim().to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// vCard parsing — extract structured fields from vCard 3.0/4.0
// ---------------------------------------------------------------------------

/// Parse a vCard string into structured contact fields.
pub fn parse_vcard(vcard: &str) -> ParsedVCard {
    let mut result = ParsedVCard::default();

    // Unfold continuation lines (RFC 6350 §3.2)
    let mut unfolded: Vec<String> = Vec::new();
    for line in vcard.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = unfolded.last_mut() {
                last.push_str(line[1..].trim_end());
            }
        } else {
            unfolded.push(line.trim_end().to_string());
        }
    }

    for line in &unfolded {
        let (prop_params, value) = match line.split_once(':') {
            Some((p, v)) => (p, v.trim()),
            None => continue,
        };
        let prop_upper = prop_params.to_uppercase();
        let params_lower = prop_params.to_lowercase();

        if prop_upper == "FN" || prop_upper.starts_with("FN;") {
            result.display_name = value.to_string();
        } else if prop_upper == "UID" {
            result.uid = Some(value.to_string());
        } else if prop_upper.starts_with("EMAIL") {
            let label = if params_lower.contains("work") {
                "work"
            } else if params_lower.contains("home") {
                "home"
            } else {
                "other"
            };
            result.emails.push(VCardEmail {
                email: value.to_string(),
                label: label.to_string(),
            });
        } else if prop_upper.starts_with("TEL") {
            let label = if params_lower.contains("cell") || params_lower.contains("mobile") {
                "mobile"
            } else if params_lower.contains("work") {
                "work"
            } else if params_lower.contains("home") {
                "home"
            } else {
                "other"
            };
            result.phones.push(VCardPhone {
                number: value.to_string(),
                label: label.to_string(),
            });
        } else if prop_upper == "ORG" || prop_upper.starts_with("ORG;") {
            let org = value.split(';').next().unwrap_or(value).trim();
            if !org.is_empty() {
                result.organization = Some(org.to_string());
            }
        } else if (prop_upper == "TITLE" || prop_upper.starts_with("TITLE;")) && !value.is_empty() {
            result.title = Some(value.to_string());
        } else if (prop_upper == "NOTE" || prop_upper.starts_with("NOTE;")) && !value.is_empty() {
            result.note = Some(value.to_string());
        }
    }

    // Fallback: build display_name from N property
    if result.display_name.is_empty() {
        for line in &unfolded {
            let (prop, value) = match line.split_once(':') {
                Some((p, v)) => (p.to_uppercase(), v.trim()),
                None => continue,
            };
            if prop == "N" || prop.starts_with("N;") {
                let parts: Vec<&str> = value.split(';').collect();
                let last = parts.first().unwrap_or(&"").trim();
                let first = parts.get(1).unwrap_or(&"").trim();
                let middle = parts.get(2).unwrap_or(&"").trim();
                result.display_name = [first, middle, last]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" ");
                break;
            }
        }
    }

    result
}

/// Generate a minimal vCard 3.0 from contact fields.
pub fn generate_vcard(
    uid: &str,
    display_name: &str,
    emails: &[VCardEmail],
    phones: &[VCardPhone],
    organization: Option<&str>,
    title: Option<&str>,
    note: Option<&str>,
) -> String {
    let mut lines = vec![
        "BEGIN:VCARD".to_string(),
        "VERSION:3.0".to_string(),
        format!("UID:{}", uid),
        format!("FN:{}", display_name),
    ];

    let parts: Vec<&str> = display_name.splitn(2, ' ').collect();
    let (first, last) = if parts.len() == 2 {
        (parts[0], parts[1])
    } else {
        (display_name, "")
    };
    lines.push(format!("N:{};{};;;", last, first));

    for e in emails {
        let t = match e.label.as_str() {
            "work" => "WORK",
            "home" => "HOME",
            _ => "OTHER",
        };
        lines.push(format!("EMAIL;TYPE={}:{}", t, e.email));
    }
    for p in phones {
        let t = match p.label.as_str() {
            "mobile" => "CELL",
            "work" => "WORK",
            "home" => "HOME",
            _ => "OTHER",
        };
        lines.push(format!("TEL;TYPE={}:{}", t, p.number));
    }
    if let Some(org) = organization {
        if !org.is_empty() {
            lines.push(format!("ORG:{}", org));
        }
    }
    if let Some(t) = title {
        if !t.is_empty() {
            lines.push(format!("TITLE:{}", t));
        }
    }
    if let Some(n) = note {
        if !n.is_empty() {
            lines.push(format!("NOTE:{}", n));
        }
    }

    lines.push("END:VCARD".to_string());
    lines.join("\r\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod connect_tests {
    use super::*;

    #[tokio::test]
    async fn connect_rejects_http_url() {
        let msg = match CardDavClient::connect("http://example.com/dav/", "u", "p", "u@example.com")
            .await
        {
            Ok(_) => String::new(),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("https"), "expected scheme error, got: {}", msg);
    }

    fn client_with_base(base: &str) -> CardDavClient {
        CardDavClient {
            http: reqwest::Client::new(),
            base_url: base.to_string(),
            auth: DavAuth::Basic {
                username: "u".into(),
                password: "p".into(),
            },
        }
    }

    #[test]
    fn resolve_url_rejects_absolute_http_href() {
        let client = client_with_base("https://example.com/dav/");
        let msg = match client.resolve_url("http://evil.example.com/path") {
            Ok(_) => String::new(),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("https"), "expected scheme error, got: {}", msg);
    }

    #[test]
    fn resolve_url_resolves_relative_href_with_port() {
        let client = client_with_base("https://example.com:8443/dav/");
        let resolved = client
            .resolve_url("/addressbooks/u/default/")
            .expect("expected Ok");
        assert_eq!(resolved, "https://example.com:8443/addressbooks/u/default/");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_vcard_basic() {
        let vcard = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:abc123\r\nFN:Alice Smith\r\nEMAIL;TYPE=WORK:alice@example.com\r\nTEL;TYPE=CELL:+1-555-0100\r\nORG:Acme Corp\r\nTITLE:Engineer\r\nNOTE:A note\r\nEND:VCARD";
        let parsed = parse_vcard(vcard);
        assert_eq!(parsed.display_name, "Alice Smith");
        assert_eq!(parsed.uid, Some("abc123".to_string()));
        assert_eq!(parsed.emails.len(), 1);
        assert_eq!(parsed.emails[0].email, "alice@example.com");
        assert_eq!(parsed.emails[0].label, "work");
        assert_eq!(parsed.phones.len(), 1);
        assert_eq!(parsed.phones[0].number, "+1-555-0100");
        assert_eq!(parsed.phones[0].label, "mobile");
        assert_eq!(parsed.organization, Some("Acme Corp".to_string()));
        assert_eq!(parsed.title, Some("Engineer".to_string()));
        assert_eq!(parsed.note, Some("A note".to_string()));
    }

    #[test]
    fn test_parse_vcard_fallback_to_n() {
        let vcard =
            "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Doe;John;M;;\r\nEMAIL:john@example.com\r\nEND:VCARD";
        let parsed = parse_vcard(vcard);
        assert_eq!(parsed.display_name, "John M Doe");
        assert_eq!(parsed.emails[0].label, "other");
    }

    #[test]
    fn test_parse_vcard_multiple_emails() {
        let vcard = "BEGIN:VCARD\r\nFN:Bob\r\nEMAIL;TYPE=WORK:bob@work.com\r\nEMAIL;TYPE=HOME:bob@home.com\r\nEND:VCARD";
        let parsed = parse_vcard(vcard);
        assert_eq!(parsed.emails.len(), 2);
        assert_eq!(parsed.emails[0].label, "work");
        assert_eq!(parsed.emails[1].label, "home");
    }

    #[test]
    fn test_parse_vcard_folded_lines() {
        let vcard = "BEGIN:VCARD\r\nFN:Alice\r\nNOTE:This is a long note\r\n  that continues here\r\nEND:VCARD";
        let parsed = parse_vcard(vcard);
        assert_eq!(
            parsed.note,
            Some("This is a long note that continues here".to_string())
        );
    }

    #[test]
    fn test_generate_vcard_roundtrip() {
        let emails = vec![
            VCardEmail {
                email: "a@work.com".to_string(),
                label: "work".to_string(),
            },
            VCardEmail {
                email: "a@home.com".to_string(),
                label: "home".to_string(),
            },
        ];
        let phones = vec![VCardPhone {
            number: "+1-555".to_string(),
            label: "mobile".to_string(),
        }];
        let vcard = generate_vcard(
            "uid-1",
            "Alice Smith",
            &emails,
            &phones,
            Some("Acme"),
            Some("CTO"),
            None,
        );
        let parsed = parse_vcard(&vcard);
        assert_eq!(parsed.display_name, "Alice Smith");
        assert_eq!(parsed.emails.len(), 2);
        assert_eq!(parsed.phones.len(), 1);
        assert_eq!(parsed.organization, Some("Acme".to_string()));
    }
}
