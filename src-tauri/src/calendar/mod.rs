use serde::{Deserialize, Serialize};

pub mod ical;
pub mod recurrence;
pub mod timezone;

/// Recurrence classification supplied by a provider or known local creation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecurrenceKind {
    Standalone,
    Series,
    Occurrence,
    #[default]
    #[serde(other)]
    Unknown,
}

impl RecurrenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Standalone => "standalone",
            Self::Series => "series",
            Self::Occurrence => "occurrence",
        }
    }

    /// Unrecognized stored values carry no positive evidence of mutability.
    pub fn from_stored(value: &str) -> Self {
        match value {
            "standalone" => Self::Standalone,
            "series" => Self::Series,
            "occurrence" => Self::Occurrence,
            _ => Self::Unknown,
        }
    }

    /// Classify a known local creation, where the rule is all recurrence data.
    /// Never use this to infer classification for cached or provider events:
    /// an occurrence can lack a rule while still belonging to a series.
    pub fn from_rule(rule: Option<&str>) -> Self {
        if rule.is_some_and(|rule| !rule.is_empty()) {
            Self::Series
        } else {
            Self::Standalone
        }
    }
}

/// Provider-neutral calendar event shared by persistence and backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    pub account_id: String,
    pub calendar_id: String,
    pub uid: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_time: String,
    pub end_time: String,
    pub all_day: bool,
    pub timezone: Option<String>,
    pub recurrence_rule: Option<String>,
    #[serde(default)]
    pub recurrence_kind: RecurrenceKind,
    pub organizer_email: Option<String>,
    pub attendees_json: Option<String>,
    pub my_status: Option<String>,
    pub source_message_id: Option<String>,
    pub ical_data: Option<String>,
    pub remote_id: Option<String>,
    pub etag: Option<String>,
}

impl CalendarEvent {
    /// Require positive standalone evidence before an ordinary edit, delete, or move.
    pub fn ensure_mutable(&self) -> crate::error::Result<()> {
        use crate::error::CalendarMutationBlockReason;

        if self
            .recurrence_rule
            .as_deref()
            .is_some_and(|rule| !rule.is_empty())
            || matches!(
                self.recurrence_kind,
                RecurrenceKind::Series | RecurrenceKind::Occurrence
            )
        {
            return Err(CalendarMutationBlockReason::Recurring.into());
        }
        if self.recurrence_kind != RecurrenceKind::Standalone {
            return Err(CalendarMutationBlockReason::UnknownRecurrence.into());
        }
        Ok(())
    }
}

/// Attendee serialized inside [`CalendarEvent::attendees_json`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attendee {
    pub email: String,
    pub name: Option<String>,
    /// `accepted`, `tentative`, `declined`, or `needs-action`.
    pub status: String,
    /// Provider-confirmed account identity, used when the attendee address is an alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_self: Option<bool>,
}

/// Return the RSVP status for `email` from a provider attendee list.
pub(crate) fn attendee_status_for_email(attendees: &[Attendee], email: &str) -> Option<String> {
    attendees
        .iter()
        .find(|attendee| attendee.is_self == Some(true))
        .or_else(|| {
            attendees
                .iter()
                .find(|attendee| attendee.email.eq_ignore_ascii_case(email))
        })
        .map(|attendee| attendee.status.clone())
}

/// Parse a persisted provider attendee list and return this account's RSVP.
pub(crate) fn attendee_status_from_json(
    attendees_json: Option<&str>,
    email: &str,
) -> Option<String> {
    let attendees = serde_json::from_str::<Vec<Attendee>>(attendees_json?).ok()?;
    attendee_status_for_email(&attendees, email)
}

#[cfg(test)]
mod tests {
    use super::{
        attendee_status_for_email, attendee_status_from_json, Attendee, CalendarEvent,
        RecurrenceKind,
    };
    use crate::error::{CalendarMutationBlockReason, Error};

    fn minimal_event_json() -> serde_json::Value {
        serde_json::json!({
            "id": "event-2",
            "account_id": "account-1",
            "calendar_id": "calendar-1",
            "title": "Minimal",
            "start_time": "2026-08-22",
            "end_time": "2026-08-23",
            "all_day": true,
        })
    }

