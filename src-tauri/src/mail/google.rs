//! Google REST client: Calendar API v3 and People API v1.
//!
//! Owns the wire payloads for the Google side of calendar/contact sync
//! and push (ADR 0016, ADR 0050). Token acquisition lives in
//! `ProviderCredentials`; this client just sends requests with a ready token,
//! mirroring `GraphClient`.

use crate::calendar::CalendarEvent;
use crate::error::{Error, Result};

const PEOPLE_PAGE_SIZE: usize = 1_000;
const PEOPLE_PAGE_SIZE_PARAMETER: &str = "1000";
const PEOPLE_BATCH_SIZE: usize = 200;
const MAX_PEOPLE_PAGES: usize = 1_000;
const MAX_PEOPLE_CONTACTS: usize = PEOPLE_PAGE_SIZE * MAX_PEOPLE_PAGES;
const PEOPLE_CHANGE_FIELDS: &str = "metadata";
const PEOPLE_PERSON_FIELDS: &str = "names,emailAddresses,phoneNumbers,organizations,metadata";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleContact {
    pub resource_name: String,
    pub contact_source_etag: String,
    pub display_name: String,
    pub emails_json: String,
    pub phones_json: String,
    pub organization: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleContactChange {
    pub resource_name: String,
    pub previous_resource_names: Vec<String>,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleContactChanges {
    pub changes: Vec<GoogleContactChange>,
    pub next_sync_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoogleContactsSync {
    Changes(GoogleContactChanges),
    SyncTokenExpired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleContactLookup {
    pub requested_resource_name: String,
    pub contact: Option<GoogleContact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GooglePushedContact {
    pub resource_name: String,
    pub contact_source_etag: String,
}

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

    /// Find an event by its iCalUID. The complete event is returned because
    /// attendee PATCHes must preserve every guest in Google's replacement array.
    pub async fn find_event_by_ical_uid(
        &self,
        calendar_id: &str,
        ical_uid: &str,
    ) -> Result<Option<serde_json::Value>> {
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
            .cloned())
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

    /// Create a contact and return the identity metadata from the mutation.
    pub async fn create_contact(&self, person: &serde_json::Value) -> Result<GooglePushedContact> {
        let response = self
            .http
            .post(self.people_url("people:createContact"))
            .bearer_auth(&self.token)
            .query(&[
                ("personFields", PEOPLE_PERSON_FIELDS),
                ("sources", "READ_SOURCE_TYPE_CONTACT"),
            ])
            .json(person)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google create contact request failed: {}", e)))?;
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google create contact failed: {}",
                body
            )));
        }
        let data: serde_json::Value = response
            .json()
            .await
            .map_err(|e| Error::Other(format!("Google create contact parse error: {}", e)))?;
        parse_pushed_contact(&data, "create response")
    }

    /// Merge locally changed fields into a fresh contact-only Person.
    pub async fn update_contact(
        &self,
        resource_name: &str,
        expected_source_etag: Option<&str>,
        person: &serde_json::Value,
    ) -> Result<GooglePushedContact> {
        validate_people_resource_name(resource_name, "update request")?;
        let current = self.get_contact_for_update(resource_name).await?;
        let current_object = current.as_object().ok_or_else(|| {
            Error::Other("Google People update preflight must return an object".into())
        })?;
        let current_resource_name =
            require_people_resource_name(current_object.get("resourceName"), "update preflight")?;
        let contact_source = require_contact_source(current_object, "update preflight")?;
        let desired = person
            .as_object()
            .ok_or_else(|| Error::Other("Google People update payload must be an object".into()))?;
        let (mut body, changed_fields) = merge_people_update(current_object, desired)?;
        if changed_fields.is_empty() {
            return Ok(GooglePushedContact {
                resource_name: current_resource_name,
                contact_source_etag: contact_source.etag,
            });
        }
        if expected_source_etag != Some(contact_source.etag.as_str()) {
            return Err(Error::Sync(
                "Google contact changed remotely since the local snapshot; sync before retrying"
                    .into(),
            ));
        }
        body.insert(
            "resourceName".into(),
            serde_json::Value::String(current_resource_name.clone()),
        );
        if let Some(etag) = optional_people_string(current_object, "etag", "Person")? {
            body.insert("etag".into(), serde_json::Value::String(etag.into()));
        }
        body.insert(
            "metadata".into(),
            serde_json::json!({"sources": [contact_source.value]}),
        );
        let update_mask = changed_fields.join(",");

        let response = self
            .http
            .patch(self.people_url(&format!("{current_resource_name}:updateContact")))
            .bearer_auth(&self.token)
            .query(&[
                ("updatePersonFields", update_mask.as_str()),
                ("personFields", PEOPLE_PERSON_FIELDS),
                ("sources", "READ_SOURCE_TYPE_CONTACT"),
            ])
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google update contact request failed: {}", e)))?;
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google update contact failed: {}",
                body
            )));
        }
        let data: serde_json::Value = response
            .json()
            .await
            .map_err(|e| Error::Other(format!("Google update contact parse error: {e}")))?;
        parse_pushed_contact(&data, "update response")
    }

    async fn get_contact_for_update(&self, resource_name: &str) -> Result<serde_json::Value> {
        let response = self
            .http
            .get(self.people_url(resource_name))
            .bearer_auth(&self.token)
            .query(&[
                ("personFields", PEOPLE_PERSON_FIELDS),
                ("sources", "READ_SOURCE_TYPE_CONTACT"),
            ])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Google contact preflight request failed: {e}")))?;
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google contact preflight failed: {body}"
            )));
        }
        response
            .json()
            .await
            .map_err(|e| Error::Other(format!("Google contact preflight parse error: {e}")))
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
        if !resp.status().is_success()
            && resp.status().as_u16() != 404
            && resp.status().as_u16() != 410
        {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Google delete contact failed: {}",
                body
            )));
        }
        Ok(())
    }

    /// Return all changed identities and a final replacement sync token.
    pub async fn list_contact_changes(
        &self,
        sync_token: Option<&str>,
    ) -> Result<GoogleContactsSync> {
        if sync_token.is_some_and(|token| token.trim().is_empty()) {
            return Err(Error::Other(
                "Google People sync token must not be blank".into(),
            ));
        }
        let mut changes = Vec::new();
        let mut seen_resource_names = std::collections::HashSet::new();
        let mut seen_page_tokens = std::collections::HashSet::new();
        let mut next_page_token: Option<String> = None;

        for _ in 0..MAX_PEOPLE_PAGES {
            let mut request = self
                .http
                .get(self.people_url("people/me/connections"))
                .bearer_auth(&self.token)
                .query(&[
                    ("personFields", PEOPLE_CHANGE_FIELDS),
                    ("pageSize", PEOPLE_PAGE_SIZE_PARAMETER),
                    ("requestSyncToken", "true"),
                    ("sources", "READ_SOURCE_TYPE_CONTACT"),
                ]);
            if let Some(sync_token) = sync_token {
                request = request.query(&[("syncToken", sync_token)]);
            }
            if let Some(page_token) = next_page_token.as_deref() {
                request = request.query(&[("pageToken", page_token)]);
            }
            let response = request
                .send()
                .await
                .map_err(|error| Error::Other(format!("Google People API failed: {error}")))?;
            if response.status().as_u16() == 410 && sync_token.is_some() {
                return Ok(GoogleContactsSync::SyncTokenExpired);
            }
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(Error::Other(format!(
                    "Google People API error {status}: {}",
                    body.chars().take(500).collect::<String>()
                )));
            }
            let value: serde_json::Value = response
                .json()
                .await
                .map_err(|error| Error::Other(format!("Google People API parse error: {error}")))?;
            let page = parse_people_page(&value)?;
            if page.changes.len() > PEOPLE_PAGE_SIZE {
                return Err(Error::Other(
                    "Google People API returned more contacts than the requested page size".into(),
                ));
            }
            for change in page.changes {
                if !seen_resource_names.insert(change.resource_name.clone()) {
                    return Err(Error::Other(
                        "Google People API returned a duplicate resourceName".into(),
                    ));
                }
                changes.push(change);
            }
            if changes.len() > MAX_PEOPLE_CONTACTS {
                return Err(Error::Other(
                    "Google People API contact changes exceeded the client limit".into(),
                ));
            }

            let Some(page_token) = page.next_page_token else {
                let next_sync_token = page.next_sync_token.ok_or_else(|| {
                    Error::Other("Google People API final page omitted nextSyncToken".into())
                })?;
                return Ok(GoogleContactsSync::Changes(GoogleContactChanges {
                    changes,
                    next_sync_token,
                }));
            };
            if page.next_sync_token.is_some() {
                return Err(Error::Other(
                    "Google People API returned nextSyncToken before the final page".into(),
                ));
            }
            if !seen_page_tokens.insert(page_token.clone()) {
                return Err(Error::Other(
                    "Google People API repeated a contact page token".into(),
                ));
            }
            next_page_token = Some(page_token);
        }

        Err(Error::Other(
            "Google People API contact pagination exceeded the client limit".into(),
        ))
    }

    /// Fetch complete contact-only Persons, preserving request correlation.
    pub async fn get_contacts_batch(
        &self,
        resource_names: &[String],
    ) -> Result<Vec<GoogleContactLookup>> {
        if resource_names.len() > MAX_PEOPLE_CONTACTS {
            return Err(Error::Other(
                "Google People batch lookup exceeded the client limit".into(),
            ));
        }
        let mut seen = std::collections::HashSet::with_capacity(resource_names.len());
        for resource_name in resource_names {
            validate_people_resource_name(resource_name, "batch request")?;
            if !seen.insert(resource_name.as_str()) {
                return Err(Error::Other(
                    "Google People batch lookup contains a duplicate resourceName".into(),
                ));
            }
        }

        let mut lookups = Vec::with_capacity(resource_names.len());
        for chunk in resource_names.chunks(PEOPLE_BATCH_SIZE) {
            let mut request = self
                .http
                .get(self.people_url("people:batchGet"))
                .bearer_auth(&self.token)
                .query(&[
                    ("personFields", PEOPLE_PERSON_FIELDS),
                    ("sources", "READ_SOURCE_TYPE_CONTACT"),
                ]);
            for resource_name in chunk {
                request = request.query(&[("resourceNames", resource_name)]);
            }
            let response = request.send().await.map_err(|error| {
                Error::Other(format!("Google People batch lookup failed: {error}"))
            })?;
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(Error::Other(format!(
                    "Google People batch lookup error {status}: {}",
                    body.chars().take(500).collect::<String>()
                )));
            }
            let value: serde_json::Value = response.json().await.map_err(|error| {
                Error::Other(format!("Google People batch lookup parse error: {error}"))
            })?;
            lookups.extend(parse_people_batch(&value, chunk)?);
        }
        Ok(lookups)
    }
}

