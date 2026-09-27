//! Internal, provider-neutral snapshots used by scoped calendar mutations.
//!
//! A set is one standalone event or a master and its finite override set. It is
//! not an expanded list of every occurrence. Native payloads belong to adapters
//! and private persistence; renderer responses must use separate safe DTOs.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use super::{recurrence_identity::OccurrenceFields, CalendarEvent, RecurrenceKind};
use crate::error::{Error, Result};

/// Exact provider resource and validator. Never return this through IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCalendarResource {
    pub protocol: String,
    pub calendar_id: String,
    pub event_id: String,
    pub revision: Option<String>,
    pub data: String,
}

/// An exception or exclusion keyed by the original, not effective, start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarOverride {
    /// ISO date for an all-day master; canonical UTC RFC3339 otherwise.
    pub original_start: String,
    /// `None` excludes the original slot. A live override contains complete
    /// effective event content, not a patch whose inheritance must be guessed.
    pub event: Option<CalendarEvent>,
    pub native: Option<NativeCalendarResource>,
}

/// Private canonical snapshot, including exceptions outside the displayed range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEventSet {
    pub event: CalendarEvent,
    pub overrides: Vec<CalendarOverride>,
    pub native: Option<NativeCalendarResource>,
    /// Explicit private content intent survives edits, selection and local moves.
    /// Fresh provider snapshots start with `None` and derive this from native data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<CalendarContentProvenance>,
}

