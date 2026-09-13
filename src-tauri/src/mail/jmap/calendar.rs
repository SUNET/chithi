//! JMAP calendar domain: `Calendar/*` and `CalendarEvent/*` methods
//! (RFC 8984 JSCalendar).

use crate::calendar::recurrence::{faithful_local_recurrence_rules, valid_local_datetime};
use crate::calendar::RecurrenceKind;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

use super::{JmapConfig, JmapConnection};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JmapCalendar {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JmapCalendarEvent {
    pub id: String,
    pub calendar_id: String,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: String, // ISO 8601
    pub end: String,
    pub all_day: bool,
    pub timezone: Option<String>,
    pub recurrence_rule: Option<String>,
    #[serde(default)]
    pub recurrence_kind: RecurrenceKind,
    /// Only known local creation can prove that RRULE is all recurrence data.
    #[serde(skip)]
    recurrence_rule_is_complete: bool,
    pub uid: Option<String>,
    pub organizer_email: Option<String>,
    pub attendees_json: Option<String>,
}

impl JmapCalendarEvent {
    pub(crate) fn for_local_creation(
        event: &crate::calendar::CalendarEvent,
        remote_calendar_id: &str,
    ) -> Result<Self> {
        let wire = Self {
            id: String::new(),
            calendar_id: remote_calendar_id.to_string(),
            title: event.title.clone(),
            description: event.description.clone(),
            location: event.location.clone(),
            start: event.start_time.clone(),
            end: event.end_time.clone(),
            all_day: event.all_day,
            timezone: event.timezone.clone(),
            recurrence_rule: event.recurrence_rule.clone(),
            recurrence_kind: event.recurrence_kind,
            recurrence_rule_is_complete: event.recurrence_kind == RecurrenceKind::Series
                && event.ical_data.is_none()
                && event.source_message_id.is_none(),
            uid: event.uid.clone(),
            organizer_email: event.organizer_email.clone(),
            attendees_json: event.attendees_json.clone(),
        };
        wire.creation_recurrence_rules()?;
        Ok(wire)
    }

    /// Validate before transport I/O; never drop an unsupported rule or instance id.
    fn creation_recurrence_rules(&self) -> Result<Option<serde_json::Value>> {
        let unsupported = |reason: &str| {
            Error::Other(format!(
            "Cannot create JMAP event '{}': {reason}. Keep this event local and manage its recurrence in the source calendar",
            self.title
        ))
        };
        let rule = self
            .recurrence_rule
            .as_deref()
            .filter(|rule| !rule.is_empty());
        match self.recurrence_kind {
            RecurrenceKind::Unknown => Err(unsupported("recurrence classification is unknown")),
            RecurrenceKind::Occurrence => {
                Err(unsupported("detached occurrence creation is unsupported"))
            }
            RecurrenceKind::Standalone if rule.is_none() => Ok(None),
            RecurrenceKind::Standalone => Err(unsupported(
                "standalone classification conflicts with a recurrence rule",
            )),
            RecurrenceKind::Series if !self.recurrence_rule_is_complete => Err(unsupported(
                "source recurrence data cannot be represented by the available RRULE alone",
            )),
            RecurrenceKind::Series => {
                let rule =
                    rule.ok_or_else(|| unsupported("a complete recurrence rule is missing"))?;
                let rules = faithful_local_recurrence_rules(rule, self.timezone.as_deref())
                    .ok_or_else(|| {
                        unsupported("the recurrence rule cannot be represented faithfully")
                    })?;
                Ok(Some(rules))
            }
        }
    }
}

/// Fetch the provider's complete native JSCalendar Event representation.
/// JMAP Calendars §5.7 defines omitted `properties` to return stored properties;
/// listing names from different schema versions risks RFC 8620 §5.1 rejection.
fn calendar_events_request(account_id: &str) -> serde_json::Value {
    serde_json::json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
        "methodCalls": [
            ["CalendarEvent/query", {
                "accountId": account_id,
                "limit": 1000
            }, "q1"],
            ["CalendarEvent/get", {
                "#ids": { "resultOf": "q1", "name": "CalendarEvent/query", "path": "/ids" },
                "accountId": account_id
            }, "g1"]
        ]
    })
}

