//! Exact-resource operations for complete calendar sets. No write is retried
//! implicitly; insert reconciliation uses the operation identity, never a UID.

use chrono::{DateTime, NaiveDate, SecondsFormat, TimeZone};
use serde_json::{json, Value};

use super::GoogleClient;
use crate::calendar::{
    event_set::event_fields, simple_recurrence, Attendee, CalendarEvent, RecurrenceKind,
};
use crate::error::{Error, Result};

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::Sync(format!("Google calendar event set: {message}"))
}

fn event_path(calendar: &str, id: &str) -> String {
    format!(
        "calendars/{}/events/{}",
        urlencoding::encode(calendar),
        urlencoding::encode(id)
    )
}

async fn json_response(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    if !status.is_success() {
        return Err(invalid(format!(
            "HTTP {status}: {}",
            response.text().await.unwrap_or_default()
        )));
    }
    response
        .json()
        .await
        .map_err(|e| invalid(format!("invalid canonical response: {e}")))
}

impl GoogleClient {
    pub(crate) async fn get_set_event(&self, calendar: &str, id: &str) -> Result<Value> {
        let response = self
            .http
            .get(self.calendar_url(&event_path(calendar, id)))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(invalid)?;
        let value = json_response(response).await?;
        if value["id"].as_str() != Some(id) {
            return Err(invalid("GET returned a different event identity"));
        }
        Ok(value)
    }

    /// With singleEvents=false, events.list returns masters and finite exceptions,
    /// including cancelled exceptions when showDeleted=true. No window is applied.
    pub(crate) async fn list_set_resources(&self, calendar: &str) -> Result<Vec<Value>> {
        self.collect_event_pages(
            calendar,
            &[("singleEvents", "false"), ("showDeleted", "true")],
            false,
            false,
        )
        .await?
        .map(|page| page.items)
        .ok_or_else(|| invalid("unexpected expired listing"))
    }

    /// originalStart is the documented instances filter. It also finds an
    /// exception moved outside its original date; IDs are never synthesized.
    /// Keep date-valued identities as dates, rather than inventing UTC midnight.
    pub(crate) async fn find_set_instance(
        &self,
        calendar: &str,
        master: &str,
        original: &str,
    ) -> Result<Vec<Value>> {
        let mut token = None;
        let mut seen = std::collections::HashSet::new();
        let mut items = Vec::new();
        for _ in 0..super::MAX_EVENT_PAGES {
            let mut request = self
                .http
                .get(self.calendar_url(&format!("{}/instances", event_path(calendar, master))))
                .bearer_auth(&self.token)
                .query(&[
                    ("originalStart", original),
                    ("showDeleted", "true"),
                    ("maxResults", "2500"),
                ]);
            if let Some(ref token) = token {
                request = request.query(&[("pageToken", token)]);
            }
            let page = json_response(request.send().await.map_err(invalid)?).await?;
            items.extend(
                page["items"]
                    .as_array()
                    .ok_or_else(|| invalid("instances omitted items"))?
                    .iter()
                    .cloned(),
            );
            if items.len() > 1 {
                return Err(invalid("originalStart returned multiple instances"));
            }
            token = super::calendar_token(
                page.as_object()
                    .ok_or_else(|| invalid("invalid instance page"))?,
                "nextPageToken",
            )?;
            match &token {
                None => return Ok(items),
                Some(token) if !seen.insert(token.clone()) => {
                    return Err(invalid("repeated instance page token"))
                }
                _ => {}
            }
        }
        Err(invalid("instance page budget exceeded"))
    }