impl CalendarEventSet {
    /// Whether any live component has recipients whose scheduling semantics
    /// must be preserved. This validates private participant provenance rather
    /// than trusting the renderer-safe attendee projection alone.
    pub(crate) fn has_attendees(&self) -> Result<bool> {
        if !self.semantic_content(None)?.attendees.is_empty() {
            return Ok(true);
        }
        for key in self
            .overrides
            .iter()
            .filter(|item| item.event.is_some())
            .map(|item| item.original_start.as_str())
        {
            if !self.semantic_content(Some(key))?.attendees.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Freeze native format and roles before changing fields or original positions.
    pub(crate) fn capture_content(&mut self) -> Result<()> {
        if self.content.is_some() {
            return Ok(());
        }
        let master = self.content_provenance(None)?;
        let overrides = self
            .overrides
            .iter()
            .filter(|item| item.event.is_some())
            .map(|item| {
                Ok((
                    item.original_start.clone(),
                    self.content_provenance(Some(&item.original_start))?,
                ))
            })
            .collect::<Result<_>>()?;
        self.content = Some(CalendarContentProvenance { master, overrides });
        Ok(())
    }

    /// Select a replacement's complete content without losing its native format.
    pub(crate) fn standalone_at(&self, key: &str) -> Result<Self> {
        let mut set = self.clone();
        set.capture_content()?;
        let provenance = set.content_provenance(Some(key))?;
        let mut event = super::actions::selected_event(&set, Some(key))?;
        event.recurrence_kind = RecurrenceKind::Standalone;
        event.recurrence_rule = None;
        let native = self
            .overrides
            .iter()
            .find(|item| item.original_start == key)
            .and_then(|item| item.native.clone())
            .or_else(|| self.native.clone());
        Ok(Self {
            event,
            overrides: Vec::new(),
            native,
            content: Some(CalendarContentProvenance {
                master: provenance,
                overrides: BTreeMap::new(),
            }),
        })
    }

    pub(crate) fn mark_description_plain(&mut self, key: Option<&str>) -> Result<()> {
        self.capture_content()?;
        let content = self.content.as_mut().expect("captured content");
        let provenance = match key {
            None => &mut content.master,
            Some(key) => content
                .overrides
                .entry(key.to_owned())
                .or_insert_with(|| content.master.clone()),
        };
        provenance.format = DescriptionFormat::Plain;
        Ok(())
    }

    pub(crate) fn remap_content(&mut self, positions: &[(String, String)]) {
        if let Some(content) = &mut self.content {
            let old = std::mem::take(&mut content.overrides);
            for (before, after) in positions {
                if let Some(value) = old.get(before) {
                    content.overrides.insert(after.clone(), value.clone());
                }
            }
        }
    }

    /// The MIME type used by adapters when materializing the intended body.
    pub(crate) fn description_content_type(&self, key: Option<&str>) -> Result<&'static str> {
        Ok(match self.content_provenance(key)?.format {
            DescriptionFormat::Plain => "text/plain",
            DescriptionFormat::Html => "text/html",
        })
    }

    /// Establish explicit new-meeting intent before creation and verification.
    /// Source responses are not transferable consent. The caller chooses the
    /// destination organizer; identity-preserving native moves must not call this.
    pub(crate) fn prepare_new_meeting(&mut self, organizer: &str) -> Result<()> {
        self.capture_content()?;
        let organizer = address(organizer)?;
        let mut participants = Vec::new();
        for key in std::iter::once(None).chain(
            self.overrides
                .iter()
                .filter(|item| item.event.is_some())
                .map(|item| Some(item.original_start.as_str())),
        ) {
            let mut content = self.semantic_content(key)?;
            // Reassigning ownership must not silently remove the former owner
            // from the meeting's people, including JSCalendar's implicit owner.
            if let Some(previous) = content.organizer.filter(|email| email != &organizer) {
                content
                    .attendees
                    .entry(previous)
                    .or_insert((ParticipantRole::Required, "needs-action"));
            }
            participants.push(content.attendees.into_iter().filter(|(email,_)| email != &organizer)
                .map(|(email,(role,_))| serde_json::json!({"email": email,
                    "role": match role {
                        ParticipantRole::Required => "required", ParticipantRole::Optional => "optional",
                        ParticipantRole::Resource => "resource", ParticipantRole::Chair => "chair",
                        ParticipantRole::NonParticipant => "non-participant",
                    }, "status": "needs-action"})).collect::<Vec<_>>());
        }
        for (event, attendees) in std::iter::once(&mut self.event)
            .chain(
                self.overrides
                    .iter_mut()
                    .filter_map(|item| item.event.as_mut()),
            )
            .zip(participants)
        {
            // Preserve provider/user display labels while rewriting only scheduling
            // intent. Names are labels, never participant identity evidence.
            let names: BTreeMap<_, _> = event
                .attendees_json
                .as_deref()
                .map(serde_json::from_str::<Vec<serde_json::Value>>)
                .transpose()
                .map_err(|_| semantic_error("invalid attendees JSON"))?
                .unwrap_or_default()
                .into_iter()
                .filter_map(|a| Some((address(a["email"].as_str()?).ok()?, a["name"].clone())))
                .collect();
            let attendees: Vec<_> = attendees
                .into_iter()
                .map(|mut a| {
                    if let Some(name) = a["email"].as_str().and_then(|email| names.get(email)) {
                        if name.is_string() {
                            a["name"] = name.clone();
                        }
                    }
                    a
                })
                .collect();
            event.attendees_json =
                (!attendees.is_empty()).then(|| serde_json::Value::Array(attendees).to_string());
            event.organizer_email = Some(organizer.clone());
            event.my_status = None;
        }
        Ok(())
    }

    /// Private verification projection. The native resource supplies information
    /// deliberately absent from renderer fields (body format and attendee roles).
    pub(crate) fn semantic_content(&self, key: Option<&str>) -> Result<SemanticContent> {
        let event = super::actions::selected_event(self, key)?;
        let provenance = self.content_provenance(key)?;
        let organizer = event
            .organizer_email
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(address)
            .transpose()?;
        let mut attendees = BTreeMap::new();
        let mut identities = HashSet::new();
        if let Some(json) = &event.attendees_json {
            let values: Vec<serde_json::Value> =
                serde_json::from_str(json).map_err(|_| semantic_error("invalid attendees JSON"))?;
            for value in values {
                let email = address(
                    value["email"]
                        .as_str()
                        .ok_or_else(|| semantic_error("missing attendee identity"))?,
                )?;
                if !identities.insert(email.clone()) {
                    return Err(semantic_error("duplicate attendee identity"));
                }
                let status = status(optional_string(&value, "status")?.unwrap_or("needs-action"))?;
                let explicit_role = optional_string(&value, "role")?.map(role).transpose()?;
                let native_role = provenance.roles.get(&email).copied();
                if explicit_role.zip(native_role).is_some_and(|(a, b)| a != b) {
                    return Err(semantic_error("contradictory attendee role"));
                }
                let role = explicit_role
                    .or(native_role)
                    .unwrap_or(ParticipantRole::Required);
                // JSCalendar represents the owner as an accepted participant.
                // Only this exact organizer-equivalent entry is redundant.
                if organizer.as_deref() == Some(&email)
                    && status == "accepted"
                    && role == ParticipantRole::Required
                {
                    continue;
                }
                if attendees.insert(email, (role, status)).is_some() {
                    return Err(semantic_error("duplicate attendee identity"));
                }
            }
        }
        if provenance.roles.keys().any(|email| {
            !identities.contains(email)
                && organizer.as_ref() != Some(email)
                && provenance.organizer.as_ref() != Some(email)
        }) {
            return Err(semantic_error(
                "native participant is absent from the effective projection",
            ));
        }
        Ok(SemanticContent {
            description: event.description.unwrap_or_default().replace("\r\n", "\n"),
            format: provenance.format,
            organizer,
            attendees,
        })
    }

    fn content_provenance(&self, key: Option<&str>) -> Result<Provenance> {
        if let Some(content) = &self.content {
            return Ok(key
                .and_then(|key| content.overrides.get(key))
                .unwrap_or(&content.master)
                .clone());
        }
        let event = super::actions::selected_event(self, key)?;
        let native = key
            .and_then(|key| {
                self.overrides
                    .iter()
                    .find(|item| item.original_start == key)
                    .and_then(|item| item.native.as_ref())
            })
            .or(self.native.as_ref());
        let provenance = match native {
            Some(native) => provenance(native, &self.event, key, &event)?,
            None if event.ical_data.is_some() => ical_provenance(
                event.ical_data.as_deref().expect("checked iCalendar"),
                &self.event,
                key,
                &event,
            )?,
            None => Provenance::default(),
        };
        Ok(provenance)
    }

    pub fn validate(&self) -> Result<()> {
        event_fields(&self.event).validate()?;
        match self.event.recurrence_kind {
            RecurrenceKind::Standalone
                if self
                    .event
                    .recurrence_rule
                    .as_deref()
                    .is_none_or(str::is_empty)
                    && self.overrides.is_empty() => {}
            RecurrenceKind::Series
                if self
                    .event
                    .recurrence_rule
                    .as_deref()
                    .is_some_and(|rule| !rule.is_empty()) => {}
            _ => {
                return Err(Error::Other(
                    "Incomplete calendar event-set classification".into(),
                ))
            }
        }
        let mut positions = HashSet::new();
        for exception in &self.overrides {
            let position = canonical_position(&self.event, &exception.original_start)?;
            if position != exception.original_start || !positions.insert(position) {
                return Err(Error::Other(
                    "Duplicate or noncanonical original occurrence position".into(),
                ));
            }
            if let Some(event) = &exception.event {
                event_fields(event).validate()?;
                if event
                    .recurrence_rule
                    .as_deref()
                    .is_some_and(|rule| !rule.is_empty())
                {
                    return Err(Error::Other(
                        "An occurrence override cannot carry a recurrence rule".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn semantic_error(message: &str) -> Error {
    Error::Other(format!("Calendar semantic verification: {message}"))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum DescriptionFormat {
    #[default]
    Plain,
    Html,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ParticipantRole {
    Required,
    Optional,
    Resource,
    Chair,
    NonParticipant,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Provenance {
    format: DescriptionFormat,
    roles: BTreeMap<String, ParticipantRole>,
    organizer: Option<String>,
}

/// Private, journal-serializable metadata; never part of a renderer DTO.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarContentProvenance {
    master: Provenance,
    overrides: BTreeMap<String, Provenance>,
}

pub(crate) struct SemanticContent {
    description: String,
    format: DescriptionFormat,
    organizer: Option<String>,
    attendees: BTreeMap<String, (ParticipantRole, &'static str)>,
}

impl SemanticContent {
    pub(crate) fn equivalent(&self, other: &Self) -> Result<bool> {
        if self.organizer != other.organizer || self.attendees != other.attendees {
            return Ok(false);
        }
        if self.format == other.format && self.description == other.description {
            return Ok(true);
        }
        // A deliberately small grammar recognizes only materialized plain text.
        // No sanitizer, tag stripping, or browser textContent proves rich fidelity.
        let plain = |value: &Self| match value.format {
            DescriptionFormat::Plain => Some(value.description.clone()),
            DescriptionFormat::Html => materialized_plain(&value.description),
        };
        Ok(matches!((plain(self), plain(other)), (Some(a), Some(b)) if a == b))
    }
}

fn optional_string<'a>(object: &'a serde_json::Value, key: &str) -> Result<Option<&'a str>> {
    match object.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value)),
        _ => Err(semantic_error("invalid string property")),
    }
}

fn address(value: &str) -> Result<String> {
    let value = value.trim();
    let value = if value
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("mailto:"))
    {
        &value[7..]
    } else {
        value
    };
    if value.is_empty()
        || !value.contains('@')
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
        || value.contains(['?', '#', '<', '>'])
    {
        return Err(semantic_error("unsupported participant address"));
    }
    Ok(value.to_ascii_lowercase())
}

fn status(value: &str) -> Result<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "needs-action" | "needsaction" | "notresponded" | "none" => Ok("needs-action"),
        "accepted" => Ok("accepted"),
        "tentative" | "tentativelyaccepted" => Ok("tentative"),
        "declined" => Ok("declined"),
        "delegated" => Ok("delegated"),
        _ => Err(semantic_error("unsupported participant response")),
    }
}

fn role(value: &str) -> Result<ParticipantRole> {
    match value.to_ascii_lowercase().as_str() {
        "required" | "req-participant" | "attendee" => Ok(ParticipantRole::Required),
        "optional" | "opt-participant" => Ok(ParticipantRole::Optional),
        "resource" => Ok(ParticipantRole::Resource),
        "chair" => Ok(ParticipantRole::Chair),
        "non-participant" => Ok(ParticipantRole::NonParticipant),
        _ => Err(semantic_error("unsupported participant role")),
    }
}

fn provenance(
    native: &NativeCalendarResource,
    master: &CalendarEvent,
    key: Option<&str>,
    event: &CalendarEvent,
) -> Result<Provenance> {
    if native.protocol == "caldav" {
        return ical_provenance(&native.data, master, key, event);
    }
    let value: serde_json::Value = serde_json::from_str(&native.data)
        .map_err(|_| semantic_error("invalid native content provenance"))?;
    let value = if native.protocol == "jmap" {
        effective_jmap(&value, master, key)?
    } else {
        value
    };
    let mut result = Provenance::default();
    let description;
    let mut responses = BTreeMap::new();
    match native.protocol.as_str() {
        "google" => {
            // Calendar API descriptions are HTML, even without an explicit MIME type.
            result.format = DescriptionFormat::Html;
            description = optional_string(&value, "description")?.unwrap_or("");
            result.organizer = value["organizer"]["email"]
                .as_str()
                .map(address)
                .transpose()?;
            if let Some(values) = value.get("attendees") {
                for attendee in values
                    .as_array()
                    .ok_or_else(|| semantic_error("invalid Google attendees"))?
                {
                    let email = address(
                        attendee["email"]
                            .as_str()
                            .ok_or_else(|| semantic_error("missing Google recipient"))?,
                    )?;
                    for field in ["optional", "resource"] {
                        if attendee.get(field).is_some_and(|v| !v.is_boolean()) {
                            return Err(semantic_error("invalid Google role"));
                        }
                    }
                    if attendee["resource"] == true && attendee["optional"] == true {
                        return Err(semantic_error(
                            "combined optional/resource role needs explicit representation",
                        ));
                    }
                    let role = if attendee["resource"] == true {
                        ParticipantRole::Resource
                    } else if attendee["optional"] == true {
                        ParticipantRole::Optional
                    } else {
                        ParticipantRole::Required
                    };
                    responses.insert(
                        email.clone(),
                        status(
                            optional_string(attendee, "responseStatus")?.unwrap_or("needs-action"),
                        )?,
                    );
                    if result.roles.insert(email, role).is_some() {
                        return Err(semantic_error("duplicate Google attendee"));
                    }
                }
            }
        }
        "graph" => {
            description = value["body"]["content"]
                .as_str()
                .ok_or_else(|| semantic_error("missing Graph full body"))?;
            result.organizer = value["organizer"]["emailAddress"]["address"]
                .as_str()
                .map(address)
                .transpose()?;
            result.format = match value["body"]["contentType"]
                .as_str()
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("text") => DescriptionFormat::Plain,
                Some("html") => DescriptionFormat::Html,
                _ => return Err(semantic_error("missing Graph body contentType")),
            };
            for attendee in value["attendees"]
                .as_array()
                .ok_or_else(|| semantic_error("missing Graph attendees"))?
            {
                let email = address(
                    attendee["emailAddress"]["address"]
                        .as_str()
                        .ok_or_else(|| semantic_error("missing Graph recipient"))?,
                )?;
                let role = role(
                    attendee["type"]
                        .as_str()
                        .ok_or_else(|| semantic_error("missing Graph role"))?,
                )?;
                let response = status(
                    attendee["status"]["response"]
                        .as_str()
                        .ok_or_else(|| semantic_error("missing Graph response"))?,
                )?;
                responses.insert(email.clone(), response);
                if result.roles.insert(email, role).is_some() {
                    return Err(semantic_error("duplicate Graph attendee"));
                }
            }
        }
        "jmap" => {
            description = optional_string(&value, "description")?.unwrap_or("");
            result.format =
                match optional_string(&value, "descriptionContentType")?.unwrap_or("text/plain") {
                    "text/plain" => DescriptionFormat::Plain,
                    "text/html" => DescriptionFormat::Html,
                    _ => return Err(semantic_error("unsupported JSCalendar description type")),
                };
            if let Some(participants) = value.get("participants").filter(|v| !v.is_null()) {
                for participant in participants
                    .as_object()
                    .ok_or_else(|| semantic_error("invalid JSCalendar participants"))?
                    .values()
                {
                    let email = address(
                        participant["calendarAddress"]
                            .as_str()
                            .or_else(|| participant["sendTo"]["imip"].as_str())
                            .or_else(|| participant["email"].as_str())
                            .ok_or_else(|| semantic_error("missing JSCalendar recipient"))?,
                    )?;
                    let roles = participant["roles"]
                        .as_object()
                        .ok_or_else(|| semantic_error("missing JSCalendar roles"))?;
                    if let Some(imip) = participant["sendTo"]["imip"].as_str() {
                        if address(imip)? != email {
                            return Err(semantic_error(
                                "different JSCalendar scheduling recipient",
                            ));
                        }
                    }
                    if roles.iter().any(|(k, v)| {
                        !v.is_boolean()
                            || !matches!(
                                k.as_str(),
                                "owner" | "attendee" | "optional" | "chair" | "informational"
                            )
                    }) {
                        return Err(semantic_error("unsupported JSCalendar roles"));
                    }
                    if ["optional", "chair", "informational"]
                        .iter()
                        .filter(|key| roles.get(**key).is_some_and(|v| v == true))
                        .count()
                        > 1
                    {
                        return Err(semantic_error(
                            "combined JSCalendar roles need explicit representation",
                        ));
                    }
                    if roles.get("owner").is_some_and(|v| v == true)
                        && result.organizer.replace(email.clone()).is_some()
                    {
                        return Err(semantic_error("multiple JSCalendar owners"));
                    }
                    let role = if roles.get("chair").is_some_and(|v| v == true) {
                        ParticipantRole::Chair
                    } else if roles.get("optional").is_some_and(|v| v == true) {
                        ParticipantRole::Optional
                    } else if roles.get("informational").is_some_and(|v| v == true) {
                        ParticipantRole::NonParticipant
                    } else if roles.get("attendee").is_some_and(|v| v == true)
                        || roles.get("owner").is_some_and(|v| v == true)
                    {
                        ParticipantRole::Required
                    } else {
                        return Err(semantic_error("missing JSCalendar participation role"));
                    };
                    let role = match optional_string(participant, "kind")? {
                        Some("location" | "resource") if role == ParticipantRole::Required => {
                            ParticipantRole::Resource
                        }
                        None | Some("individual" | "group") => role,
                        _ => return Err(semantic_error("unsupported participant kind")),
                    };
                    let response = status(
                        optional_string(participant, "participationStatus")?
                            .unwrap_or("needs-action"),
                    )?;
                    responses.insert(email.clone(), response);
                    if result.roles.insert(email, role).is_some() {
                        return Err(semantic_error("duplicate JSCalendar attendee"));
                    }
                }
            }
        }
        _ => return Err(semantic_error("unknown native content provenance")),
    }
    verify_native_projection(result, description, responses, event)
}

/// Fresh snapshots must contain full canonical projections. Captured intent
/// instead uses explicit metadata, independently of stale native body/RSVP data.
fn verify_native_projection(
    provenance: Provenance,
    description: &str,
    mut responses: BTreeMap<String, &'static str>,
    event: &CalendarEvent,
) -> Result<Provenance> {
    if description.replace("\r\n", "\n")
        != event
            .description
            .as_deref()
            .unwrap_or("")
            .replace("\r\n", "\n")
    {
        return Err(semantic_error(
            "effective description is not the full native body; capture intent before editing",
        ));
    }
    let organizer = event
        .organizer_email
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(address)
        .transpose()?;
    if organizer != provenance.organizer {
        return Err(semantic_error(
            "organizer projection differs from native content",
        ));
    }
    let mut projected = BTreeMap::new();
    if let Some(json) = &event.attendees_json {
        let values: Vec<serde_json::Value> =
            serde_json::from_str(json).map_err(|_| semantic_error("invalid attendees JSON"))?;
        for value in values {
            let email = address(
                value["email"]
                    .as_str()
                    .ok_or_else(|| semantic_error("missing attendee identity"))?,
            )?;
            let response = status(optional_string(&value, "status")?.unwrap_or("needs-action"))?;
            if projected.insert(email, response).is_some() {
                return Err(semantic_error("duplicate projected attendee"));
            }
        }
    }
    if let Some(organizer) = &organizer {
        if provenance
            .roles
            .get(organizer)
            .is_none_or(|role| *role == ParticipantRole::Required)
        {
            if projected.get(organizer) == Some(&"accepted") {
                projected.remove(organizer);
            }
            if responses.get(organizer) == Some(&"accepted") {
                responses.remove(organizer);
            }
        }
    }
    if projected != responses {
        return Err(semantic_error(
            "attendee response projection differs from native content",
        ));
    }
    Ok(provenance)
}

fn effective_jmap(
    envelope: &serde_json::Value,
    master: &CalendarEvent,
    key: Option<&str>,
) -> Result<serde_json::Value> {
    let mut value = envelope
        .get("event")
        .filter(|v| v.is_object())
        .cloned()
        .ok_or_else(|| semantic_error("missing JSCalendar native event"))?;
    if value.get("baseEventId").is_some() || key.is_none() {
        return Ok(value);
    }
    let Some(overrides) = value.get("recurrenceOverrides").filter(|v| !v.is_null()) else {
        return Ok(value);
    };
    let overrides = overrides
        .as_object()
        .ok_or_else(|| semantic_error("invalid JSCalendar overrides"))?;
    let mut selected = None;
    for (position, patch) in overrides {
        let local = NaiveDateTime::parse_from_str(position, "%Y-%m-%dT%H:%M:%S%.f")
            .map_err(|_| semantic_error("invalid JSCalendar original position"))?;
        let canonical = if master.all_day {
            local.date().to_string()
        } else {
            let zone: chrono_tz::Tz = value["timeZone"]
                .as_str()
                .unwrap_or("UTC")
                .parse()
                .map_err(|_| semantic_error("unknown JSCalendar zone"))?;
            zone.from_local_datetime(&local)
                .single()
                .ok_or_else(|| semantic_error("ambiguous JSCalendar position"))?
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::AutoSi, true)
        };
        if Some(canonical.as_str()) == key && selected.replace(patch.clone()).is_some() {
            return Err(semantic_error("duplicate native position"));
        }
    }
    if let Some(patch) = selected {
        let patch = patch
            .as_object()
            .ok_or_else(|| semantic_error("invalid JSCalendar patch"))?;
        for path in patch.keys() {
            if patch
                .keys()
                .any(|other| other != path && other.starts_with(&format!("{path}/")))
            {
                return Err(semantic_error("overlapping JSCalendar patch"));
            }
        }
        for (path, replacement) in patch {
            let segments: Vec<_> = path
                .split('/')
                .map(|s| s.replace("~1", "/").replace("~0", "~"))
                .collect();
            let mut target = &mut value;
            for segment in &segments[..segments.len() - 1] {
                target = target
                    .get_mut(segment)
                    .ok_or_else(|| semantic_error("missing JSCalendar patch parent"))?;
            }
            let object = target
                .as_object_mut()
                .ok_or_else(|| semantic_error("invalid JSCalendar patch parent"))?;
            let key = segments.last().expect("split is nonempty");
            if replacement.is_null() {
                object.remove(key);
            } else {
                object.insert(key.clone(), replacement.clone());
            }
        }
    }
    Ok(value)
}

fn ical_provenance(
    data: &str,
    master: &CalendarEvent,
    key: Option<&str>,
    event: &CalendarEvent,
) -> Result<Provenance> {
    let data = icalendar::parser::unfold(data);
    let calendar = icalendar::parser::read_calendar(&data)
        .map_err(|_| semantic_error("invalid iCalendar provenance"))?;
    let mut selected = None;
    let mut base = None;
    for component in &calendar.components {
        if !component.name.as_str().eq_ignore_ascii_case("VEVENT") {
            continue;
        }
        if let Some(rid) = component.find_prop("RECURRENCE-ID") {
            let raw = rid.val.as_str();
            let position = if master.all_day {
                NaiveDate::parse_from_str(raw, "%Y%m%d")
                    .map_err(|_| semantic_error("invalid DATE recurrence ID"))?
                    .to_string()
            } else if let Ok(utc) = NaiveDateTime::parse_from_str(raw, "%Y%m%dT%H%M%SZ") {
                utc.and_utc().to_rfc3339_opts(SecondsFormat::AutoSi, true)
            } else {
                let local = NaiveDateTime::parse_from_str(raw, "%Y%m%dT%H%M%S")
                    .map_err(|_| semantic_error("invalid recurrence ID"))?;
                let zone = rid
                    .params
                    .iter()
                    .find(|p| p.key.as_str().eq_ignore_ascii_case("TZID"))
                    .and_then(|p| p.val.as_ref())
                    .map(|v| v.as_str())
                    .ok_or_else(|| semantic_error("floating recurrence ID"))?;
                let zone: chrono_tz::Tz = zone
                    .parse()
                    .map_err(|_| semantic_error("unknown recurrence ID zone"))?;
                zone.from_local_datetime(&local)
                    .single()
                    .ok_or_else(|| semantic_error("ambiguous recurrence ID"))?
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::AutoSi, true)
            };
            if key == Some(position.as_str()) && selected.replace(component).is_some() {
                return Err(semantic_error("duplicate iCalendar position"));
            }
        } else if base.replace(component).is_some() {
            return Err(semantic_error("multiple iCalendar masters"));
        }
    }
    let component = selected
        .or(base)
        .ok_or_else(|| semantic_error("missing iCalendar event"))?;
    let mut result = Provenance::default();
    let mut responses = BTreeMap::new();
    result.organizer = component
        .find_prop("ORGANIZER")
        .map(|p| address(p.val.as_str()))
        .transpose()?;
    // DESCRIPTION is RFC 5545 TEXT. Alternate rich representations cannot be
    // inferred from the plain DTO or silently discarded during a transfer.
    if component.find_prop("X-ALT-DESC").is_some()
        || component.find_prop("DESCRIPTION").is_some_and(|p| {
            p.params
                .iter()
                .any(|p| p.key.as_str().eq_ignore_ascii_case("ALTREP"))
        })
    {
        return Err(semantic_error(
            "alternate iCalendar body needs explicit rich provenance",
        ));
    }
    for prop in component
        .properties
        .iter()
        .filter(|p| p.name.as_str().eq_ignore_ascii_case("ATTENDEE"))
    {
        let email = address(prop.val.as_str())?;
        let parameter = |name: &str| {
            prop.params
                .iter()
                .find(|p| p.key.as_str().eq_ignore_ascii_case(name))
                .and_then(|p| p.val.as_ref())
                .map(|v| v.as_str())
        };
        let role = role(parameter("ROLE").unwrap_or("REQ-PARTICIPANT"))?;
        let role = match parameter("CUTYPE").map(str::to_ascii_uppercase).as_deref() {
            Some("RESOURCE" | "ROOM") if role == ParticipantRole::Required => {
                ParticipantRole::Resource
            }
            None | Some("INDIVIDUAL" | "GROUP") => role,
            _ => return Err(semantic_error("unsupported iCalendar participant kind")),
        };
        responses.insert(
            email.clone(),
            status(parameter("PARTSTAT").unwrap_or("NEEDS-ACTION"))?,
        );
        if result.roles.insert(email, role).is_some() {
            return Err(semantic_error("duplicate iCalendar attendee"));
        }
    }
    verify_native_projection(
        result,
        component
            .find_prop("DESCRIPTION")
            .map(|p| p.val.as_str())
            .unwrap_or(""),
        responses,
        event,
    )
}

