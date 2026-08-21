//! Google REST client: Calendar API v3 and People API v1.
//!
//! Owns the wire payloads for the Google side of calendar/contact sync
//! and push (ADR 0016, ADR 0050). Token acquisition lives in
//! `ProviderCredentials`; this client just sends requests with a ready token,
//! mirroring `GraphClient`.

use crate::calendar::CalendarEvent;
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct GoogleSchedule {
    pub email: String,
    pub available: bool,
    pub busy: Vec<GoogleBusyPeriod>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GoogleBusyPeriod {
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleEndpoints {
    pub calendar_api_root: String,
    pub people_api_root: String,
}

impl Default for GoogleEndpoints {
    fn default() -> Self {
        Self {
            calendar_api_root: "https://www.googleapis.com/calendar/v3".into(),
            people_api_root: "https://people.googleapis.com/v1".into(),
        }
    }
}

pub struct GoogleClient {
    http: reqwest::Client,
    token: String,
    endpoints: GoogleEndpoints,
}

/// One page of a Calendar events listing.
pub enum EventsPage {
    /// Parsed response body (`items`, optional `nextSyncToken`).
    Page(serde_json::Value),
    /// HTTP 410 — the sync token expired; caller should clear it and
    /// run a full sync on the next cycle.
    SyncTokenExpired,
}

impl GoogleClient {
    pub fn with_client(
        http: reqwest::Client,
        access_token: &str,
        endpoints: GoogleEndpoints,
    ) -> Self {
        Self {
            http,
            token: access_token.to_string(),
            endpoints,
        }
    }

    fn calendar_url(&self, path: &str) -> String {
        endpoint_url(&self.endpoints.calendar_api_root, path)
    }

    fn people_url(&self, path: &str) -> String {
        endpoint_url(&self.endpoints.people_api_root, path)
    }

    // -----------------------------------------------------------------------
    // Calendar API v3
    // -----------------------------------------------------------------------

    /// List the user's calendars (`users/me/calendarList`). Returns the
    /// parsed response body; calendars are under `items`.
    pub async fn list_calendar_list(&self) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(self.calendar_url("users/me/calendarList"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar API failed: {}", e)))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("Google Calendar API error: {}", body)));
        }

        resp.json()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar API parse error: {}", e)))
    }

    /// Query Calendar FreeBusy for participant calendar addresses.
    pub async fn get_schedules(
        &self,
        emails: &[String],
        start: &str,
        end: &str,
    ) -> Result<Vec<GoogleSchedule>> {
        let body = serde_json::json!({
            "timeMin": start,
            "timeMax": end,
            "items": emails.iter().map(|email| serde_json::json!({ "id": email })).collect::<Vec<_>>(),
        });
        let resp = self
            .http
            .post(self.calendar_url("freeBusy"))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar FreeBusy failed: {}", e)))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google Calendar FreeBusy error: {}",
                body
            )));
        }
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar FreeBusy parse error: {}", e)))?;
        Ok(parse_google_schedules(&value, emails))
    }

    /// Incremental events listing using a stored sync token.
    pub async fn list_events_incremental(
        &self,
        calendar_id: &str,
        sync_token: &str,
    ) -> Result<EventsPage> {
        let resp = self
            .http
            .get(self.calendar_url(&format!(
                "calendars/{}/events",
                urlencoding::encode(calendar_id)
            )))
            .bearer_auth(&self.token)
            .query(&[("syncToken", sync_token)])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar events fetch failed: {}", e)))?;
        Self::events_page(resp).await
    }

    /// Full events listing over a time window (first sync, or after the
    /// sync token expired).
    pub async fn list_events_full(
        &self,
        calendar_id: &str,
        time_min: &str,
        time_max: &str,
    ) -> Result<EventsPage> {
        let resp = self
            .http
            .get(self.calendar_url(&format!(
                "calendars/{}/events",
                urlencoding::encode(calendar_id)
            )))
            .bearer_auth(&self.token)
            .query(&[
                ("timeMin", time_min),
                ("timeMax", time_max),
                ("singleEvents", "true"),
                ("maxResults", "500"),
            ])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar events fetch failed: {}", e)))?;
        Self::events_page(resp).await
    }

    async fn events_page(resp: reqwest::Response) -> Result<EventsPage> {
        if resp.status().as_u16() == 410 {
            return Ok(EventsPage::SyncTokenExpired);
        }
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google Calendar events error: {}",
                body
            )));
        }
        let data = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar events parse error: {}", e)))?;
        Ok(EventsPage::Page(data))
    }

    /// Create an event. Returns `(event_id, iCalUID)` — the iCalUID is
    /// what RSVP replies reference, so callers persist it as the local
    /// event UID.
    pub async fn create_event(
        &self,
        calendar_id: &str,
        event: &serde_json::Value,
        send_updates: &str,
    ) -> Result<(String, Option<String>)> {
        let url = self.calendar_url(&format!(
            "calendars/{}/events?sendUpdates={}",
            urlencoding::encode(calendar_id),
            send_updates
        ));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(event)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar request failed: {}", e)))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google Calendar insert failed: {}",
                body
            )));
        }
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar insert parse error: {}", e)))?;
        let remote_id = data["id"].as_str().unwrap_or_default().to_string();
        let ical_uid = data["iCalUID"].as_str().map(|s| s.to_string());
        Ok((remote_id, ical_uid))
    }

    /// Patch an event (partial update).
    pub async fn patch_event(
        &self,
        calendar_id: &str,
        event_id: &str,
        patch: &serde_json::Value,
        send_updates: &str,
    ) -> Result<()> {
        let url = self.calendar_url(&format!(
            "calendars/{}/events/{}?sendUpdates={}",
            urlencoding::encode(calendar_id),
            urlencoding::encode(event_id),
            send_updates
        ));
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(&self.token)
            .json(patch)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar request failed: {}", e)))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google Calendar PATCH failed: {}",
                body
            )));
        }
        Ok(())
    }

    /// Delete an event. 404-adjacent responses Google uses for already
    /// -gone events (204 body-less success, 410 Gone) count as success.
    pub async fn delete_event(
        &self,
        calendar_id: &str,
        event_id: &str,
        send_updates: &str,
    ) -> Result<()> {
        let url = self.calendar_url(&format!(
            "calendars/{}/events/{}?sendUpdates={}",
            urlencoding::encode(calendar_id),
            urlencoding::encode(event_id),
            send_updates
        ));
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar request failed: {}", e)))?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 204 || status.as_u16() == 410 {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(Error::Other(format!(
            "Google Calendar delete failed: {}",
            body
        )))
    }

    /// Find an event's Google id by its iCalUID. `Ok(None)` when the
    /// event is not on the calendar.
    pub async fn find_event_by_ical_uid(
        &self,
        calendar_id: &str,
        ical_uid: &str,
    ) -> Result<Option<String>> {
        let url = self.calendar_url(&format!(
            "calendars/{}/events?iCalUID={}",
            urlencoding::encode(calendar_id),
            urlencoding::encode(ical_uid)
        ));
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar request failed: {}", e)))?;
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar parse error: {}", e)))?;
        Ok(data["items"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(|e| e["id"].as_str())
            .map(|s| s.to_string()))
    }

    /// Import an existing event (preserving its iCalUID) onto a
    /// calendar. Returns the created event's Google id.
    pub async fn import_event(
        &self,
        calendar_id: &str,
        event: &serde_json::Value,
    ) -> Result<Option<String>> {
        let url = self.calendar_url(&format!(
            "calendars/{}/events/import",
            urlencoding::encode(calendar_id)
        ));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(event)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar import request failed: {}", e)))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google Calendar import failed: {}",
                body
            )));
        }
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar import parse error: {}", e)))?;
        Ok(data["id"].as_str().map(|s| s.to_string()))
    }

    /// Set a calendar's colors on the per-user calendarList entry.
    /// Google only accepts arbitrary RGB with `colorRgbFormat=true`,
    /// and omitting foregroundColor in practice resets it to the
    /// default for the new background — always send both.
    pub async fn set_calendar_color(
        &self,
        calendar_id: &str,
        background: &str,
        foreground: &str,
    ) -> Result<()> {
        let url = self.calendar_url(&format!(
            "users/me/calendarList/{}?colorRgbFormat=true",
            urlencoding::encode(calendar_id)
        ));
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "backgroundColor": background,
                "foregroundColor": foreground,
            }))
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google color push request failed: {}", e)))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google color push got {}: {}",
                status,
                body.chars().take(500).collect::<String>()
            )));
        }
        Ok(())
    }

    /// Rename a calendar (the underlying calendar resource's summary).
    pub async fn rename_calendar(&self, calendar_id: &str, name: &str) -> Result<()> {
        let url = self.calendar_url(&format!("calendars/{}", urlencoding::encode(calendar_id)));
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "summary": name }))
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google Calendar PATCH failed: {}", e)))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google rename failed: {}",
                body.chars().take(200).collect::<String>()
            )));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // People API v1
    // -----------------------------------------------------------------------

    /// Create a contact. Returns the People API `resourceName`, which
    /// callers persist as the contact's remote id.
    pub async fn create_contact(&self, person: &serde_json::Value) -> Result<String> {
        let resp = self
            .http
            .post(self.people_url("people:createContact"))
            .bearer_auth(&self.token)
            .json(person)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google create contact request failed: {}", e)))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google create contact failed: {}",
                body
            )));
        }
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Google create contact parse error: {}", e)))?;
        data["resourceName"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| Error::Other("Google create contact: no resourceName".into()))
    }

    /// Update a contact's names/emails/phones by resourceName.
    pub async fn update_contact(
        &self,
        resource_name: &str,
        person: &serde_json::Value,
    ) -> Result<()> {
        let url = self.people_url(&format!(
            "{}:updateContact?updatePersonFields=names,emailAddresses,phoneNumbers",
            resource_name
        ));
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(&self.token)
            .json(person)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google update contact request failed: {}", e)))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google update contact failed: {}",
                body
            )));
        }
        Ok(())
    }

    /// Delete a contact by resourceName.
    pub async fn delete_contact(&self, resource_name: &str) -> Result<()> {
        let url = self.people_url(&format!("{}:deleteContact", resource_name));
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google delete contact request failed: {}", e)))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google delete contact failed: {}",
                body
            )));
        }
        Ok(())
    }

    /// List the user's contacts (`people/me/connections`). Returns the
    /// parsed response body; contacts are under `connections`.
    pub async fn list_connections(&self) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(self.people_url("people/me/connections"))
            .bearer_auth(&self.token)
            .query(&[
                (
                    "personFields",
                    "names,emailAddresses,phoneNumbers,organizations",
                ),
                ("pageSize", "1000"),
            ])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google People API failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google People API error {}: {}",
                status, body
            )));
        }

        resp.json()
            .await
            .map_err(|e| Error::Other(format!("Google People API parse error: {}", e)))
    }
}

