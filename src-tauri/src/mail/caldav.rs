//! CalDAV client for syncing calendars and events with CalDAV servers.
//!
//! Uses raw HTTP requests (reqwest) with XML payloads to communicate with
//! CalDAV servers. Supports discovery, listing calendars, fetching events,
//! creating/updating events, and deleting events.

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration needed to connect to a CalDAV server.
pub struct CalDavConfig {
    pub caldav_url: String, // e.g., "https://mail.example.com/dav/cal"
    pub username: String,
    pub password: String,
    pub email: String, // Used for domain extraction during auto-discovery
}

/// A CalDAV client that holds an HTTP client and connection details.
pub struct CalDavClient {
    http: reqwest::Client,
    base_url: String,
    username: String,
    password: String,
}

/// A calendar collection discovered from the CalDAV server.
#[derive(Debug, Clone)]
pub struct CalDavCalendar {
    /// Calendar collection URL path (href).
    pub href: String,
    /// Display name of the calendar.
    pub name: String,
    /// Calendar color (Apple extension), if available.
    pub color: Option<String>,
}

/// An event fetched from a CalDAV calendar.
#[derive(Debug, Clone)]
pub struct CalDavEvent {
    /// Event resource URL path (href).
    pub href: String,
    /// ETag for change detection.
    pub etag: String,
    /// iCalendar UID of the event.
    pub uid: String,
    /// Raw iCalendar text data.
    pub ical_data: String,
}

// ---------------------------------------------------------------------------
// XML namespace constants
// ---------------------------------------------------------------------------

// XML namespace URIs used in CalDAV/WebDAV (kept for reference).
#[allow(dead_code)]
const NS_DAV: &str = "DAV:";
#[allow(dead_code)]
const NS_CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
#[allow(dead_code)]
const NS_APPLE_ICAL: &str = "http://apple.com/ns/ical/";

// ---------------------------------------------------------------------------
// CalDavClient implementation
// ---------------------------------------------------------------------------

impl CalDavClient {
    /// Create a new CalDAV client. If `caldav_url` is empty, attempt
    /// auto-discovery via `.well-known/caldav`.
    pub async fn connect(config: &CalDavConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|e| Error::Other(format!("Failed to create HTTP client: {}", e)))?;

        let base_url = if config.caldav_url.is_empty() {
            log::info!("caldav: no URL configured, attempting auto-discovery");
            Self::auto_discover(&http, &config.username, &config.password, &config.email)
                .await?
        } else {
            config.caldav_url.clone()
        };

        log::info!("caldav: connected to {}", base_url);

