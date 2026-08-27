use serde::{Deserialize, Serialize};

pub mod ical;
pub mod recurrence;
pub mod timezone;

/// Provider-neutral calendar event shared by persistence and backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub organizer_email: Option<String>,
    pub attendees_json: Option<String>,
    pub my_status: Option<String>,
    pub source_message_id: Option<String>,
    pub ical_data: Option<String>,
    pub remote_id: Option<String>,
    pub etag: Option<String>,
}

/// Attendee serialized inside [`CalendarEvent::attendees_json`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attendee {
    pub email: String,
    pub name: Option<String>,
    /// `accepted`, `tentative`, `declined`, or `needs-action`.
    pub status: String,
}

/// Return the RSVP status for `email` from a provider attendee list.
pub(crate) fn attendee_status_for_email(attendees: &[Attendee], email: &str) -> Option<String> {
    attendees
        .iter()
        .find(|attendee| attendee.email.eq_ignore_ascii_case(email))
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
    use super::{attendee_status_for_email, attendee_status_from_json, Attendee, CalendarEvent};

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

        let minimal: CalendarEvent = serde_json::from_value(serde_json::json!({
            "id": "event-2",
            "account_id": "account-1",
            "calendar_id": "calendar-1",
            "title": "Minimal",
            "start_time": "2026-08-22",
            "end_time": "2026-08-23",
            "all_day": true,
        }))
        .unwrap();
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
    fn attendee_json_contract_is_stable() {
        let attendee = Attendee {
            email: "guest@example.com".into(),
            name: Some("Guest".into()),
            status: "tentative".into(),
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
}