    #[test]
    fn calendar_event_json_contract_is_stable() {
        let event = CalendarEvent {
            id: "event-1".into(),
            account_id: "account-1".into(),
            calendar_id: "calendar-1".into(),
            uid: Some("uid-1".into()),
            title: "Planning".into(),
            description: Some("Quarterly planning".into()),
            location: Some("Room 1".into()),
            start_time: "2026-08-21T09:00:00Z".into(),
            end_time: "2026-08-21T10:00:00Z".into(),
            all_day: false,
            timezone: Some("Europe/Stockholm".into()),
            recurrence_rule: Some("FREQ=WEEKLY".into()),
            recurrence_kind: RecurrenceKind::Series,
            organizer_email: Some("owner@example.com".into()),
            attendees_json: Some("[]".into()),
            my_status: Some("accepted".into()),
            source_message_id: Some("message-1".into()),
            ical_data: Some("BEGIN:VCALENDAR".into()),
            remote_id: Some("remote-1".into()),
            etag: Some("etag-1".into()),
        };
        let expected = serde_json::json!({
            "id": "event-1",
            "account_id": "account-1",
            "calendar_id": "calendar-1",
            "uid": "uid-1",
            "title": "Planning",
            "description": "Quarterly planning",
            "location": "Room 1",
            "start_time": "2026-08-21T09:00:00Z",
            "end_time": "2026-08-21T10:00:00Z",
            "all_day": false,
            "timezone": "Europe/Stockholm",
            "recurrence_rule": "FREQ=WEEKLY",
            "recurrence_kind": "series",
            "organizer_email": "owner@example.com",
            "attendees_json": "[]",
            "my_status": "accepted",
            "source_message_id": "message-1",
            "ical_data": "BEGIN:VCALENDAR",
            "remote_id": "remote-1",
            "etag": "etag-1",
        });

        assert_eq!(serde_json::to_value(&event).unwrap(), expected);
        let decoded: CalendarEvent = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);