struct PeoplePage {
    changes: Vec<GoogleContactChange>,
    next_page_token: Option<String>,
    next_sync_token: Option<String>,
}

fn parse_people_page(value: &serde_json::Value) -> Result<PeoplePage> {
    let page = value
        .as_object()
        .ok_or_else(|| Error::Other("Google People API response must be an object".into()))?;
    let next_page_token = optional_people_token(page, "nextPageToken")?;
    let next_sync_token = optional_people_token(page, "nextSyncToken")?;
    let connections: &[serde_json::Value] = match page.get("connections") {
        Some(serde_json::Value::Array(connections)) => connections.as_slice(),
        None | Some(serde_json::Value::Null) => &[],
        Some(_) => {
            return Err(Error::Other(
                "Google People API connections must be an array".into(),
            ));
        }
    };
    let changes = connections
        .iter()
        .map(parse_google_contact_change)
        .collect::<Result<Vec<_>>>()?;
    Ok(PeoplePage {
        changes,
        next_page_token,
        next_sync_token,
    })
}

fn parse_google_contact_change(value: &serde_json::Value) -> Result<GoogleContactChange> {
    let person = value
        .as_object()
        .ok_or_else(|| Error::Other("Google People contact change must be an object".into()))?;
    let resource_name = require_people_resource_name(person.get("resourceName"), "change")?;
    let metadata = optional_people_object(person, "metadata", "contact change")?;
    let deleted = metadata
        .map(|metadata| optional_people_bool(metadata, "deleted", "Person metadata"))
        .transpose()?
        .flatten()
        .unwrap_or(false);
    let previous_values = metadata
        .map(|metadata| optional_people_array(metadata, "previousResourceNames"))
        .transpose()?
        .flatten();
    let mut previous_resource_names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for value in previous_values.into_iter().flatten() {
        let previous = value.as_str().ok_or_else(|| {
            Error::Other("Google People previousResourceNames entry must be a string".into())
        })?;
        validate_people_resource_name(previous, "previous resource name")?;
        if previous == resource_name || !seen.insert(previous) {
            return Err(Error::Other(
                "Google People contact change contains duplicate or current previousResourceName"
                    .into(),
            ));
        }
        previous_resource_names.push(previous.into());
    }
    Ok(GoogleContactChange {
        resource_name,
        previous_resource_names,
        deleted,
    })
}