/// Recognize only an exact, unstyled text container with explicit line breaks.
/// All rich tags, attributes, comments, CSS and unknown entities fail closed.
fn materialized_plain(html: &str) -> Option<String> {
    let mut body = html;
    for (open, close) in [
        ("<html>", "</html>"),
        ("<body>", "</body>"),
        ("<div>", "</div>"),
        ("<p>", "</p>"),
    ] {
        if body.starts_with(open) {
            body = body.strip_prefix(open)?.strip_suffix(close)?;
        }
    }
    let mut output = String::new();
    while !body.is_empty() {
        if body.starts_with('<') {
            let tag = ["<br>", "<br/>", "<br />"]
                .into_iter()
                .find(|tag| body.starts_with(tag))?;
            output.push('\n');
            body = &body[tag.len()..];
        } else if body.starts_with('&') {
            let end = body.find(';')?;
            let entity = &body[1..end];
            let c = match entity {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                    char::from_u32(u32::from_str_radix(&entity[2..], 16).ok()?)?
                }
                _ if entity.starts_with('#') => char::from_u32(entity[1..].parse().ok()?)?,
                _ => return None,
            };
            // HTML parsing remaps certain numeric references and controls.
            if c.is_ascii_control() || ('\u{80}'..='\u{9f}').contains(&c) {
                return None;
            }
            output.push(c);
            body = &body[end + 1..];
        } else {
            let c = body.chars().next()?;
            // Literal HTML whitespace collapses; it cannot prove a plain body's
            // newlines, tabs, leading/trailing or repeated spaces survived.
            if c == '\n' || c == '\r' || c == '\t' || c == '\0' {
                return None;
            }
            output.push(c);
            body = &body[c.len_utf8()..];
        }
    }
    if output.starts_with(' ')
        || output.ends_with(' ')
        || output.contains("  ")
        || output.contains(" \n")
        || output.contains("\n ")
    {
        return None;
    }
    Some(output)
}