/// Classify the native provider object before lossy DTO/RRULE conversion.
/// Optional absent/null properties and empty recurrence sets mean no recurrence.
/// RFC 8984 §4.3 and JSCalendar-bis §4.3 define the two rule representations;
/// JMAP Calendars §5 defines detached instances and synthetic `baseEventId`.
fn classify_recurrence(event: &serde_json::Value) -> RecurrenceKind {
    use serde_json::Value;

    if event["@type"].as_str() != Some("Event") {
        return RecurrenceKind::Unknown;
    }
    let present = |name: &str| event.get(name).filter(|value| !value.is_null());
    let recurrence_id = match present("recurrenceId") {
        Some(Value::String(value)) if valid_local_datetime(value) => true,
        Some(_) => return RecurrenceKind::Unknown,
        None => false,
    };
    if let Some(timezone) = present("recurrenceIdTimeZone") {
        if !recurrence_id || !timezone.as_str().is_some_and(|s| !s.is_empty()) {
            return RecurrenceKind::Unknown;
        }
    }
    if let Some(base_id) = present("baseEventId") {
        if !recurrence_id || !base_id.as_str().is_some_and(|s| !s.is_empty()) {
            return RecurrenceKind::Unknown;
        }
    }
    if let Some(excluded) = present("excluded") {
        if !excluded.is_boolean() || (excluded == &Value::Bool(true) && !recurrence_id) {
            return RecurrenceKind::Unknown;
        }
    }

    let mut recurring = false;
    for name in ["recurrenceRules", "excludedRecurrenceRules"] {
        if let Some(value) = present(name) {
            let Some(rules) = value.as_array() else {
                return RecurrenceKind::Unknown;
            };
            if !rules.iter().all(valid_recurrence_rule) {
                return RecurrenceKind::Unknown;
            }
            recurring |= !rules.is_empty();
        }
    }
    if let Some(rule) = present("recurrenceRule") {
        if !valid_recurrence_rule(rule) {
            return RecurrenceKind::Unknown;
        }
        recurring = true;
    }
    if let Some(value) = present("recurrenceOverrides") {
        let Some(overrides) = value.as_object() else {
            return RecurrenceKind::Unknown;
        };
        for (id, patch) in overrides {
            let Some(patch) = patch.as_object() else {
                return RecurrenceKind::Unknown;
            };
            // An empty patch adds a date; it is not an empty recurrence set.
            if !valid_local_datetime(id)
                || patch
                    .get("@type")
                    .is_some_and(|value| value.as_str() != Some("Event"))
                || patch
                    .get("excluded")
                    .is_some_and(|value| !value.is_boolean())
            {
                return RecurrenceKind::Unknown;
            }
        }
        recurring |= !overrides.is_empty();
    }
    if recurrence_id {
        return if recurring {
            RecurrenceKind::Unknown
        } else {
            RecurrenceKind::Occurrence
        };
    }

    // "This and future" splits link the series with first/next relations.
    if let Some(value) = present("relatedTo") {
        let Some(relations) = value.as_object() else {
            return RecurrenceKind::Unknown;
        };
        for relation in relations.values() {
            if !relation.is_object()
                || relation
                    .get("@type")
                    .is_some_and(|value| value.as_str() != Some("Relation"))
            {
                return RecurrenceKind::Unknown;
            }
            if let Some(value) = relation.get("relation") {
                let Some(kinds) = value.as_object() else {
                    return RecurrenceKind::Unknown;
                };
                if kinds.values().any(|value| value.as_bool() != Some(true)) {
                    return RecurrenceKind::Unknown;
                }
                recurring |= kinds.contains_key("first") || kinds.contains_key("next");
            }
        }
    }
    if recurring {
        return RecurrenceKind::Series;
    }

    // A native event can omit legitimate optional fields, but not its event
    // identity/start. Do not authorize mutation from malformed DTO fallbacks.
    if !["id", "uid"]
        .iter()
        .all(|name| event[*name].as_str().is_some_and(|s| !s.trim().is_empty()))
        || !event["start"].as_str().is_some_and(valid_local_datetime)
        || !event["calendarIds"].as_object().is_some_and(|ids| {
            !ids.is_empty()
                && ids
                    .iter()
                    .all(|(id, value)| !id.is_empty() && value.as_bool() == Some(true))
        })
        || ["title", "description", "timeZone", "duration"]
            .iter()
            .any(|name| present(name).is_some_and(|value| !value.is_string()))
        || present("showWithoutTime").is_some_and(|value| !value.is_boolean())
        || [("locations", "Location"), ("participants", "Participant")]
            .iter()
            .any(|(name, expected_type)| {
                present(name).is_some_and(|value| {
                    !value.as_object().is_some_and(|objects| {
                        objects.values().all(|object| {
                            object.is_object()
                                && object
                                    .get("@type")
                                    .is_none_or(|value| value.as_str() == Some(*expected_type))
                        })
                    })
                })
            })
    {
        return RecurrenceKind::Unknown;
    }
    RecurrenceKind::Standalone
}