fn parse_people_batch(
    value: &serde_json::Value,
    requested: &[String],
) -> Result<Vec<GoogleContactLookup>> {
    let batch = value
        .as_object()
        .ok_or_else(|| Error::Other("Google People batch response must be an object".into()))?;
    let responses: &[serde_json::Value] = match batch.get("responses") {
        Some(serde_json::Value::Array(responses)) => responses,
        None | Some(serde_json::Value::Null) => &[],
        Some(_) => {
            return Err(Error::Other(
                "Google People batch responses must be an array".into(),
            ));
        }
    };
    let expected: std::collections::HashSet<&str> = requested.iter().map(String::as_str).collect();
    let mut seen = std::collections::HashSet::with_capacity(responses.len());
    let mut lookups = Vec::with_capacity(responses.len());
    for value in responses {
        let response = value.as_object().ok_or_else(|| {
            Error::Other("Google People batch response entry must be an object".into())
        })?;
        let requested_resource_name =
            required_people_string(response, "requestedResourceName", "batch response")?;
        validate_people_resource_name(requested_resource_name, "batch response")?;
        if !expected.contains(requested_resource_name) || !seen.insert(requested_resource_name) {
            return Err(Error::Other(
                "Google People batch response contains an unexpected or duplicate correlation ID"
                    .into(),
            ));
        }
        let status = optional_people_object(response, "status", "batch response")?;
        let code = status
            .map(parse_people_status_code)
            .transpose()?
            .unwrap_or(0);
        let person = match response.get("person") {
            Some(serde_json::Value::Object(person)) => Some(person),
            None | Some(serde_json::Value::Null) => None,
            Some(_) => {
                return Err(Error::Other(
                    "Google People batch person must be an object".into(),
                ));
            }
        };
        let contact = match (code, person) {
            (0, Some(person)) => Some(parse_google_contact(person)?),
            (5, None) => None,
            (0, None) => {
                return Err(Error::Other(
                    "Google People successful batch response omitted person".into(),
                ));
            }
            (5, Some(_)) => {
                return Err(Error::Other(
                    "Google People not-found batch response included person".into(),
                ));
            }
            (other, _) => {
                let message = status
                    .and_then(|status| status.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown error");
                return Err(Error::Other(format!(
                    "Google People batch item failed with status {other}: {message}"
                )));
            }
        };
        lookups.push(GoogleContactLookup {
            requested_resource_name: requested_resource_name.into(),
            contact,
        });
    }
    if seen.len() != expected.len() {
        return Err(Error::Other(
            "Google People batch response omitted a requested correlation ID".into(),
        ));
    }
    Ok(lookups)
}

fn parse_people_status_code(status: &serde_json::Map<String, serde_json::Value>) -> Result<i64> {
    match status.get("code") {
        None | Some(serde_json::Value::Null) => Ok(0),
        Some(serde_json::Value::Number(code)) => code.as_i64().ok_or_else(|| {
            Error::Other("Google People batch status code must be an integer".into())
        }),
        Some(serde_json::Value::String(code)) => code
            .parse()
            .map_err(|_| Error::Other("Google People batch status code must be an integer".into())),
        Some(_) => Err(Error::Other(
            "Google People batch status code must be an integer".into(),
        )),
    }
}

fn parse_google_contact(
    person: &serde_json::Map<String, serde_json::Value>,
) -> Result<GoogleContact> {
    let resource_name = require_people_resource_name(person.get("resourceName"), "contact")?;
    let contact_source_etag = require_contact_source(person, "contact")?.etag;

    let names = optional_people_array(person, "names")?;
    let display_name = primary_people_entry(names, "names")?
        .map(|name| optional_people_string(name, "displayName", "name"))
        .transpose()?
        .flatten()
        .map(str::to_string);

    let mut emails = Vec::new();
    for value in optional_people_array(person, "emailAddresses")?
        .into_iter()
        .flatten()
    {
        let email = people_entry(value, "emailAddresses")?;
        let address = optional_people_string(email, "value", "email address")?.unwrap_or("");
        let label = optional_people_string(email, "type", "email address")?.unwrap_or("");
        if !address.is_empty() {
            emails.push((
                people_entry_primary_rank(email, "email address")?,
                address.to_string(),
                label.to_string(),
            ));
        }
    }
    emails.sort();
    let emails = emails
        .into_iter()
        .map(|(_, email, label)| serde_json::json!({"email": email, "label": label}))
        .collect::<Vec<_>>();

    let mut phones = Vec::new();
    for value in optional_people_array(person, "phoneNumbers")?
        .into_iter()
        .flatten()
    {
        let phone = people_entry(value, "phoneNumbers")?;
        let number = optional_people_string(phone, "value", "phone number")?.unwrap_or("");
        let label = optional_people_string(phone, "type", "phone number")?.unwrap_or("");
        if !number.is_empty() {
            phones.push((
                people_entry_primary_rank(phone, "phone number")?,
                number.to_string(),
                label.to_string(),
            ));
        }
    }
    phones.sort();
    let phones = phones
        .into_iter()
        .map(|(_, number, label)| serde_json::json!({"number": number, "label": label}))
        .collect::<Vec<_>>();

    let organizations = optional_people_array(person, "organizations")?;
    let primary_organization = primary_people_entry(organizations, "organizations")?;
    let organization = primary_organization
        .map(|entry| optional_people_string(entry, "name", "organization"))
        .transpose()?
        .flatten()
        .map(str::to_string);
    let title = primary_organization
        .map(|entry| optional_people_string(entry, "title", "organization"))
        .transpose()?
        .flatten()
        .map(str::to_string);

    Ok(GoogleContact {
        resource_name,
        contact_source_etag,
        display_name: display_name.unwrap_or_else(|| "(No name)".into()),
        emails_json: serde_json::to_string(&emails)
            .map_err(|error| Error::Other(format!("Google email conversion failed: {error}")))?,
        phones_json: serde_json::to_string(&phones)
            .map_err(|error| Error::Other(format!("Google phone conversion failed: {error}")))?,
        organization,
        title,
    })
}

fn merge_people_update(
    current: &serde_json::Map<String, serde_json::Value>,
    desired: &serde_json::Map<String, serde_json::Value>,
) -> Result<(
    serde_json::Map<String, serde_json::Value>,
    Vec<&'static str>,
)> {
    let current_projection = parse_google_contact(current)?;
    let mut body = serde_json::Map::new();
    let mut changed_fields = Vec::new();

    let desired_names = optional_people_array(desired, "names")?
        .ok_or_else(|| Error::Other("Google People local update payload omitted names".into()))?;
    let desired_name = desired_display_name(desired_names)?;
    if desired_name != current_projection.display_name {
        body.insert(
            "names".into(),
            serde_json::Value::Array(desired_names.clone()),
        );
        changed_fields.push("names");
    }

    for (property, value_property) in [("emailAddresses", "value"), ("phoneNumbers", "value")] {
        if people_value_keys(current, property, value_property)?
            != people_value_keys(desired, property, value_property)?
        {
            body.insert(
                property.into(),
                serde_json::Value::Array(merge_people_value_entries(
                    current,
                    desired,
                    property,
                    value_property,
                )?),
            );
            changed_fields.push(property);
        }
    }

    let desired_organization = desired_primary_organization(desired)?;
    if desired_organization
        != (
            current_projection.organization.clone(),
            current_projection.title.clone(),
        )
    {
        body.insert(
            "organizations".into(),
            serde_json::Value::Array(merge_people_organizations(current, desired_organization)?),
        );
        changed_fields.push("organizations");
    }

    Ok((body, changed_fields))
}

fn desired_display_name(names: &[serde_json::Value]) -> Result<String> {
    let name = primary_people_entry_index(names, "names")?
        .map(|index| people_entry(&names[index], "names"))
        .transpose()?
        .ok_or_else(|| Error::Other("Google People local update has no name".into()))?;
    for property in ["unstructuredName", "givenName", "displayName"] {
        if let Some(value) = optional_people_string(name, property, "local name")? {
            return Ok(value.into());
        }
    }
    Err(Error::Other(
        "Google People local update name has no writable value".into(),
    ))
}

fn people_value_keys(
    person: &serde_json::Map<String, serde_json::Value>,
    property: &str,
    value_property: &str,
) -> Result<Vec<(String, String)>> {
    let mut keys = Vec::new();
    for value in optional_people_array(person, property)?
        .into_iter()
        .flatten()
    {
        let entry = people_entry(value, property)?;
        let item = optional_people_string(entry, value_property, property)?.unwrap_or("");
        if item.is_empty() {
            continue;
        }
        let item_type = optional_people_string(entry, "type", property)?.unwrap_or("");
        keys.push((item.into(), item_type.into()));
    }
    keys.sort();
    Ok(keys)
}

fn merge_people_value_entries(
    current: &serde_json::Map<String, serde_json::Value>,
    desired: &serde_json::Map<String, serde_json::Value>,
    property: &str,
    value_property: &str,
) -> Result<Vec<serde_json::Value>> {
    let current_values = optional_people_array(current, property)?
        .cloned()
        .unwrap_or_default();
    let desired_values = optional_people_array(desired, property)?
        .cloned()
        .unwrap_or_default();
    let current_entries = current_values
        .iter()
        .map(|value| people_entry(value, property))
        .collect::<Result<Vec<_>>>()?;
    let mut used = vec![false; current_entries.len()];
    let mut merged = Vec::with_capacity(desired_values.len());

    for desired_value in desired_values {
        let desired_entry = people_entry(&desired_value, property)?;
        let item = required_people_string(desired_entry, value_property, property)?;
        if item.trim().is_empty() {
            return Err(Error::Other(format!(
                "Google People local {property} entry has a blank {value_property}"
            )));
        }
        let item_type = optional_people_string(desired_entry, "type", property)?.unwrap_or("");
        let exact_match = current_entries
            .iter()
            .enumerate()
            .position(|(index, entry)| {
                !used[index]
                    && entry
                        .get(value_property)
                        .and_then(serde_json::Value::as_str)
                        == Some(item)
                    && entry
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        == item_type
            });
        let value_match = current_entries
            .iter()
            .enumerate()
            .position(|(index, entry)| {
                !used[index]
                    && entry
                        .get(value_property)
                        .and_then(serde_json::Value::as_str)
                        == Some(item)
            });
        let mut entry = if let Some(index) = exact_match.or(value_match) {
            used[index] = true;
            current_entries[index].clone()
        } else {
            serde_json::Map::new()
        };
        entry.insert(value_property.into(), item.into());
        if item_type.is_empty() {
            entry.remove("type");
        } else {
            entry.insert("type".into(), item_type.into());
        }
        entry.remove("formattedType");
        merged.push(serde_json::Value::Object(entry));
    }
    ensure_source_primary(&mut merged, property)?;
    Ok(merged)
}

fn desired_primary_organization(
    desired: &serde_json::Map<String, serde_json::Value>,
) -> Result<(Option<String>, Option<String>)> {
    let organizations = optional_people_array(desired, "organizations")?;
    let organization = primary_people_entry(organizations, "organizations")?;
    Ok((
        organization
            .map(|entry| optional_people_string(entry, "name", "organization"))
            .transpose()?
            .flatten()
            .map(str::to_string),
        organization
            .map(|entry| optional_people_string(entry, "title", "organization"))
            .transpose()?
            .flatten()
            .map(str::to_string),
    ))
}

fn merge_people_organizations(
    current: &serde_json::Map<String, serde_json::Value>,
    desired: (Option<String>, Option<String>),
) -> Result<Vec<serde_json::Value>> {
    let mut organizations = optional_people_array(current, "organizations")?
        .cloned()
        .unwrap_or_default();
    let primary_index = primary_people_entry_index(&organizations, "organizations")?;
    match primary_index {
        Some(index) => {
            let organization = organizations[index].as_object_mut().ok_or_else(|| {
                Error::Other("Google People organization must be an object".into())
            })?;
            set_optional_people_string(organization, "name", desired.0.as_deref());
            set_optional_people_string(organization, "title", desired.1.as_deref());
            let has_writable_value = organization
                .keys()
                .any(|property| property != "metadata" && property != "formattedType");
            if !has_writable_value {
                organizations.remove(index);
            }
        }
        None if desired.0.is_some() || desired.1.is_some() => {
            let mut organization = serde_json::Map::new();
            set_optional_people_string(&mut organization, "name", desired.0.as_deref());
            set_optional_people_string(&mut organization, "title", desired.1.as_deref());
            organizations.push(serde_json::Value::Object(organization));
        }
        None => {}
    }
    ensure_source_primary(&mut organizations, "organizations")?;
    Ok(organizations)
}

fn set_optional_people_string(
    entry: &mut serde_json::Map<String, serde_json::Value>,
    property: &str,
    value: Option<&str>,
) {
    match value {
        Some(value) => {
            entry.insert(property.into(), value.into());
        }
        None => {
            entry.remove(property);
        }
    }
}

fn ensure_source_primary(values: &mut [serde_json::Value], property: &str) -> Result<()> {
    if values.is_empty()
        || values.iter().try_fold(false, |found, value| {
            Ok::<_, Error>(
                found || people_entry_primary_rank(people_entry(value, property)?, property)? == 0,
            )
        })?
    {
        return Ok(());
    }
    let first = values[0]
        .as_object_mut()
        .ok_or_else(|| Error::Other(format!("Google People {property} entry must be an object")))?;
    let metadata = first
        .entry("metadata")
        .or_insert_with(|| serde_json::json!({}));
    if metadata.is_null() {
        *metadata = serde_json::json!({});
    }
    let metadata = metadata.as_object_mut().ok_or_else(|| {
        Error::Other(format!(
            "Google People {property} entry metadata must be an object"
        ))
    })?;
    metadata.insert("sourcePrimary".into(), true.into());
    Ok(())
}

fn primary_people_entry<'a>(
    values: Option<&'a Vec<serde_json::Value>>,
    property: &str,
) -> Result<Option<&'a serde_json::Map<String, serde_json::Value>>> {
    let Some(values) = values else {
        return Ok(None);
    };
    let Some(index) = primary_people_entry_index(values, property)? else {
        return Ok(None);
    };
    people_entry(&values[index], property).map(Some)
}