    pub(crate) async fn patch_set_event(
        &self,
        calendar: &str,
        id: &str,
        revision: &str,
        patch: &Value,
    ) -> Result<()> {
        let response = self
            .http
            .patch(self.calendar_url(&event_path(calendar, id)))
            .bearer_auth(&self.token)
            .header(reqwest::header::IF_MATCH, revision)
            .query(&[
                ("sendUpdates", "none"),
                ("supportsAttachments", "true"),
                ("conferenceDataVersion", "1"),
            ])
            .json(patch)
            .send()
            .await
            .map_err(invalid)?;
        if !response.status().is_success() {
            return Err(invalid(format!(
                "conditional PATCH HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }
        Ok(())
    }

    pub(crate) async fn delete_set_event(
        &self,
        calendar: &str,
        id: &str,
        revision: &str,
    ) -> Result<()> {
        let response = self
            .http
            .delete(self.calendar_url(&event_path(calendar, id)))
            .bearer_auth(&self.token)
            .header(reqwest::header::IF_MATCH, revision)
            .query(&[("sendUpdates", "none")])
            .send()
            .await
            .map_err(invalid)?;
        if !response.status().is_success() {
            return Err(invalid(format!(
                "conditional DELETE HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }
        Ok(())
    }

    /// A deterministic insert can be reconciled after a 409, transport failure,
    /// or unreadable success response. A mismatching operation tag is a conflict.
    pub(crate) async fn insert_set_event(
        &self,
        calendar: &str,
        body: &Value,
        operation: &str,
    ) -> Result<Value> {
        let id = body["id"]
            .as_str()
            .ok_or_else(|| invalid("insert requires deterministic ID"))?;
        let response = self
            .http
            .post(self.calendar_url(&format!(
                "calendars/{}/events",
                urlencoding::encode(calendar)
            )))
            .bearer_auth(&self.token)
            .query(&[
                ("sendUpdates", "none"),
                ("supportsAttachments", "true"),
                ("conferenceDataVersion", "1"),
            ])
            .json(body)
            .send()
            .await;
        let value = match response {
            Ok(response) if response.status().is_success() => {
                match response.json::<Value>().await {
                    Ok(value) => value,
                    Err(_) => self.get_set_event(calendar, id).await?,
                }
            }
            Ok(response)
                if response.status().as_u16() == 409 || response.status().is_server_error() =>
            {
                self.get_set_event(calendar, id).await?
            }
            Err(_) => self.get_set_event(calendar, id).await?,
            Ok(response) => {
                return Err(invalid(format!(
                    "insert HTTP {}: {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                )))
            }
        };
        if value["id"].as_str() != Some(id)
            || value["extendedProperties"]["private"]["chithiOperation"].as_str() != Some(operation)
        {
            return Err(invalid(
                "insert identity/operation conflict; reconciliation required",
            ));
        }
        Ok(value)
    }

    pub(crate) async fn calendar_access_role(&self, calendar: &str) -> Result<String> {
        let response = self
            .http
            .get(self.calendar_url(&format!(
                "users/me/calendarList/{}",
                urlencoding::encode(calendar)
            )))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(invalid)?;
        let value = json_response(response).await?;
        value["accessRole"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| invalid("calendar access role missing"))
    }

    pub(crate) async fn move_set_event(
        &self,
        source: &str,
        id: &str,
        revision: &str,
        destination: &str,
    ) -> Result<()> {
        let response = self
            .http
            .post(self.calendar_url(&format!("{}/move", event_path(source, id))))
            .bearer_auth(&self.token)
            .header(reqwest::header::IF_MATCH, revision)
            .query(&[("destination", destination), ("sendUpdates", "none")])
            .send()
            .await
            .map_err(invalid)?;
        if !response.status().is_success() {
            return Err(invalid(format!(
                "move HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }
        Ok(())
    }
}

/// Normalize UI rules, translating date UNTIL to the end of that local day for
/// timed DTSTARTs (RFC 5545 requires a UTC UNTIL with a zoned DTSTART).
pub(crate) fn google_rule(event: &CalendarEvent) -> Result<String> {
    let rule = simple_recurrence::normalize_rule(
        event
            .recurrence_rule
            .as_deref()
            .ok_or_else(|| invalid("missing rule"))?,
        event,
    )?;
    if event.all_day {
        return Ok(rule);
    }
    let zone = event_zone(event)?;
    rule.split(';')
        .map(|part| {
            if let Some(date) = part.strip_prefix("UNTIL=").filter(|v| v.len() == 8) {
                let date = NaiveDate::parse_from_str(date, "%Y%m%d").map_err(invalid)?;
                let end = date
                    .and_hms_opt(23, 59, 59)
                    .ok_or_else(|| invalid("invalid UNTIL"))?;
                let end = zone
                    .from_local_datetime(&end)
                    .latest()
                    .ok_or_else(|| invalid("UNTIL falls in timezone gap"))?;
                Ok(format!(
                    "UNTIL={}",
                    end.with_timezone(&chrono::Utc).format("%Y%m%dT%H%M%SZ")
                ))
            } else {
                Ok(part.to_owned())
            }
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join(";"))
}

fn event_zone(event: &CalendarEvent) -> Result<chrono_tz::Tz> {
    let name = event.timezone.as_deref().unwrap_or("UTC");
    crate::calendar::timezone::windows_to_iana(name)
        .unwrap_or(name)
        .parse()
        .map_err(|_| invalid("unknown IANA timezone"))
}

/// Complete common writable content, also used as the semantic diff projection.
/// Native-only properties are intentionally not reconstructed here.
pub(crate) fn event_set_content(event: &CalendarEvent) -> Result<Value> {
    event_fields(event).validate()?;
    let zone = event_zone(event)?;
    let boundary = |value: &str| -> Result<Value> {
        if event.all_day {
            let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(invalid)?;
            Ok(json!({"date": date.to_string()}))
        } else {
            let instant = DateTime::parse_from_rfc3339(value).map_err(invalid)?;
            Ok(
                json!({"dateTime": instant.with_timezone(&zone).to_rfc3339_opts(SecondsFormat::AutoSi, true), "timeZone": zone.name()}),
            )
        }
    };
    let mut body = json!({"summary": event.title, "description": event.description, "location": event.location,
        "start": boundary(&event.start_time)?, "end": boundary(&event.end_time)?});
    if let Some(attendees) = &event.attendees_json {
        let attendees: Vec<Attendee> = serde_json::from_str(attendees).map_err(invalid)?;
        body["attendees"] = json!(attendees.into_iter().map(|a| {
            let mut value = json!({"email": a.email, "responseStatus": if a.status == "needs-action" { "needsAction" } else { &a.status }});
            if let Some(name) = a.name { value["displayName"] = json!(name); }
            value
        }).collect::<Vec<_>>());
    } else {
        body["attendees"] = json!([]);
    }
    match event.recurrence_kind {
        RecurrenceKind::Series => {
            body["recurrence"] = json!([format!("RRULE:{}", google_rule(event)?)])
        }
        RecurrenceKind::Standalone | RecurrenceKind::Occurrence
            if event.recurrence_rule.as_deref().is_none_or(str::is_empty) => {}
        _ => return Err(invalid("invalid recurrence classification")),
    }
    Ok(body)
}