/// Validate rule identity without restricting recurrence to the local expander.
fn valid_recurrence_rule(rule: &serde_json::Value) -> bool {
    rule.is_object()
        && rule
            .get("@type")
            .is_none_or(|value| value.as_str() == Some("RecurrenceRule"))
        && matches!(
            rule["frequency"].as_str(),
            Some("yearly" | "monthly" | "weekly" | "daily" | "hourly" | "minutely" | "secondly")
        )
}

impl JmapConnection {
    /// List all JMAP calendars for the account.
    pub async fn list_jmap_calendars(&self, config: &JmapConfig) -> Result<Vec<JmapCalendar>> {
        log::debug!("JMAP listing calendars");
        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["Calendar/get", {
                    "accountId": self.account_id,
                    "properties": ["id", "name", "color", "isDefault"]
                }, "c1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        let calendars_json = resp["methodResponses"][0][1]["list"]
            .as_array()
            .ok_or_else(|| Error::Other("Invalid Calendar/get response".into()))?;

        let mut calendars = Vec::new();
        for cal in calendars_json {
            let id = cal["id"].as_str().unwrap_or("").to_string();
            let name = cal["name"].as_str().unwrap_or("Untitled").to_string();
            let color = cal["color"].as_str().map(|s| s.to_string());
            let is_default = cal["isDefault"].as_bool().unwrap_or(false);

            log::debug!("  calendar: {} ({}) default={}", name, id, is_default);
            calendars.push(JmapCalendar {
                id,
                name,
                color,
                is_default,
            });
        }
        log::info!("JMAP found {} calendars", calendars.len());
        Ok(calendars)
    }

    /// Update the JMAP `color` property on a calendar via
    /// `Calendar/set`. JMAP calendars (RFC 8984 / "JSCalendar") store
    /// color as a CSS-format string, conventionally a `#RRGGBB` hex.
    /// Stalwart and Cyrus both honor the property; servers that
    /// don't will surface the rejection in `notUpdated` and we
    /// return that as an error so the caller can roll back.
    pub async fn set_calendar_color(
        &self,
        config: &JmapConfig,
        calendar_id: &str,
        hex: &str,
    ) -> Result<()> {
        log::info!("JMAP set color for calendar {} -> {}", calendar_id, hex);

        let mut update = serde_json::Map::new();
        update.insert(calendar_id.to_string(), serde_json::json!({ "color": hex }));

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["Calendar/set", {
                    "accountId": self.account_id,
                    "update": update
                }, "c1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notUpdated"][calendar_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP Calendar/set rejected color update: {}",
                desc
            )));
        }

        log::info!("JMAP color set for calendar {}", calendar_id);
        Ok(())
    }

    /// Rename a JMAP calendar via `Calendar/set` with an update
    /// entry whose `name` field carries the new display name.
    pub async fn rename_calendar(
        &self,
        config: &JmapConfig,
        calendar_id: &str,
        new_name: &str,
    ) -> Result<()> {
        log::info!("JMAP rename calendar: id={} -> {}", calendar_id, new_name);

        // Build the update map by hand: `serde_json::json!({ calendar_id: ... })`
        // would emit the literal key "calendar_id", not the id's value.
        let mut update = serde_json::Map::new();
        update.insert(
            calendar_id.to_string(),
            serde_json::json!({ "name": new_name }),
        );

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["Calendar/set", {
                    "accountId": self.account_id,
                    "update": update
                }, "c1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notUpdated"][calendar_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP Calendar/set rejected rename: {}",
                desc
            )));
        }

        log::info!("JMAP renamed calendar {}", calendar_id);
        Ok(())
    }

    /// Fetch calendar events, optionally filtered by calendar_id.
    /// Uses CalendarEvent/query + CalendarEvent/get with JSCalendar format.
    pub async fn fetch_calendar_events(
        &self,
        config: &JmapConfig,
        calendar_id: Option<&str>,
    ) -> Result<Vec<JmapCalendarEvent>> {
        log::debug!("JMAP fetching calendar events (calendar={:?})", calendar_id);

        // Note: Stalwart doesn't support "inCalendars" filter, so we fetch all
        // events and filter by calendarIds client-side.
        let request = calendar_events_request(&self.account_id);

        let resp = self.api_request(&request, config).await?;
        log::debug!(
            "JMAP CalendarEvent response: {}",
            serde_json::to_string(&resp).unwrap_or_default()
        );

        // Check if the query returned an error
        if resp["methodResponses"][0][0].as_str() == Some("error") {
            let desc = resp["methodResponses"][0][1]["description"]
                .as_str()
                .unwrap_or("Unknown");
            log::error!("JMAP CalendarEvent/query error: {}", desc);
            return Ok(vec![]);
        }

        // The get response might be at index 1 or could be missing if query returned no IDs
        let events_json = match resp["methodResponses"][1][1]["list"].as_array() {
            Some(list) => list.clone(),
            None => {
                log::debug!("JMAP CalendarEvent/get returned no list, possibly empty");
                return Ok(vec![]);
            }
        };

        let mut events = Vec::new();
        for ev in events_json {
            let recurrence_kind = classify_recurrence(&ev);
            let id = ev["id"].as_str().unwrap_or("").to_string();
            let title = ev["title"].as_str().unwrap_or("(No title)").to_string();
            let description = ev["description"].as_str().map(|s| s.to_string());
            let uid = ev["uid"].as_str().map(|s| s.to_string());

            // calendarIds is a map { "cal-id": true, ... } — pick the first key
            let cal_id = ev["calendarIds"]
                .as_object()
                .and_then(|m| m.keys().next().cloned())
                .unwrap_or_default();

            // Location: JSCalendar uses "locations" as a map { id: { name: "..." } }
            let location = ev["locations"]
                .as_object()
                .and_then(|m| m.values().next())
                .and_then(|loc| loc["name"].as_str())
                .map(|s| s.to_string());

            // Start datetime — JSCalendar uses "start" as local time + "timeZone" as IANA id.
            let raw_start = ev["start"].as_str().unwrap_or("").to_string();
            let event_tz = ev["timeZone"].as_str().unwrap_or("").to_string();
            let start = if raw_start.is_empty() {
                raw_start.clone()
            } else {
                crate::calendar::timezone::to_utc(&raw_start, &event_tz)
            };

            let all_day = ev["showWithoutTime"].as_bool().unwrap_or(false);

            let duration_str = ev["duration"].as_str().unwrap_or("PT1H");
            let end = {
                let e = compute_end_from_duration(start.trim_end_matches('Z'), duration_str);
                if start.ends_with('Z') && !e.ends_with('Z') {
                    format!("{}Z", e)
                } else {
                    e
                }
            };

            let event_tz_opt = if event_tz.is_empty() {
                None
            } else {
                Some(event_tz.clone())
            };

            // Recurrence: JSCalendar carries an array of RecurrenceRule
            // objects — convert to the app's canonical iCal RRULE string
            // so the local DB, the frontend expander and the other
            // backends all agree on one format.
            let recurrence_rules = ev["recurrenceRules"]
                .as_array()
                .filter(|rules| !rules.is_empty())
                .map(Vec::as_slice)
                .or_else(|| {
                    ev.get("recurrenceRule")
                        .filter(|rule| rule.is_object())
                        .map(std::slice::from_ref)
                });
            let recurrence_rule = recurrence_rules
                .and_then(|rules| {
                    crate::calendar::recurrence::jscalendar_to_rrule(rules, Some(&event_tz))
                        .or_else(|| {
                            log::warn!(
                                "JMAP event {} has a recurrence rule not supported by the local expander",
                                id
                            );
                            serde_json::to_string(rules).ok()
                        })
                });

            // Participants: supports both JSCalendar-bis (calendarAddress) and old format (sendTo.imip)
            let mut organizer_email = None;
            let mut attendees: Vec<serde_json::Value> = Vec::new();
            if let Some(participants) = ev["participants"].as_object() {
                for (_pid, p) in participants {
                    // Try calendarAddress (JSCalendar-bis), then sendTo.imip (old), then email
                    let email = p["calendarAddress"]
                        .as_str()
                        .map(|s| s.trim_start_matches("mailto:").to_string())
                        .or_else(|| {
                            p["sendTo"]
                                .as_object()
                                .and_then(|s| s.get("imip"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.trim_start_matches("mailto:").to_string())
                        })
                        .or_else(|| p["email"].as_str().map(|s| s.to_string()));
                    let name = p["name"].as_str().map(|s| s.to_string());
                    let mut status = p["participationStatus"]
                        .as_str()
                        .unwrap_or("needs-action")
                        .to_string();
                    let roles = p["roles"].as_object();
                    let is_owner = roles.map(|r| r.contains_key("owner")).unwrap_or(false);

                    if is_owner {
                        organizer_email = email.clone();
                        // Organizer is implicitly "accepted" — they created the event
                        if status == "needs-action" {
                            status = "accepted".to_string();
                        }
                    }
                    if let Some(ref em) = email {
                        attendees.push(serde_json::json!({
                            "email": em,
                            "name": name,
                            "status": status,
                        }));
                    }
                }
            }
            let attendees_json = if attendees.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&attendees).unwrap_or_default())
            };

            log::debug!(
                "  event: {} ({}) start={} end={} attendees={}",
                title,
                id,
                start,
                end,
                attendees.len()
            );
            events.push(JmapCalendarEvent {
                id,
                calendar_id: cal_id,
                title,
                description,
                location,
                start,
                end,
                all_day,
                timezone: event_tz_opt,
                recurrence_rule,
                recurrence_kind,
                recurrence_rule_is_complete: false,
                uid,
                organizer_email,
                attendees_json,
            });
        }

        // Client-side filter by calendar if requested
        let filtered = if let Some(cal_id) = calendar_id {
            events
                .into_iter()
                .filter(|e| e.calendar_id == cal_id)
                .collect()
        } else {
            events
        };

        log::info!("JMAP fetched {} calendar events", filtered.len());
        Ok(filtered)
    }

    /// Create a calendar event on the server via CalendarEvent/set.
    /// Returns the server-assigned event ID.
    pub async fn create_calendar_event(
        &self,
        config: &JmapConfig,
        event: &JmapCalendarEvent,
    ) -> Result<String> {
        let recurrence_rules = event.creation_recurrence_rules()?;
        log::info!(
            "JMAP creating calendar event: '{}' organizer={:?} attendees={:?}",
            event.title,
            event.organizer_email,
            event.attendees_json
        );

        let uid = event
            .uid
            .clone()
            .unwrap_or_else(|| format!("{}@chithi", uuid::Uuid::new_v4()));

        let duration = compute_duration(&event.start, &event.end);

        let mut event_obj = serde_json::json!({
            "@type": "Event",
            "calendarIds": { &event.calendar_id: true },
            "title": event.title,
            "start": event.start,
            "duration": duration,
            "showWithoutTime": event.all_day,
            "uid": uid,
        });

        if let Some(ref desc) = event.description {
            event_obj["description"] = serde_json::json!(desc);
        }
        if let Some(ref loc) = event.location {
            event_obj["locations"] = serde_json::json!({
                "loc1": { "@type": "Location", "name": loc }
            });
        }
        if let Some(rules) = recurrence_rules {
            event_obj["recurrenceRules"] = rules;
        }

        // Add participants (organizer + attendees)
        // Uses JSCalendar-bis format (draft-ietf-calext-jscalendarbis-14):
        // - "calendarAddress" instead of "sendTo"
        // - No "replyTo" on the event
        let mut participants = serde_json::Map::new();
        if let Some(ref org_email) = event.organizer_email {
            if !org_email.is_empty() {
                participants.insert(
                    "organizer".to_string(),
                    serde_json::json!({
                        "@type": "Participant",
                        "calendarAddress": format!("mailto:{}", org_email),
                        "roles": {"owner": true, "attendee": true},
                        "participationStatus": "accepted",
                        "expectReply": false,
                    }),
                );
            }
        }
        if let Some(ref att_json) = event.attendees_json {
            if let Ok(attendees) = serde_json::from_str::<Vec<serde_json::Value>>(att_json) {
                for (i, att) in attendees.iter().enumerate() {
                    let email = att["email"].as_str().unwrap_or_default();
                    if !email.is_empty() {
                        let status = att["status"].as_str().unwrap_or("needs-action");
                        participants.insert(
                            format!("att{}", i),
                            serde_json::json!({
                                "@type": "Participant",
                                "calendarAddress": format!("mailto:{}", email),
                                "roles": {"attendee": true},
                                "participationStatus": status,
                                "expectReply": true,
                            }),
                        );
                    }
                }
            }
        }
        if !participants.is_empty() {
            event_obj["participants"] = serde_json::Value::Object(participants);
        }

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["CalendarEvent/set", {
                    "accountId": self.account_id,
                    "create": {
                        "new1": event_obj
                    }
                }, "s1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        // Check for creation errors
        if let Some(err) = resp["methodResponses"][0][1]["notCreated"]["new1"].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP create calendar event failed: {}",
                desc
            )));
        }

        let created_id = resp["methodResponses"][0][1]["created"]["new1"]["id"]
            .as_str()
            .ok_or_else(|| Error::Other("No id in CalendarEvent/set create response".into()))?
            .to_string();

        log::info!("JMAP created calendar event id={}", created_id);
        Ok(created_id)
    }

    /// Update a participant's status on a calendar event via JMAP patch.
    /// Uses the JSCalendar-bis path syntax: participants/<id>/participationStatus
    pub async fn update_participant_status(
        &self,
        config: &JmapConfig,
        event_id: &str,
        participant_key: &str,
        status: &str,
    ) -> Result<()> {
        log::info!(
            "JMAP updating participant {} status to {} on event {}",
            participant_key,
            status,
            event_id
        );

        let patch_key = format!("participants/{}/participationStatus", participant_key);
        let mut patch = serde_json::Map::new();
        patch.insert(patch_key, serde_json::json!(status));

        let mut update = serde_json::Map::new();
        update.insert(event_id.to_string(), serde_json::Value::Object(patch));

        let request = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
            "methodCalls": [
                ["CalendarEvent/set", {
                    "accountId": self.account_id,
                    "update": update
                }, "u1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notUpdated"][event_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP update participant failed: {}",
                desc
            )));
        }

        log::info!("JMAP updated participant status on event {}", event_id);
        Ok(())
    }

    /// Delete a calendar event on the server via CalendarEvent/set.
    pub async fn delete_calendar_event(&self, config: &JmapConfig, event_id: &str) -> Result<()> {
        log::info!("JMAP deleting calendar event: id={}", event_id);

        let request = serde_json::json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:calendars"
            ],
            "methodCalls": [
                ["CalendarEvent/set", {
                    "accountId": self.account_id,
                    "destroy": [event_id]
                }, "d1"]
            ]
        });

        let resp = self.api_request(&request, config).await?;

        if let Some(err) = resp["methodResponses"][0][1]["notDestroyed"][event_id].as_object() {
            let desc = err
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("Unknown error");
            return Err(Error::Other(format!(
                "JMAP delete calendar event failed: {}",
                desc
            )));
        }

        log::info!("JMAP deleted calendar event id={}", event_id);
        Ok(())
    }
}