        Ok(Self {
            http,
            base_url,
            username: config.username.clone(),
            password: config.password.clone(),
        })
    }

    /// Auto-discover CalDAV URL by trying `.well-known/caldav` on the email domain.
    async fn auto_discover(
        http: &reqwest::Client,
        username: &str,
        password: &str,
        email: &str,
    ) -> Result<String> {
        let domain = email
            .rsplit('@')
            .next()
            .ok_or_else(|| Error::Other("Invalid email for CalDAV discovery".to_string()))?;

        // Try the bare domain first, then mail.<domain>
        let candidates = vec![
            format!("https://{}/.well-known/caldav", domain),
            format!("https://mail.{}/.well-known/caldav", domain),
        ];

        for url in &candidates {
            log::debug!("caldav: trying auto-discovery at {}", url);
            match http
                .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), url)
                .basic_auth(username, Some(password))
                .header("Depth", "0")
                .header("Content-Type", "application/xml; charset=utf-8")
                .body(PROPFIND_CURRENT_USER_PRINCIPAL)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    let final_url = resp.url().clone();
                    if status.is_success() || status.as_u16() == 207 {
                        // The redirect target (or final URL) is our CalDAV base
                        let discovered = format!(
                            "{}://{}{}",
                            final_url.scheme(),
                            final_url.host_str().unwrap_or(domain),
                            final_url.path().trim_end_matches('/')
                        );
                        log::info!("caldav: auto-discovered URL: {}", discovered);
                        return Ok(discovered);
                    }
                    log::debug!(
                        "caldav: {} returned status {} (final: {})",
                        url,
                        status,
                        final_url
                    );
                }
                Err(e) => {
                    log::debug!("caldav: auto-discovery failed for {}: {}", url, e);
                }
            }
        }

        Err(Error::Other(
            "CalDAV auto-discovery failed: could not find CalDAV server".to_string(),
        ))
    }

    /// Discover the current user's principal URL via PROPFIND.
    pub async fn discover_principal(&self) -> Result<String> {
        log::debug!("caldav: discovering principal at {}", self.base_url);
        let resp = self
            .propfind(&self.base_url, "0", PROPFIND_CURRENT_USER_PRINCIPAL)
            .await?;

        let principal = parse_href_from_xml(&resp, "current-user-principal")
            .ok_or_else(|| {
                Error::Other("CalDAV: could not find current-user-principal in response".to_string())
            })?;

        let principal_url = self.resolve_url(&principal);
        log::info!("caldav: principal URL: {}", principal_url);
        Ok(principal_url)
    }

    /// Discover the calendar home set URL from the principal.
    pub async fn discover_calendar_home(&self, principal_url: &str) -> Result<String> {
        log::debug!("caldav: discovering calendar-home-set at {}", principal_url);
        let resp = self
            .propfind(principal_url, "0", PROPFIND_CALENDAR_HOME_SET)
            .await?;

        let home = parse_href_from_xml(&resp, "calendar-home-set")
            .ok_or_else(|| {
                Error::Other(
                    "CalDAV: could not find calendar-home-set in response".to_string(),
                )
            })?;

        let home_url = self.resolve_url(&home);
        log::info!("caldav: calendar home URL: {}", home_url);
        Ok(home_url)
    }

    /// List all calendars under the calendar home set.
    pub async fn list_calendars(&self) -> Result<Vec<CalDavCalendar>> {
        // Step 1: Discover principal
        let principal = self.discover_principal().await?;
        // Step 2: Discover calendar home
        let home = self.discover_calendar_home(&principal).await?;
        // Step 3: PROPFIND on calendar home with Depth: 1
        self.list_calendars_at(&home).await
    }

    /// List calendars at a specific calendar home URL.
    async fn list_calendars_at(&self, home_url: &str) -> Result<Vec<CalDavCalendar>> {
        log::debug!("caldav: listing calendars at {}", home_url);
        let resp = self
            .propfind(home_url, "1", PROPFIND_LIST_CALENDARS)
            .await?;

        let calendars = parse_calendars_from_xml(&resp);
        log::info!("caldav: found {} calendars", calendars.len());
        for cal in &calendars {
            log::debug!(
                "caldav:   calendar: href={} name='{}' color={:?}",
                cal.href,
                cal.name,
                cal.color
            );
        }
        Ok(calendars)
    }

    /// Fetch all events from a calendar collection using REPORT calendar-query.
    pub async fn fetch_events(&self, calendar_href: &str) -> Result<Vec<CalDavEvent>> {
        let url = self.resolve_url(calendar_href);
        log::debug!("caldav: fetching events from {}", url);

        let resp = self
            .http
            .request(reqwest::Method::from_bytes(b"REPORT").unwrap(), &url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(REPORT_CALENDAR_QUERY)
            .send()
            .await
            .map_err(|e| Error::Other(format!("CalDAV REPORT failed: {}", e)))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("CalDAV REPORT read failed: {}", e)))?;

        if !status.is_success() && status.as_u16() != 207 {
            return Err(Error::Other(format!(
                "CalDAV REPORT returned {}: {}",
                status,
                body.chars().take(500).collect::<String>()
            )));
        }

        let events = parse_events_from_xml(&body);
        log::info!(
            "caldav: fetched {} events from {}",
            events.len(),
            calendar_href
        );
        Ok(events)
    }

    /// PUT an iCalendar event to the server. Returns the new etag.
    pub async fn put_event(
        &self,
        calendar_href: &str,
        uid: &str,
        ical_data: &str,
    ) -> Result<String> {
        let event_url = format!(
            "{}/{}.ics",
            self.resolve_url(calendar_href).trim_end_matches('/'),
            uid
        );
        log::info!("caldav: PUT event to {}", event_url);

        let resp = self
            .http
            .put(&event_url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Content-Type", "text/calendar; charset=utf-8")
            .body(ical_data.to_string())
            .send()
            .await
            .map_err(|e| Error::Other(format!("CalDAV PUT failed: {}", e)))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "(no body)".to_string());
            return Err(Error::Other(format!(
                "CalDAV PUT returned {}: {}",
                status,
                body.chars().take(500).collect::<String>()
            )));
        }

        // Extract ETag from response headers
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        log::info!("caldav: PUT success, etag={}", etag);
        Ok(etag)
    }

    /// DELETE an event from the server.
    pub async fn delete_event(&self, event_href: &str) -> Result<()> {
        let url = self.resolve_url(event_href);
        log::info!("caldav: DELETE event at {}", url);

        let resp = self
            .http
            .delete(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .map_err(|e| Error::Other(format!("CalDAV DELETE failed: {}", e)))?;

        let status = resp.status();
        if !status.is_success() && status.as_u16() != 204 {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "(no body)".to_string());
            return Err(Error::Other(format!(
                "CalDAV DELETE returned {}: {}",
                status,
                body.chars().take(500).collect::<String>()
            )));
        }

        log::info!("caldav: DELETE success");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Send a PROPFIND request and return the response body.
    async fn propfind(&self, url: &str, depth: &str, body: &str) -> Result<String> {
        let resp = self
            .http
            .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Depth", depth)
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| Error::Other(format!("CalDAV PROPFIND failed for {}: {}", url, e)))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("CalDAV PROPFIND read failed: {}", e)))?;

        if !status.is_success() && status.as_u16() != 207 {
            return Err(Error::Other(format!(
                "CalDAV PROPFIND {} returned {}: {}",
                url,
                status,
                text.chars().take(500).collect::<String>()
            )));
        }

        log::debug!(
            "caldav: PROPFIND {} depth={} -> {} ({} bytes)",
            url,
            depth,
            status,
            text.len()
        );
        Ok(text)
    }

    /// Resolve a potentially relative URL against the base URL.
    fn resolve_url(&self, href: &str) -> String {
        if href.starts_with("http://") || href.starts_with("https://") {
            return href.to_string();
        }
        // Parse the base URL to get the scheme and host
        if let Ok(base) = url::Url::parse(&self.base_url) {
            let scheme = base.scheme();
            let host = base.host_str().unwrap_or("");
            let port_str = base
                .port()
                .map(|p| format!(":{}", p))
                .unwrap_or_default();
            format!("{}://{}{}{}", scheme, host, port_str, href)
        } else {
            // Fallback: just concatenate
            format!("{}{}", self.base_url.trim_end_matches('/'), href)
        }
    }
}