        let minimal: CalendarEvent = serde_json::from_value(minimal_event_json()).unwrap();
        assert_eq!(
            serde_json::to_value(minimal).unwrap(),
            serde_json::json!({
                "id": "event-2",
                "account_id": "account-1",
                "calendar_id": "calendar-1",
                "uid": null,
                "title": "Minimal",
                "description": null,
                "location": null,
                "start_time": "2026-08-22",
                "end_time": "2026-08-23",
                "all_day": true,
                "timezone": null,
                "recurrence_rule": null,
                "recurrence_kind": "unknown",
                "organizer_email": null,
                "attendees_json": null,
                "my_status": null,
                "source_message_id": null,
                "ical_data": null,
                "remote_id": null,
                "etag": null,
            })
        );
    }

    #[test]
    fn recurrence_kind_json_contract_is_lowercase_and_roundtrips() {
        assert_eq!(RecurrenceKind::default(), RecurrenceKind::Unknown);
        for (kind, name) in [
            (RecurrenceKind::Unknown, "unknown"),
            (RecurrenceKind::Standalone, "standalone"),
            (RecurrenceKind::Series, "series"),
            (RecurrenceKind::Occurrence, "occurrence"),
        ] {
            assert_eq!(kind.as_str(), name);
            assert_eq!(RecurrenceKind::from_stored(name), kind);
            let mut json = minimal_event_json();
            json["recurrence_kind"] = serde_json::json!(name);
            let event: CalendarEvent = serde_json::from_value(json).unwrap();
            assert_eq!(event.recurrence_kind, kind);
            assert_eq!(
                serde_json::to_value(event).unwrap()["recurrence_kind"],
                name
            );
        }
    }

    #[test]
    fn calendar_event_missing_or_unrecognized_recurrence_fails_closed() {
        let legacy: CalendarEvent = serde_json::from_value(minimal_event_json()).unwrap();
        assert_eq!(legacy.recurrence_kind, RecurrenceKind::Unknown);
        assert!(matches!(
            legacy.ensure_mutable(),
            Err(Error::CalendarMutationBlocked(
                CalendarMutationBlockReason::UnknownRecurrence
            ))
        ));

        let mut ruled_json = minimal_event_json();
        ruled_json["recurrence_rule"] = serde_json::json!("FREQ=WEEKLY");
        let ruled_legacy: CalendarEvent = serde_json::from_value(ruled_json).unwrap();
        assert_eq!(ruled_legacy.recurrence_kind, RecurrenceKind::Unknown);
        assert!(matches!(
            ruled_legacy.ensure_mutable(),
            Err(Error::CalendarMutationBlocked(
                CalendarMutationBlockReason::Recurring
            ))
        ));

        for value in ["future-kind", "Standalone", "", " standalone "] {
            let mut json = minimal_event_json();
            json["recurrence_kind"] = serde_json::json!(value);
            let event: CalendarEvent = serde_json::from_value(json).unwrap();
            assert_eq!(event.recurrence_kind, RecurrenceKind::Unknown);
            assert_eq!(RecurrenceKind::from_stored(value), RecurrenceKind::Unknown);
            assert!(event.ensure_mutable().is_err());
        }
        for value in [serde_json::Value::Null, serde_json::json!(42)] {
            let mut json = minimal_event_json();
            json["recurrence_kind"] = value;
            assert!(serde_json::from_value::<CalendarEvent>(json).is_err());
        }
    }

    #[test]
    fn recurrence_kind_from_rule_classifies_known_local_creation() {
        for rule in [None, Some("")] {
            assert_eq!(RecurrenceKind::from_rule(rule), RecurrenceKind::Standalone);
        }
        for rule in ["FREQ=WEEKLY", " ", "unrecognized-rule"] {
            assert_eq!(
                RecurrenceKind::from_rule(Some(rule)),
                RecurrenceKind::Series
            );
        }
    }

    #[test]
    fn calendar_event_mutation_policy_requires_uncontradicted_standalone() {
        use CalendarMutationBlockReason::{Recurring, UnknownRecurrence};
        use RecurrenceKind::{Occurrence, Series, Standalone, Unknown};

        for (kind, rule, expected) in [
            (Standalone, None, None),
            (Standalone, Some(""), None),
            (Standalone, Some("FREQ=WEEKLY"), Some(Recurring)),
            (Standalone, Some(" "), Some(Recurring)),
            (Series, None, Some(Recurring)),
            (Series, Some(""), Some(Recurring)),
            (Series, Some("FREQ=WEEKLY"), Some(Recurring)),
            (Series, Some(" "), Some(Recurring)),
            (Occurrence, None, Some(Recurring)),
            (Occurrence, Some(""), Some(Recurring)),
            (Occurrence, Some("FREQ=WEEKLY"), Some(Recurring)),
            (Occurrence, Some(" "), Some(Recurring)),
            (Unknown, None, Some(UnknownRecurrence)),
            (Unknown, Some(""), Some(UnknownRecurrence)),
            (Unknown, Some("FREQ=WEEKLY"), Some(Recurring)),
            (Unknown, Some(" "), Some(Recurring)),
        ] {
            let mut event: CalendarEvent = serde_json::from_value(minimal_event_json()).unwrap();
            event.recurrence_kind = kind;
            event.recurrence_rule = rule.map(str::to_string);

            match (event.ensure_mutable(), expected) {
                (Ok(()), None) => {}
                (Err(Error::CalendarMutationBlocked(actual)), Some(expected)) => {
                    assert_eq!(actual, expected, "kind={kind:?}, rule={rule:?}");
                }
                (actual, expected) => {
                    panic!("kind={kind:?}, rule={rule:?}: {actual:?}, expected {expected:?}");
                }
            }
        }
    }

    #[test]
    fn attendee_json_contract_is_stable() {
        let attendee = Attendee {
            email: "guest@example.com".into(),
            name: Some("Guest".into()),
            status: "tentative".into(),
            is_self: None,
        };
        let expected = serde_json::json!({
            "email": "guest@example.com",
            "name": "Guest",
            "status": "tentative",
        });

        assert_eq!(serde_json::to_value(&attendee).unwrap(), expected);
        let decoded: Attendee = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);

        let unnamed: Attendee = serde_json::from_value(serde_json::json!({
            "email": "guest@example.com",
            "status": "needs-action",
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(unnamed).unwrap(),
            serde_json::json!({
                "email": "guest@example.com",
                "name": null,
                "status": "needs-action",
            })
        );
    }

    #[test]
    fn attendee_status_lookup_is_case_insensitive() {
        let attendees = vec![Attendee {
            email: "Me@Example.com".into(),
            name: None,
            status: "accepted".into(),
            is_self: None,
        }];

        assert_eq!(
            attendee_status_for_email(&attendees, "me@example.com"),
            Some("accepted".into())
        );
        assert_eq!(
            attendee_status_from_json(
                Some(r#"[{"email":"ME@example.com","name":null,"status":"tentative"}]"#),
                "me@example.com",
            ),
            Some("tentative".into())
        );
        assert_eq!(
            attendee_status_from_json(Some("invalid"), "me@example.com"),
            None
        );
    }

    #[test]
    fn attendee_status_prefers_provider_self_marker_for_aliases() {
        let attendees = vec![
            Attendee {
                email: "me@example.com".into(),
                name: None,
                status: "accepted".into(),
                is_self: Some(false),
            },
            Attendee {
                email: "alias@example.com".into(),
                name: None,
                status: "declined".into(),
                is_self: Some(true),
            },
        ];

        assert_eq!(
            attendee_status_for_email(&attendees, "me@example.com"),
            Some("declined".into())
        );
        let encoded = serde_json::to_value(&attendees[1]).unwrap();
        assert_eq!(encoded["is_self"], true);
    }
}