/// Compute end datetime from a start datetime and an ISO 8601 duration string.
/// Handles simple cases like PT1H, PT30M, P1D, PT1H30M, etc.
/// Falls back to start + 1 hour if parsing fails.
fn compute_end_from_duration(start: &str, duration: &str) -> String {
    use chrono::{Duration, NaiveDate, NaiveDateTime};

    let total_seconds = parse_iso8601_duration_seconds(duration);

    // Try parsing as full datetime first, then as date-only
    if let Ok(dt) = NaiveDateTime::parse_from_str(start, "%Y-%m-%dT%H:%M:%S") {
        let end = dt + Duration::seconds(total_seconds);
        return end.format("%Y-%m-%dT%H:%M:%S").to_string();
    }
    if let Ok(d) = NaiveDate::parse_from_str(start, "%Y-%m-%d") {
        let dt = d.and_hms_opt(0, 0, 0).unwrap();
        let end = dt + Duration::seconds(total_seconds);
        if total_seconds % 86400 == 0 {
            return end.format("%Y-%m-%d").to_string();
        }
        return end.format("%Y-%m-%dT%H:%M:%S").to_string();
    }
    // Fallback: return start as-is
    start.to_string()
}

/// Compute an ISO 8601 duration string from start and end datetimes.
/// Returns "P1D" for full-day spans, "PT{n}H" / "PT{n}M" for shorter spans.
fn compute_duration(start: &str, end: &str) -> String {
    use chrono::NaiveDateTime;

    let start_dt = NaiveDateTime::parse_from_str(start, "%Y-%m-%dT%H:%M:%S");
    let end_dt = NaiveDateTime::parse_from_str(end, "%Y-%m-%dT%H:%M:%S");

    if let (Ok(s), Ok(e)) = (start_dt, end_dt) {
        let diff = e - s;
        let total_secs = diff.num_seconds();
        if total_secs <= 0 {
            return "PT1H".to_string();
        }
        let days = total_secs / 86400;
        let remaining = total_secs % 86400;
        let hours = remaining / 3600;
        let minutes = (remaining % 3600) / 60;
        let secs = remaining % 60;

        if remaining == 0 && days > 0 {
            return format!("P{}D", days);
        }
        let mut s = String::from("P");
        if days > 0 {
            s.push_str(&format!("{}D", days));
        }
        s.push('T');
        if hours > 0 {
            s.push_str(&format!("{}H", hours));
        }
        if minutes > 0 {
            s.push_str(&format!("{}M", minutes));
        }
        if secs > 0 {
            s.push_str(&format!("{}S", secs));
        }
        // Ensure we have at least something after 'T'
        if s.ends_with('T') {
            s.push_str("0S");
        }
        return s;
    }
    // Fallback
    "PT1H".to_string()
}