fn primary_people_entry_index(
    values: &[serde_json::Value],
    property: &str,
) -> Result<Option<usize>> {
    let entries = values
        .iter()
        .map(|value| people_entry(value, property))
        .collect::<Result<Vec<_>>>()?;
    if entries.len() <= 1 {
        return Ok((!entries.is_empty()).then_some(0));
    }
    for metadata_key in ["primary", "sourcePrimary"] {
        let mut selected = None;
        for (index, entry) in entries.iter().enumerate() {
            let metadata = optional_people_object(entry, "metadata", property)?;
            let is_selected = metadata
                .map(|metadata| optional_people_bool(metadata, metadata_key, "field metadata"))
                .transpose()?
                .flatten()
                .unwrap_or(false);
            if is_selected && selected.replace(index).is_some() {
                return Err(Error::Other(format!(
                    "Google People contact {property} contains multiple {metadata_key} entries"
                )));
            }
        }
        if selected.is_some() {
            return Ok(selected);
        }
    }
    Err(Error::Other(format!(
        "Google People contact {property} has multiple entries without a primary"
    )))
}

fn people_entry_primary_rank(
    entry: &serde_json::Map<String, serde_json::Value>,
    context: &str,
) -> Result<u8> {
    let Some(metadata) = optional_people_object(entry, "metadata", context)? else {
        return Ok(1);
    };
    for property in ["primary", "sourcePrimary"] {
        if optional_people_bool(metadata, property, "field metadata")?.unwrap_or(false) {
            return Ok(0);
        }
    }
    Ok(1)
}