fn endpoint_url(root: &str, path: &str) -> String {
    format!("{}/{}", root.trim_end_matches('/'), path)
}

fn parse_google_schedules(value: &serde_json::Value, requested: &[String]) -> Vec<GoogleSchedule> {
    requested
        .iter()
        .map(|email| {
            let calendar = &value["calendars"][email];
            let available = calendar.is_object()
                && calendar["errors"]
                    .as_array()
                    .is_none_or(|errors| errors.is_empty());
            let busy = calendar["busy"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|period| {
                    Some(GoogleBusyPeriod {
                        start: period["start"].as_str()?.to_string(),
                        end: period["end"].as_str()?.to_string(),
                    })
                })
                .collect();
            GoogleSchedule {
                email: email.clone(),
                available,
                busy,
            }
        })
        .collect()
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_once(response_body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!(
            "http://{}/injected-calendar",
            listener.local_addr().unwrap()
        );
        let request = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut expected_length = None;
            loop {
                let mut chunk = [0; 1024];
                let count = socket.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(header_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let body_start = header_end + 4;
                    let headers = String::from_utf8_lossy(&bytes[..header_end]);
                    let content_length = expected_length.get_or_insert_with(|| {
                        headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|value| value.parse::<usize>().ok())
                            })
                            .unwrap_or(0)
                    });
                    if bytes.len() >= body_start + *content_length {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (root, request)
    }

    #[tokio::test]
    async fn create_event_uses_injected_root_client_and_google_wire_format() {
        let (calendar_root, captured) =
            serve_once(r#"{"id":"remote-event","iCalUID":"uid@example.org"}"#).await;
        let mut headers = HeaderMap::new();
        headers.insert("x-injected-client", HeaderValue::from_static("google-test"));
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap();
        let client = GoogleClient::with_client(
            http,
            "test-access-token",
            GoogleEndpoints {
                calendar_api_root: calendar_root,
                people_api_root: "http://127.0.0.1:1/unused-people".into(),
            },
        );
        let event = serde_json::json!({
            "summary": "Wire test",
            "start": { "dateTime": "2026-08-09T09:00:00Z" },
            "end": { "dateTime": "2026-08-09T10:00:00Z" },
        });

        let result = client
            .create_event("team@example.org", &event, "all")
            .await
            .unwrap();
        assert_eq!(
            result,
            ("remote-event".into(), Some("uid@example.org".into()))
        );

        let request = captured.await.unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        let request_line = head.lines().next().unwrap();
        assert_eq!(
            request_line,
            "POST /injected-calendar/calendars/team%40example.org/events?sendUpdates=all HTTP/1.1"
        );
        let headers = format!("{head}\r\n").to_ascii_lowercase();
        assert!(headers.contains("authorization: bearer test-access-token\r\n"));
        assert!(headers.contains("x-injected-client: google-test\r\n"));
        assert!(headers.contains("content-type: application/json\r\n"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            event
        );
    }
}

#[cfg(test)]
mod free_busy_tests {
    use super::*;

    #[test]
    fn parser_preserves_busy_periods_and_calendar_errors() {
        let value = serde_json::json!({ "calendars": {
            "free@example.org": { "busy": [{ "start": "2026-08-10T09:00:00Z", "end": "2026-08-10T10:00:00Z" }] },
            "hidden@example.org": { "errors": [{ "reason": "notFound" }], "busy": [] }
        }});
        let schedules = parse_google_schedules(
            &value,
            &["free@example.org".into(), "hidden@example.org".into()],
        );
        assert!(schedules[0].available);
        assert_eq!(schedules[0].busy.len(), 1);
        assert!(!schedules[1].available);
    }
}

// ---------------------------------------------------------------------------
// Payload builders (pure — unit-tested below)
// ---------------------------------------------------------------------------

/// Start/end object for an event: `{"date": ...}` for all-day events,
/// `{"dateTime": ...}` otherwise.
fn time_json(timestamp: &str, all_day: bool) -> serde_json::Value {
    if all_day {
        serde_json::json!({"date": timestamp.split('T').next().unwrap_or_default()})
    } else {
        serde_json::json!({"dateTime": timestamp})
    }
}

/// `sendUpdates` value for a push: notify attendees only when the
/// event has any.
pub fn send_updates_for(attendees_json: Option<&str>) -> &'static str {
    if attendees_json.is_some() {
        "all"
    } else {
        "none"
    }
}

/// Calendar v3 payload for creating an event. Includes the local UID
/// as iCalUID and the attendee list, so Google sends invites and RSVP
/// replies match back.
pub fn event_to_google_json(event: &CalendarEvent) -> serde_json::Value {
    let mut google_event = serde_json::json!({
        "summary": event.title,
        "start": time_json(&event.start_time, event.all_day),
        "end": time_json(&event.end_time, event.all_day),
        "iCalUID": event.uid,
    });
    if let Some(ref desc) = event.description {
        google_event["description"] = serde_json::json!(desc);
    }
    if let Some(ref loc) = event.location {
        google_event["location"] = serde_json::json!(loc);
    }
    if let Some(ref att_json) = event.attendees_json {
        if let Ok(atts) = serde_json::from_str::<Vec<serde_json::Value>>(att_json) {
            let google_attendees: Vec<serde_json::Value> = atts
                .iter()
                .filter_map(|a| a["email"].as_str().map(|e| serde_json::json!({"email": e})))
                .collect();
            if !google_attendees.is_empty() {
                google_event["attendees"] = serde_json::json!(google_attendees);
            }
        }
    }
    google_event
}

/// Calendar v3 payload for patching an event. Deliberately narrower
/// than the create payload: no iCalUID (immutable) and no attendee
/// rewrite.
pub fn event_patch_to_google_json(event: &CalendarEvent) -> serde_json::Value {
    let mut patch = serde_json::json!({
        "summary": event.title,
        "start": time_json(&event.start_time, event.all_day),
        "end": time_json(&event.end_time, event.all_day),
    });
    if let Some(ref desc) = event.description {
        patch["description"] = serde_json::json!(desc);
    }
    if let Some(ref loc) = event.location {
        patch["location"] = serde_json::json!(loc);
    }
    patch
}

/// People v1 `Person` payload from our contact fields. Shared by
/// create and update (the update masks to names/emails/phones, which
/// is exactly what this builds).
pub fn contact_to_person_json(
    display_name: &str,
    emails_json: &str,
    phones_json: &str,
) -> serde_json::Value {
    let mut person = serde_json::json!({
        "names": [{"givenName": display_name}],
    });
    if let Ok(emails) = serde_json::from_str::<Vec<serde_json::Value>>(emails_json) {
        let ge: Vec<_> = emails
            .iter()
            .filter_map(|e| {
                e["email"]
                    .as_str()
                    .map(|addr| serde_json::json!({"value": addr}))
            })
            .collect();
        if !ge.is_empty() {
            person["emailAddresses"] = serde_json::json!(ge);
        }
    }
    if let Ok(phones) = serde_json::from_str::<Vec<serde_json::Value>>(phones_json) {
        let gp: Vec<_> = phones
            .iter()
            .filter_map(|p| {
                p["number"]
                    .as_str()
                    .map(|n| serde_json::json!({"value": n}))
            })
            .collect();
        if !gp.is_empty() {
            person["phoneNumbers"] = serde_json::json!(gp);
        }
    }
    person
}

#[cfg(test)]
mod builder_tests {
    use super::*;

    fn event(all_day: bool, attendees_json: Option<&str>) -> CalendarEvent {
        CalendarEvent {
            id: "local-id".into(),
            account_id: "acct".into(),
            calendar_id: "cal".into(),
            uid: Some("uid-1@chithi".into()),
            title: "Standup".into(),
            description: Some("daily".into()),
            location: None,
            start_time: "2026-07-14T09:00:00Z".into(),
            end_time: "2026-07-14T09:15:00Z".into(),
            all_day,
            timezone: None,
            recurrence_rule: None,
            organizer_email: Some("me@example.org".into()),
            attendees_json: attendees_json.map(|s| s.to_string()),
            my_status: None,
            source_message_id: None,
            ical_data: None,
            remote_id: None,
            etag: None,
        }
    }

    #[test]
    fn timed_event_uses_datetime() {
        let v = event_to_google_json(&event(false, None));
        assert_eq!(v["start"]["dateTime"], "2026-07-14T09:00:00Z");
        assert!(v["start"]["date"].is_null());
        assert_eq!(v["iCalUID"], "uid-1@chithi");
    }

    #[test]
    fn all_day_event_uses_date_only() {
        let v = event_to_google_json(&event(true, None));
        assert_eq!(v["start"]["date"], "2026-07-14");
        assert!(v["start"]["dateTime"].is_null());
        assert_eq!(v["end"]["date"], "2026-07-14");
    }

    #[test]
    fn attendees_map_to_email_objects() {
        let v = event_to_google_json(&event(
            false,
            Some(r#"[{"email":"a@x.org","name":"A"},{"name":"no-email"}]"#),
        ));
        let atts = v["attendees"].as_array().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["email"], "a@x.org");
    }

    #[test]
    fn empty_attendee_list_is_omitted() {
        let v = event_to_google_json(&event(false, Some("[]")));
        assert!(v["attendees"].is_null());
    }

    #[test]
    fn patch_omits_ical_uid_and_attendees() {
        let v = event_patch_to_google_json(&event(false, Some(r#"[{"email":"a@x.org"}]"#)));
        assert!(v["iCalUID"].is_null());
        assert!(v["attendees"].is_null());
        assert_eq!(v["summary"], "Standup");
    }

    #[test]
    fn send_updates_follows_attendee_presence() {
        assert_eq!(send_updates_for(Some("[]")), "all");
        assert_eq!(send_updates_for(None), "none");
    }

    #[test]
    fn person_json_maps_fields() {
        let v = contact_to_person_json(
            "Ada Lovelace",
            r#"[{"email":"ada@x.org","label":"work"}]"#,
            r#"[{"number":"+4670","label":"mobile"}]"#,
        );
        assert_eq!(v["names"][0]["givenName"], "Ada Lovelace");
        assert_eq!(v["emailAddresses"][0]["value"], "ada@x.org");
        assert_eq!(v["phoneNumbers"][0]["value"], "+4670");
    }

    #[test]
    fn person_json_handles_malformed_and_empty_lists() {
        let v = contact_to_person_json("X", "not json", "[]");
        assert!(v["emailAddresses"].is_null());
        assert!(v["phoneNumbers"].is_null());
    }
}