/// Parse a simple ISO 8601 duration like "P1D", "PT1H30M", "PT45M" into total seconds.
fn parse_iso8601_duration_seconds(dur: &str) -> i64 {
    let mut total: i64 = 0;
    let mut num_buf = String::new();
    let mut in_time = false;

    for ch in dur.chars() {
        match ch {
            'P' => {}
            'T' => {
                in_time = true;
            }
            '0'..='9' => {
                num_buf.push(ch);
            }
            'D' => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 86400;
                }
                num_buf.clear();
            }
            'H' if in_time => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 3600;
                }
                num_buf.clear();
            }
            'M' if in_time => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 60;
                }
                num_buf.clear();
            }
            'S' if in_time => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n;
                }
                num_buf.clear();
            }
            'W' => {
                if let Ok(n) = num_buf.parse::<i64>() {
                    total += n * 604800;
                }
                num_buf.clear();
            }
            _ => {
                num_buf.clear();
            }
        }
    }

    if total == 0 {
        3600
    } else {
        total
    } // default 1 hour
}

#[cfg(test)]
mod recurrence_tests {
    use super::{
        calendar_events_request, classify_recurrence, faithful_local_recurrence_rules,
        JmapCalendarEvent,
    };
    use crate::calendar::RecurrenceKind;
    use serde_json::{json, Value};