fn parse_pushed_contact(value: &serde_json::Value, context: &str) -> Result<GooglePushedContact> {
    let person = value
        .as_object()
        .ok_or_else(|| Error::Other(format!("Google People {context} must be an object")))?;
    let resource_name = require_people_resource_name(person.get("resourceName"), context)?;
    let contact_source_etag = require_contact_source(person, context)?.etag;
    Ok(GooglePushedContact {
        resource_name,
        contact_source_etag,
    })
}

struct ContactSourceMetadata {
    value: serde_json::Value,
    etag: String,
}

fn require_contact_source(
    person: &serde_json::Map<String, serde_json::Value>,
    context: &str,
) -> Result<ContactSourceMetadata> {
    let metadata = optional_people_object(person, "metadata", context)?
        .ok_or_else(|| Error::Other(format!("Google People {context} omitted Person metadata")))?;
    let sources = optional_people_array(metadata, "sources")?
        .ok_or_else(|| Error::Other(format!("Google People {context} omitted metadata sources")))?;
    let mut contact_source = None;
    for source in sources {
        let source = people_entry(source, "metadata sources")?;
        if optional_people_string(source, "type", "metadata source")? != Some("CONTACT") {
            continue;
        }
        let etag = optional_people_string(source, "etag", "CONTACT source")?
            .filter(|etag| !etag.trim().is_empty())
            .ok_or_else(|| {
                Error::Other(format!(
                    "Google People {context} CONTACT source omitted its etag"
                ))
            })?;
        if contact_source
            .replace(ContactSourceMetadata {
                value: serde_json::Value::Object(source.clone()),
                etag: etag.into(),
            })
            .is_some()
        {
            return Err(Error::Other(format!(
                "Google People {context} contains multiple CONTACT sources"
            )));
        }
    }
    contact_source
        .ok_or_else(|| Error::Other(format!("Google People {context} omitted a CONTACT source")))
}

fn require_people_resource_name(
    value: Option<&serde_json::Value>,
    context: &str,
) -> Result<String> {
    let resource_name = value.and_then(serde_json::Value::as_str).ok_or_else(|| {
        Error::Other(format!(
            "Google People {context} resourceName must have the people/<id> form"
        ))
    })?;
    validate_people_resource_name(resource_name, context)?;
    Ok(resource_name.to_string())
}

fn validate_people_resource_name(resource_name: &str, context: &str) -> Result<()> {
    if resource_name
        .strip_prefix("people/")
        .is_some_and(|suffix| !suffix.trim().is_empty() && !suffix.contains('/'))
    {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "Google People {context} resourceName must have the people/<id> form"
        )))
    }
}

fn optional_people_array<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    property: &str,
) -> Result<Option<&'a Vec<serde_json::Value>>> {
    match object.get(property) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Array(values)) => Ok(Some(values)),
        Some(_) => Err(Error::Other(format!(
            "Google People contact {property} must be an array"
        ))),
    }
}

fn optional_people_object<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    property: &str,
    context: &str,
) -> Result<Option<&'a serde_json::Map<String, serde_json::Value>>> {
    match object.get(property) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Object(value)) => Ok(Some(value)),
        Some(_) => Err(Error::Other(format!(
            "Google People {context} {property} must be an object"
        ))),
    }
}

fn optional_people_bool(
    object: &serde_json::Map<String, serde_json::Value>,
    property: &str,
    context: &str,
) -> Result<Option<bool>> {
    match object.get(property) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(Error::Other(format!(
            "Google People {context} {property} must be a boolean"
        ))),
    }
}

fn optional_people_token(
    object: &serde_json::Map<String, serde_json::Value>,
    property: &str,
) -> Result<Option<String>> {
    match object.get(property) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) if value.trim().is_empty() => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(Error::Other(format!(
            "Google People API {property} must be a string"
        ))),
    }
}

fn people_entry<'a>(
    value: &'a serde_json::Value,
    property: &str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    value.as_object().ok_or_else(|| {
        Error::Other(format!(
            "Google People contact {property} entry must be an object"
        ))
    })
}

fn required_people_string<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    property: &str,
    context: &str,
) -> Result<&'a str> {
    object
        .get(property)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            Error::Other(format!(
                "Google People {context} {property} must be a string"
            ))
        })
}