pub fn event_fields(event: &CalendarEvent) -> OccurrenceFields {
    OccurrenceFields {
        title: event.title.clone(),
        description: event.description.clone(),
        location: event.location.clone(),
        start_time: event.start_time.clone(),
        end_time: event.end_time.clone(),
        all_day: event.all_day,
        timezone: event.timezone.clone(),
    }
}

pub fn apply_event_fields(event: &mut CalendarEvent, fields: &OccurrenceFields) {
    event.title.clone_from(&fields.title);
    event.description.clone_from(&fields.description);
    event.location.clone_from(&fields.location);
    event.start_time.clone_from(&fields.start_time);
    event.end_time.clone_from(&fields.end_time);
    event.all_day = fields.all_day;
    event.timezone.clone_from(&fields.timezone);
}

pub fn canonical_position(master: &CalendarEvent, value: &str) -> Result<String> {
    if master.all_day {
        NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map(|date| date.format("%Y-%m-%d").to_string())
            .map_err(|_| Error::Other("Original all-day position must be an ISO date".into()))
    } else {
        DateTime::parse_from_rfc3339(value)
            .map(|time| {
                time.with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::AutoSi, true)
            })
            .map_err(|_| {
                Error::Other("Original timed position must include an RFC3339 offset".into())
            })
    }
}