    fn event() -> Value {
        json!({
            "@type": "Event",
            "id": "event-1",
            "uid": "uid-1",
            "calendarIds": {"calendar-1": true},
            "start": "2026-09-13T10:00:00"
        })
    }

    #[test]
    fn creation_never_drops_malformed_or_unrepresentable_rrule_parts() {
        for rule in [
            "",
            "FREQ=HOURLY",
            "FREQ=WEEKLY;BYSETPOS=1",
            "FREQ=WEEKLY;COUNT=bad",
            "FREQ=WEEKLY;COUNT=0",
            "FREQ=WEEKLY;INTERVAL=bad",
            "FREQ=WEEKLY;INTERVAL=0",
            "FREQ=WEEKLY;UNTIL=bad",
            "FREQ=WEEKLY;UNTIL=20260230",
            "FREQ=WEEKLY;FREQ=DAILY",
            "FREQ=WEEKLY;COUNT=2;UNTIL=20261001",
            "[{\"frequency\":\"weekly\"}]",
        ] {
            assert!(
                faithful_local_recurrence_rules(rule, None).is_none(),
                "{rule}"
            );
        }
        for rule in [
            "FREQ=WEEKLY;COUNT=4",
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO",
            "FREQ=DAILY;UNTIL=20261231",
        ] {
            assert!(
                faithful_local_recurrence_rules(rule, None).is_some(),
                "{rule}"
            );
        }
    }