fn optional_people_string<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    property: &str,
    context: &str,
) -> Result<Option<&'a str>> {
    match object.get(property) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(Error::Other(format!(
            "Google People {context} {property} must be a string"
        ))),
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
mod people_tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    fn person(id: &str) -> serde_json::Value {
        serde_json::json!({
            "resourceName": format!("people/{id}"),
            "etag": format!("person-etag-{id}"),
            "metadata": {"sources": [{
                "type": "CONTACT",
                "id": format!("source-{id}"),
                "etag": format!("source-etag-{id}"),
            }]},
            "names": [{
                "displayName": format!("Name {id}"),
                "metadata": {"primary": true},
            }],
            "emailAddresses": [
                {"value": format!("{id}@example.test"), "type": "work"}
            ],
            "phoneNumbers": [{"value": format!("+46{id}"), "type": "mobile"}],
            "organizations": [
                {"name": "Secondary", "title": "Other"},
                {
                    "name": "Example",
                    "title": "Engineer",
                    "metadata": {"primary": true},
                }
            ],
        })
    }

    fn client(root: &str) -> GoogleClient {
        let mut headers = HeaderMap::new();
        headers.insert("x-injected-client", HeaderValue::from_static("people-test"));
        let http = reqwest::Client::builder()
            .no_proxy()
            .default_headers(headers)
            .build()
            .unwrap();
        GoogleClient::with_client(
            http,
            "people-token",
            GoogleEndpoints {
                calendar_api_root: "http://127.0.0.1:1/unused-calendar".into(),
                people_api_root: root.into(),
            },
        )
    }

    async fn serve_responses(
        responses: Vec<(u16, String)>,
    ) -> (String, oneshot::Receiver<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}/people-api", listener.local_addr().unwrap());
        let (requests_tx, requests_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end = loop {
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0, "request ended before its headers");
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(index) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                        break index + 4;
                    }
                };
                let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + content_length {
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0, "request ended before its body");
                    bytes.extend_from_slice(&chunk[..count]);
                }
                requests.push(String::from_utf8(bytes).unwrap());

                let reason = if status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests_tx.send(requests).unwrap();
        });
        (root, requests_rx)
    }

    fn query(request: &str) -> std::collections::HashMap<String, String> {
        let target = request.lines().next().unwrap().split(' ').nth(1).unwrap();
        reqwest::Url::parse(&format!("http://localhost{target}"))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    fn request_body(request: &str) -> serde_json::Value {
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    #[tokio::test]
    async fn contact_changes_follow_pages_with_stable_contact_only_queries() {
        let (root, requests) = serve_responses(vec![
            (
                200,
                serde_json::json!({
                    "connections": [person("one")],
                    "nextPageToken": "opaque +/= token",
                })
                .to_string(),
            ),
            (
                200,
                serde_json::json!({
                    "connections": [person("two")],
                    "nextSyncToken": "next-sync-token",
                })
                .to_string(),
            ),
        ])
        .await;

        let GoogleContactsSync::Changes(result) =
            client(&root).list_contact_changes(None).await.unwrap()
        else {
            panic!("full sync unexpectedly expired");
        };

        assert_eq!(
            result
                .changes
                .iter()
                .map(|change| change.resource_name.as_str())
                .collect::<Vec<_>>(),
            ["people/one", "people/two"]
        );
        assert_eq!(result.next_sync_token, "next-sync-token");
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            let headers = request.to_ascii_lowercase();
            assert!(headers.contains("authorization: bearer people-token\r\n"));
            assert!(headers.contains("x-injected-client: people-test\r\n"));
            assert_eq!(query(request)["pageSize"], "1000");
            assert_eq!(query(request)["personFields"], PEOPLE_CHANGE_FIELDS);
            assert_eq!(query(request)["sources"], "READ_SOURCE_TYPE_CONTACT");
            assert_eq!(query(request)["requestSyncToken"], "true");
            assert!(!query(request).contains_key("syncToken"));
        }
        assert!(!query(&requests[0]).contains_key("pageToken"));
        assert_eq!(query(&requests[1])["pageToken"], "opaque +/= token");
    }

    #[tokio::test]
    async fn contacts_reject_repeated_page_tokens_before_a_third_request() {
        let (root, requests) = serve_responses(vec![
            (
                200,
                serde_json::json!({
                    "connections": [person("one")],
                    "nextPageToken": "same-token",
                })
                .to_string(),
            ),
            (
                200,
                serde_json::json!({
                    "connections": [person("two")],
                    "nextPageToken": "same-token",
                })
                .to_string(),
            ),
        ])
        .await;

        let error = client(&root)
            .list_contact_changes(Some("stored-token"))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("repeated a contact page token"));
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert_eq!(query(&request)["syncToken"], "stored-token");
        }
    }

    #[tokio::test]
    async fn incremental_changes_preserve_tombstones_and_previous_names() {
        let body = serde_json::json!({
            "connections": [{
                "resourceName": "people/current",
                "metadata": {
                    "deleted": true,
                    "previousResourceNames": ["people/old"],
                },
            }],
            "nextPageToken": null,
            "nextSyncToken": "replacement-token",
        });
        let (root, requests) = serve_responses(vec![(200, body.to_string())]).await;

        let GoogleContactsSync::Changes(result) = client(&root)
            .list_contact_changes(Some("stored-token"))
            .await
            .unwrap()
        else {
            panic!("token unexpectedly expired");
        };

        assert_eq!(result.next_sync_token, "replacement-token");
        assert_eq!(result.changes.len(), 1);
        assert!(result.changes[0].deleted);
        assert_eq!(result.changes[0].previous_resource_names, ["people/old"]);
        let requests = requests.await.unwrap();
        assert_eq!(query(&requests[0])["syncToken"], "stored-token");
    }

    #[tokio::test]
    async fn expired_incremental_token_is_distinct_from_other_http_errors() {
        let (root, requests) = serve_responses(vec![(410, "{}".into())]).await;
        assert_eq!(
            client(&root)
                .list_contact_changes(Some("expired"))
                .await
                .unwrap(),
            GoogleContactsSync::SyncTokenExpired
        );
        assert_eq!(requests.await.unwrap().len(), 1);

        let (root, requests) = serve_responses(vec![(410, "{}".into())]).await;
        assert!(client(&root).list_contact_changes(None).await.is_err());
        assert_eq!(requests.await.unwrap().len(), 1);
    }

    #[test]
    fn page_parser_accepts_protojson_defaults_and_sparse_changes() {
        for empty in [
            serde_json::json!({}),
            serde_json::json!({"connections": null}),
            serde_json::json!({
                "connections": [],
                "nextPageToken": null,
                "nextSyncToken": "",
            }),
        ] {
            let page = parse_people_page(&empty).unwrap();
            assert!(page.changes.is_empty());
            assert!(page.next_page_token.is_none());
        }

        let page = parse_people_page(&serde_json::json!({
            "connections": [{
                "resourceName": "people/one",
                "metadata": null,
            }],
        }))
        .unwrap();
        assert_eq!(page.changes[0].resource_name, "people/one");
        assert!(!page.changes[0].deleted);
    }

    #[test]
    fn page_parser_rejects_malformed_envelopes_and_change_metadata() {
        for invalid in [
            serde_json::json!([]),
            serde_json::json!({"connections": {}}),
            serde_json::json!({"connections": [], "nextPageToken": 7}),
            serde_json::json!({"connections": [null]}),
            serde_json::json!({"connections": [{"resourceName": ""}]}),
            serde_json::json!({"connections": [{"resourceName": "other/id"}]}),
            serde_json::json!({
                "connections": [{"resourceName": "people/id", "metadata": []}]
            }),
            serde_json::json!({
                "connections": [{
                    "resourceName": "people/id",
                    "metadata": {"deleted": "true"},
                }]
            }),
            serde_json::json!({
                "connections": [{
                    "resourceName": "people/id",
                    "metadata": {"previousResourceNames": [null]},
                }]
            }),
            serde_json::json!({
                "connections": [{
                    "resourceName": "people/id",
                    "metadata": {"previousResourceNames": ["people/id"]},
                }]
            }),
        ] {
            assert!(parse_people_page(&invalid).is_err(), "accepted {invalid}");
        }
    }

    #[tokio::test]
    async fn create_requests_contact_metadata_and_validates_the_returned_person() {
        let (root, requests) = serve_responses(vec![(200, person("created").to_string())]).await;
        let pushed = client(&root)
            .create_contact(&serde_json::json!({"names": []}))
            .await
            .unwrap();
        assert_eq!(pushed.resource_name, "people/created");
        assert_eq!(pushed.contact_source_etag, "source-etag-created");
        let requests = requests.await.unwrap();
        let request = &requests[0];
        assert_eq!(query(request)["personFields"], PEOPLE_PERSON_FIELDS);
        assert_eq!(query(request)["sources"], "READ_SOURCE_TYPE_CONTACT");

        for body in [
            serde_json::json!({}),
            serde_json::json!({"resourceName": null}),
            serde_json::json!({"resourceName": " "}),
            serde_json::json!({"resourceName": "other/id"}),
            serde_json::json!({"resourceName": "people/id", "metadata": null}),
        ] {
            let (root, requests) = serve_responses(vec![(200, body.to_string())]).await;
            assert!(client(&root)
                .create_contact(&serde_json::json!({"names": []}))
                .await
                .is_err());
            assert_eq!(requests.await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn batch_get_correlates_renames_not_found_and_complete_primary_fields() {
        let body = serde_json::json!({"responses": [
            {
                "requestedResourceName": "people/old",
                "person": person("current"),
                "status": {},
            },
            {
                "requestedResourceName": "people/missing",
                "status": {"code": 5, "message": "not found"},
            }
        ]});
        let (root, requests) = serve_responses(vec![(200, body.to_string())]).await;

        let lookups = client(&root)
            .get_contacts_batch(&["people/old".into(), "people/missing".into()])
            .await
            .unwrap();

        assert_eq!(lookups.len(), 2);
        let contact = lookups[0].contact.as_ref().unwrap();
        assert_eq!(lookups[0].requested_resource_name, "people/old");
        assert_eq!(contact.resource_name, "people/current");
        assert_eq!(contact.display_name, "Name current");
        assert_eq!(contact.organization.as_deref(), Some("Example"));
        assert_eq!(contact.title.as_deref(), Some("Engineer"));
        assert!(lookups[1].contact.is_none());
        let requests = requests.await.unwrap();
        let request = &requests[0];
        let target = request.lines().next().unwrap().split(' ').nth(1).unwrap();
        let resource_names = reqwest::Url::parse(&format!("http://localhost{target}"))
            .unwrap()
            .query_pairs()
            .filter(|(name, _)| name == "resourceNames")
            .map(|(_, value)| value.into_owned())
            .collect::<Vec<_>>();
        assert_eq!(resource_names, ["people/old", "people/missing"]);
        assert_eq!(query(request)["personFields"], PEOPLE_PERSON_FIELDS);
        assert_eq!(query(request)["sources"], "READ_SOURCE_TYPE_CONTACT");
    }

    #[tokio::test]
    async fn batch_get_chunks_at_the_people_api_limit() {
        let names = (0..201)
            .map(|index| format!("people/{index}"))
            .collect::<Vec<_>>();
        let response = |range: std::ops::Range<usize>| {
            serde_json::json!({
                "responses": range.map(|index| serde_json::json!({
                    "requestedResourceName": format!("people/{index}"),
                    "status": {"code": 5},
                })).collect::<Vec<_>>()
            })
            .to_string()
        };
        let (root, requests) =
            serve_responses(vec![(200, response(0..200)), (200, response(200..201))]).await;

        let lookups = client(&root).get_contacts_batch(&names).await.unwrap();

        assert_eq!(lookups.len(), 201);
        assert!(lookups.iter().all(|lookup| lookup.contact.is_none()));
        let requests = requests.await.unwrap();
        let resource_name_count = |request: &str| {
            let target = request.lines().next().unwrap().split(' ').nth(1).unwrap();
            reqwest::Url::parse(&format!("http://localhost{target}"))
                .unwrap()
                .query_pairs()
                .filter(|(name, _)| name == "resourceNames")
                .count()
        };
        assert_eq!(resource_name_count(&requests[0]), 200);
        assert_eq!(resource_name_count(&requests[1]), 1);
    }

    #[test]
    fn batch_parser_rejects_missing_correlations_and_non_not_found_errors() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({"responses": [{
                "requestedResourceName": "people/other",
                "person": person("other"),
            }]}),
            serde_json::json!({"responses": [{
                "requestedResourceName": "people/id",
                "status": {"code": 7, "message": "denied"},
            }]}),
            serde_json::json!({"responses": [{
                "requestedResourceName": "people/id",
                "status": {},
            }]}),
        ] {
            assert!(parse_people_batch(&body, &["people/id".into()]).is_err());
        }
    }

    #[tokio::test]
    async fn update_prefetches_source_etag_and_patches_every_owned_field() {
        let mut current = person("current");
        current["metadata"]["sources"][0]["extra"] = serde_json::json!("preserved");
        let (root, requests) = serve_responses(vec![
            (200, current.to_string()),
            (200, person("current").to_string()),
        ])
        .await;
        let payload = contact_to_person_json(
            "Updated",
            r#"[{"email":"updated@example.test","label":"work"}]"#,
            r#"[{"number":"+4670","label":"mobile"}]"#,
            Some("New Org"),
            Some("New Title"),
        )
        .unwrap();

        let pushed = client(&root)
            .update_contact("people/old", Some("source-etag-current"), &payload)
            .await
            .unwrap();

        assert_eq!(pushed.resource_name, "people/current");
        let requests = requests.await.unwrap();
        assert!(requests[0].starts_with("GET /people-api/people/old?"));
        assert!(requests[1].starts_with("PATCH /people-api/people/current:updateContact?"));
        assert_eq!(
            query(&requests[1])["updatePersonFields"],
            "names,emailAddresses,phoneNumbers,organizations"
        );
        assert_eq!(query(&requests[1])["personFields"], PEOPLE_PERSON_FIELDS);
        assert_eq!(query(&requests[1])["sources"], "READ_SOURCE_TYPE_CONTACT");
        let body = request_body(&requests[1]);
        assert_eq!(body["resourceName"], "people/current");
        assert_eq!(body["etag"], "person-etag-current");
        assert_eq!(body["metadata"]["sources"][0]["type"], "CONTACT");
        assert_eq!(
            body["metadata"]["sources"][0]["etag"],
            "source-etag-current"
        );
        assert_eq!(body["metadata"]["sources"][0]["extra"], "preserved");
        assert_eq!(body["organizations"][0]["name"], "Secondary");
        assert_eq!(body["organizations"][1]["name"], "New Org");
    }

    #[tokio::test]
    async fn update_skips_patch_when_only_local_fields_changed() {
        let (root, requests) = serve_responses(vec![(200, person("current").to_string())]).await;
        let payload = contact_to_person_json(
            "Name current",
            r#"[{"email":"current@example.test","label":"work"}]"#,
            r#"[{"number":"+46current","label":"mobile"}]"#,
            Some("Example"),
            Some("Engineer"),
        )
        .unwrap();

        let pushed = client(&root)
            .update_contact("people/current", Some("source-etag-current"), &payload)
            .await
            .unwrap();

        assert_eq!(pushed.resource_name, "people/current");
        assert_eq!(pushed.contact_source_etag, "source-etag-current");
        assert_eq!(requests.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_rejects_stale_source_etag_before_patch() {
        let (root, requests) = serve_responses(vec![(200, person("current").to_string())]).await;
        let payload = contact_to_person_json(
            "Changed",
            r#"[{"email":"current@example.test","label":"work"}]"#,
            r#"[{"number":"+46current","label":"mobile"}]"#,
            Some("Example"),
            Some("Engineer"),
        )
        .unwrap();

        let error = client(&root)
            .update_contact("people/current", Some("stale-etag"), &payload)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("changed remotely"));
        assert_eq!(requests.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn phone_only_update_preserves_rich_unmodeled_person_fields() {
        let mut rich = person("rich");
        rich["names"][0] = serde_json::json!({
            "displayName": "Ada Lovelace",
            "givenName": "Ada",
            "familyName": "Lovelace",
            "metadata": {"primary": true, "sourcePrimary": true},
        });
        rich["emailAddresses"][0]["displayName"] = serde_json::json!("Ada at work");
        rich["organizations"][1]["department"] = serde_json::json!("Research");
        let (root, requests) = serve_responses(vec![
            (200, rich.to_string()),
            (200, person("rich").to_string()),
        ])
        .await;
        let payload = contact_to_person_json(
            "Ada Lovelace",
            r#"[{"email":"rich@example.test","label":"work"}]"#,
            r#"[{"number":"+461234","label":"mobile"}]"#,
            Some("Example"),
            Some("Engineer"),
        )
        .unwrap();

        client(&root)
            .update_contact("people/rich", Some("source-etag-rich"), &payload)
            .await
            .unwrap();

        let requests = requests.await.unwrap();
        assert_eq!(query(&requests[1])["updatePersonFields"], "phoneNumbers");
        let body = request_body(&requests[1]);
        assert!(body.get("names").is_none());
        assert!(body.get("emailAddresses").is_none());
        assert!(body.get("organizations").is_none());
        assert_eq!(body["phoneNumbers"][0]["value"], "+461234");
    }

    #[test]
    fn contact_parser_orders_primary_values_first_and_preserves_missing_types() {
        let mut value = person("ordered");
        value["emailAddresses"] = serde_json::json!([
            {"value": "secondary@example.test", "type": null},
            {
                "value": "primary@example.test",
                "type": "work",
                "metadata": {"primary": true},
            }
        ]);
        let contact = parse_google_contact(value.as_object().unwrap()).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&contact.emails_json).unwrap(),
            serde_json::json!([
                {"email": "primary@example.test", "label": "work"},
                {"email": "secondary@example.test", "label": ""},
            ])
        );
    }

    #[test]
    fn contact_parser_rejects_ambiguous_multiple_contact_sources() {
        let mut value = person("linked");
        let second = serde_json::json!({
            "type": "CONTACT",
            "id": "second",
            "etag": "second-etag",
        });
        value["metadata"]["sources"]
            .as_array_mut()
            .unwrap()
            .push(second);
        assert!(parse_google_contact(value.as_object().unwrap()).is_err());
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

/// People v1 `Person` payload for all fields owned by the Google adapter.
pub fn contact_to_person_json(
    display_name: &str,
    emails_json: &str,
    phones_json: &str,
    organization: Option<&str>,
    title: Option<&str>,
) -> Result<serde_json::Value> {
    let mut person = serde_json::json!({
        "names": [{"unstructuredName": display_name}],
        "emailAddresses": [],
        "phoneNumbers": [],
        "organizations": [],
    });
    let emails = serde_json::from_str::<Vec<serde_json::Value>>(emails_json)
        .map_err(|error| Error::Other(format!("contact email JSON is invalid: {error}")))?;
    let mut google_emails = Vec::with_capacity(emails.len());
    for email in emails {
        let entry = email
            .as_object()
            .ok_or_else(|| Error::Other("contact email entry must be an object".into()))?;
        let address = required_local_contact_string(entry, "email", "email")?;
        let label = optional_local_contact_string(entry, "label", "email")?;
        let mut value = serde_json::json!({"value": address});
        if let Some(label) = label.filter(|label| !label.trim().is_empty()) {
            value["type"] = serde_json::Value::String(label.into());
        }
        google_emails.push(value);
    }
    person["emailAddresses"] = serde_json::json!(google_emails);
    let phones = serde_json::from_str::<Vec<serde_json::Value>>(phones_json)
        .map_err(|error| Error::Other(format!("contact phone JSON is invalid: {error}")))?;
    let mut google_phones = Vec::with_capacity(phones.len());
    for phone in phones {
        let entry = phone
            .as_object()
            .ok_or_else(|| Error::Other("contact phone entry must be an object".into()))?;
        let number = required_local_contact_string(entry, "number", "phone")?;
        let label = optional_local_contact_string(entry, "label", "phone")?;
        let mut value = serde_json::json!({"value": number});
        if let Some(label) = label.filter(|label| !label.trim().is_empty()) {
            value["type"] = serde_json::Value::String(label.into());
        }
        google_phones.push(value);
    }
    person["phoneNumbers"] = serde_json::json!(google_phones);
    if organization.is_some() || title.is_some() {
        let mut value = serde_json::Map::new();
        if let Some(organization) = organization {
            value.insert("name".into(), organization.into());
        }
        if let Some(title) = title {
            value.insert("title".into(), title.into());
        }
        person["organizations"] = serde_json::json!([value]);
    }
    Ok(person)
}

fn required_local_contact_string<'a>(
    entry: &'a serde_json::Map<String, serde_json::Value>,
    property: &str,
    context: &str,
) -> Result<&'a str> {
    entry
        .get(property)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            Error::Other(format!(
                "contact {context} entry requires a nonblank {property}"
            ))
        })
}