// ---------------------------------------------------------------------------
// XML request bodies
// ---------------------------------------------------------------------------

const PROPFIND_CURRENT_USER_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:current-user-principal/>
  </d:prop>
</d:propfind>"#;

const PROPFIND_CALENDAR_HOME_SET: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <c:calendar-home-set/>
  </d:prop>
</d:propfind>"#;

const PROPFIND_LIST_CALENDARS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:a="http://apple.com/ns/ical/">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <a:calendar-color/>
  </d:prop>
</d:propfind>"#;

const REPORT_CALENDAR_QUERY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
    <c:calendar-data/>
  </d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT"/>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#;

// ---------------------------------------------------------------------------
// XML parsing helpers using uppsala DOM parser
// ---------------------------------------------------------------------------

use uppsala::{Document, NodeId, NodeKind};

/// Find text content of a descendant element by local name.
fn find_text_in(doc: &Document, node_id: NodeId, local_name: &str) -> Option<String> {
    for child_id in doc.children(node_id) {
        if let Some(NodeKind::Element(el)) = doc.node_kind(child_id) {
            let qname = &el.name;
            if qname.local_name.as_ref() == local_name {
                let text = doc.text_content_deep(child_id);
                let t = text.trim().to_string();
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
        if let Some(found) = find_text_in(doc, child_id, local_name) {
            return Some(found);
        }
    }
    None
}

/// Check if a node has a descendant element with the given local name.
fn has_descendant(doc: &Document, node_id: NodeId, local_name: &str) -> bool {
    for child_id in doc.children(node_id) {
        if let Some(NodeKind::Element(el)) = doc.node_kind(child_id) {
            if el.name.local_name.as_ref() == local_name {
                return true;
            }
        }
        if has_descendant(doc, child_id, local_name) {
            return true;
        }
    }
    false
}

/// Find all descendant element NodeIds with a given local name.
fn find_elements(doc: &Document, node_id: NodeId, local_name: &str) -> Vec<NodeId> {
    let mut result = Vec::new();
    for child_id in doc.children(node_id) {
        if let Some(NodeKind::Element(el)) = doc.node_kind(child_id) {
            if el.name.local_name.as_ref() == local_name {
                result.push(child_id);
            }
        }
        result.extend(find_elements(doc, child_id, local_name));
    }
    result
}

/// Parse an `<href>` value nested inside a specific parent element.
fn parse_href_from_xml(xml: &str, parent_local_name: &str) -> Option<String> {
    let doc = uppsala::parse(xml).ok()?;
    let root = doc.root();
    let parents = find_elements(&doc, root, parent_local_name);
    for parent in &parents {
        if let Some(href) = find_text_in(&doc, *parent, "href") {
            return Some(href);
        }
    }
    None
}

/// Parse the list of calendars from a PROPFIND Depth:1 multistatus response.
fn parse_calendars_from_xml(xml: &str) -> Vec<CalDavCalendar> {
    let mut calendars = Vec::new();
    let doc = match uppsala::parse(xml) {
        Ok(d) => d,
        Err(e) => {
            log::error!("caldav: XML parse error in calendars: {:?}", e);
            return calendars;
        }
    };
    let root = doc.root();
    let responses = find_elements(&doc, root, "response");

    for response in &responses {
        let href = find_text_in(&doc, *response, "href").unwrap_or_default();
        if href.is_empty() {
            continue;
        }

        let resourcetypes = find_elements(&doc, *response, "resourcetype");
        let is_calendar = resourcetypes.iter().any(|rt| has_descendant(&doc, *rt, "calendar"));
        if !is_calendar {
            continue;
        }

        let name = find_text_in(&doc, *response, "displayname")
            .unwrap_or_else(|| "Calendar".to_string());

        let color = find_text_in(&doc, *response, "calendar-color").map(|c| {
            let c = c.trim().to_string();
            if c.len() == 9 && c.starts_with('#') {
                c[..7].to_string()
            } else {
                c
            }
        });

        calendars.push(CalDavCalendar { href, name, color });
    }

    calendars
}

/// Parse events from a REPORT calendar-query multistatus XML response.
fn parse_events_from_xml(xml: &str) -> Vec<CalDavEvent> {
    let mut events = Vec::new();
    let doc = match uppsala::parse(xml) {
        Ok(d) => d,
        Err(e) => {
            log::error!("caldav: XML parse error in events: {:?}", e);
            return events;
        }
    };
    let root = doc.root();
    let responses = find_elements(&doc, root, "response");

    for response in &responses {
        let href = find_text_in(&doc, *response, "href").unwrap_or_default();
        let etag = find_text_in(&doc, *response, "getetag")
            .map(|e| e.trim_matches('"').to_string())
            .unwrap_or_default();
        let ical_data = find_text_in(&doc, *response, "calendar-data").unwrap_or_default();

        if ical_data.is_empty() {
            continue;
        }

        let uid = extract_uid_from_ical(&ical_data).unwrap_or_else(|| href.clone());
        events.push(CalDavEvent { href, etag, uid, ical_data });
    }

    events
}

/// Extract the UID from raw iCalendar text.
fn extract_uid_from_ical(ical: &str) -> Option<String> {
    for line in ical.lines() {
        let line = line.trim();
        if line.starts_with("UID:") {
            return Some(line[4..].trim().to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Generate minimal iCalendar for a new event
// ---------------------------------------------------------------------------

/// Generate a minimal VCALENDAR/VEVENT iCalendar string from event fields.
pub fn generate_ical_event(
    uid: &str,
    title: &str,
    description: Option<&str>,
    location: Option<&str>,
    start_time: &str,
    end_time: &str,
    all_day: bool,
) -> String {
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");

    let dtstart = if all_day {
        format!("DTSTART;VALUE=DATE:{}", to_ical_date(start_time))
    } else {
        format!("DTSTART:{}", to_ical_datetime(start_time))
    };

    let dtend = if all_day {
        format!("DTEND;VALUE=DATE:{}", to_ical_date(end_time))
    } else {
        format!("DTEND:{}", to_ical_datetime(end_time))
    };

    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//Chithi//EN".to_string(),
        "BEGIN:VEVENT".to_string(),
        format!("UID:{}", uid),
        format!("DTSTAMP:{}", now),
        dtstart,
        dtend,
        format!("SUMMARY:{}", title),
    ];

    if let Some(desc) = description {
        if !desc.is_empty() {
            lines.push(format!("DESCRIPTION:{}", desc));
        }
    }
    if let Some(loc) = location {
        if !loc.is_empty() {
            lines.push(format!("LOCATION:{}", loc));
        }
    }

    lines.push("END:VEVENT".to_string());
    lines.push("END:VCALENDAR".to_string());

    lines.join("\r\n")
}

/// Convert ISO 8601 datetime to iCalendar datetime format.
/// "2025-04-15T10:00:00Z" -> "20250415T100000Z"
fn to_ical_datetime(iso: &str) -> String {
    iso.replace('-', "").replace(':', "")
}

/// Convert ISO 8601 date to iCalendar date format.
/// "2025-04-15" -> "20250415"
fn to_ical_date(iso: &str) -> String {
    iso.replace('-', "").chars().take(8).collect()
}