    #[test]
    fn serialization_cannot_invent_proof_of_complete_local_series_recurrence() {
        let mut local = crate::backend::testutil::event();
        local.recurrence_kind = RecurrenceKind::Series;
        local.recurrence_rule = Some("FREQ=WEEKLY;COUNT=4".into());
        let wire = JmapCalendarEvent::for_local_creation(&local, "calendar").unwrap();
        let restored: JmapCalendarEvent =
            serde_json::from_value(serde_json::to_value(wire).unwrap()).unwrap();
        assert_eq!(restored.recurrence_kind, RecurrenceKind::Series);
        assert!(restored.creation_recurrence_rules().is_err());
    }

    #[test]
    fn get_requests_native_events_without_a_version_specific_property_list() {
        let request = calendar_events_request("account-1");
        let calls = &request["methodCalls"];
        assert_eq!(calls[0][0], "CalendarEvent/query");
        assert_eq!(calls[0][1]["limit"], 1000);
        assert!(calls[0][1].get("expandRecurrences").is_none());
        assert_eq!(calls[1][0], "CalendarEvent/get");
        let get = &calls[1][1];
        assert_eq!(get["accountId"], "account-1");
        assert_eq!(get["#ids"]["path"], "/ids");
        assert!(get.get("recurrenceOverridesBefore").is_none());
        assert!(get.get("recurrenceOverridesAfter").is_none());
        assert!(get.get("properties").is_none());
        assert!(get.get("reduceParticipants").is_none());
    }