fn optional_local_contact_string<'a>(
    entry: &'a serde_json::Map<String, serde_json::Value>,
    property: &str,
    context: &str,
) -> Result<Option<&'a str>> {
    match entry.get(property) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(Error::Other(format!(
            "contact {context} entry {property} must be a string"
        ))),
    }
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
            Some("Analytical Engines"),
            Some("Programmer"),
        )
        .unwrap();
        assert_eq!(v["names"][0]["unstructuredName"], "Ada Lovelace");
        assert_eq!(v["emailAddresses"][0]["value"], "ada@x.org");
        assert_eq!(v["emailAddresses"][0]["type"], "work");
        assert_eq!(v["phoneNumbers"][0]["value"], "+4670");
        assert_eq!(v["phoneNumbers"][0]["type"], "mobile");
        assert_eq!(v["organizations"][0]["name"], "Analytical Engines");
        assert_eq!(v["organizations"][0]["title"], "Programmer");
    }

    #[test]
    fn person_json_rejects_malformed_lists_and_preserves_intentional_empty_lists() {
        for (emails, phones) in [
            ("not json", "[]"),
            (r#"[{"label":"work"}]"#, "[]"),
            (r#"[{"email":" "}]"#, "[]"),
            (r#"[{"email":"x@example.test","label":7}]"#, "[]"),
            ("[]", r#"[{"label":"mobile"}]"#),
        ] {
            assert!(contact_to_person_json("X", emails, phones, None, None).is_err());
        }

        let v = contact_to_person_json("X", "[]", "[]", None, None).unwrap();
        assert_eq!(v["emailAddresses"], serde_json::json!([]));
        assert_eq!(v["phoneNumbers"], serde_json::json!([]));
        assert_eq!(v["organizations"], serde_json::json!([]));
    }
}