    #[test]
    fn standalone_accepts_omitted_null_and_empty_optional_recurrence_metadata() {
        let mut event = event();
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Standalone);
        for name in [
            "recurrenceId",
            "recurrenceIdTimeZone",
            "recurrenceRules",
            "recurrenceRule",
            "excludedRecurrenceRules",
            "recurrenceOverrides",
            "baseEventId",
            "relatedTo",
        ] {
            event[name] = Value::Null;
        }
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Standalone);
        event["recurrenceRules"] = json!([]);
        event["excludedRecurrenceRules"] = json!([]);
        event["recurrenceOverrides"] = json!({});
        event["excluded"] = json!(false);
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Standalone);
    }

    #[test]
    fn series_includes_overrides_exclusions_unsupported_rules_and_bis_rule() {
        for (name, value) in [
            (
                "recurrenceRules",
                json!([{"@type": "RecurrenceRule", "frequency": "weekly"}]),
            ),
            (
                "recurrenceRules",
                json!([{"frequency": "hourly"}, {"frequency": "yearly"}]),
            ),
            ("recurrenceRule", json!({"frequency": "daily"})),
            ("excludedRecurrenceRules", json!([{"frequency": "monthly"}])),
            ("recurrenceOverrides", json!({"2026-09-20T10:00:00": {}})),
            (
                "recurrenceOverrides",
                json!({"2026-09-20T10:00:00": {"excluded": true}}),
            ),
            (
                "relatedTo",
                json!({"previous": {"relation": {"first": true}}}),
            ),
        ] {
            let mut event = event();
            event[name] = value;
            assert_eq!(
                classify_recurrence(&event),
                RecurrenceKind::Series,
                "{event}"
            );
        }
    }

    #[test]
    fn detached_and_server_expanded_instances_are_occurrences() {
        let mut event = event();
        event["recurrenceId"] = json!("2026-09-20T10:00:00");
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Occurrence);
        event["recurrenceId"] = json!("2026-09-20T10:00:00.123");
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Occurrence);
        event["recurrenceIdTimeZone"] = json!("Europe/Stockholm");
        event["baseEventId"] = json!("base-1");
        event["recurrenceRules"] = Value::Null;
        event["recurrenceRule"] = Value::Null;
        event["recurrenceOverrides"] = Value::Null;
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Occurrence);
        event["recurrenceId"] = json!("2026-09-20T00:00:00");
        event["showWithoutTime"] = json!(true);
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Occurrence);
        event["recurrenceRules"] = json!([{"frequency": "weekly"}]);
        assert_eq!(classify_recurrence(&event), RecurrenceKind::Unknown);
    }

    #[test]
    fn malformed_or_ambiguous_provider_metadata_cannot_become_standalone() {
        for (name, value) in [
            ("@type", json!("Task")),
            ("@type", json!("VendorEvent")),
            ("@type", Value::Null),
            ("recurrenceId", json!(true)),
            ("recurrenceId", json!("")),
            ("recurrenceId", json!("2026-02-30T10:00:00")),
            ("recurrenceId", json!("2026-09-20T10:00:00Z")),
            ("recurrenceId", json!("2026-09-20T10:00:00.000")),
            ("recurrenceId", json!("2026-09-20")),
            ("recurrenceIdTimeZone", json!("UTC")),
            ("baseEventId", json!("base-1")),
            ("baseEventId", json!(false)),
            ("recurrenceRules", json!({})),
            ("recurrenceRules", json!([null])),
            (
                "recurrenceRules",
                json!([{"@type": "Unknown", "frequency": "weekly"}]),
            ),
            ("recurrenceRules", json!([{"frequency": false}])),
            ("recurrenceRule", json!([])),
            ("recurrenceRule", json!({})),
            ("excludedRecurrenceRules", json!(false)),
            ("recurrenceOverrides", json!([])),
            ("recurrenceOverrides", json!({"invalid": {}})),
            ("recurrenceOverrides", json!({"2026-09-20T10:00:00": null})),
            (
                "recurrenceOverrides",
                json!({"2026-09-20T10:00:00": {"@type": "Task"}}),
            ),
            ("excluded", json!(true)),
            ("excluded", json!("false")),
            ("relatedTo", json!([])),
            ("id", json!("")),
            ("uid", Value::Null),
            ("start", json!(42)),
            ("calendarIds", json!({"calendar-1": false})),
            ("showWithoutTime", json!("false")),
            ("locations", json!({"location-1": {"@type": "Unknown"}})),
            ("participants", json!({"participant-1": false})),
        ] {
            let mut event = event();
            event[name] = value;
            assert_eq!(
                classify_recurrence(&event),
                RecurrenceKind::Unknown,
                "{event}"
            );
        }
    }
}
