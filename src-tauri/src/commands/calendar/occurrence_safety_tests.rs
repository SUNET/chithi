//! Command-level recurrence guards, ownership preservation, and move races.

use std::cell::{Cell, RefCell};

use rusqlite::{params, types::Value, Connection};

use super::{
    checked_delivery_snapshot, checked_invitation_target, checked_mutation_target,
    create_event_inner, delete_event_inner, move_event_to_calendar_inner,
    notify_calendar_event_inner, prepare_invitation_transport, send_invites_inner,
    update_event_inner, InvitationPurpose, MeetBindingInput, NewEventInput, UpdateEventInput,
};
use crate::calendar::{Attendee, CalendarEvent, RecurrenceKind};
use crate::db;
use crate::error::{CalendarMutationBlockReason, Error};
use crate::state::AppState;

struct Fixture {
    state: AppState,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(directory.path().to_path_buf()).unwrap();
        {
            let conn = state.db.writer().await;
            for account_id in ["account-a", "account-b"] {
                conn.execute(
                    "INSERT INTO accounts (id, display_name, email, username)
                     VALUES (?1, ?1, ?2, ?2)",
                    params![account_id, format!("{account_id}@example.test")],
                )
                .unwrap();
                // No calendar, mail, or meet provider can be selected by these
                // identities, even if a command accidentally passes its guard.
                assert!(db::service_bindings::list_for_account(&conn, account_id)
                    .unwrap()
                    .is_empty());
            }
            for (calendar_id, account_id) in [
                ("source", "account-a"),
                ("same-account", "account-a"),
                ("cross-account", "account-b"),
            ] {
                db::calendar::insert_calendar(
                    &conn,
                    calendar_id,
                    &db::calendar::NewCalendar {
                        account_id: account_id.into(),
                        name: calendar_id.into(),
                        color: "#4285f4".into(),
                        is_default: calendar_id == "source",
                    },
                )
                .unwrap();
            }
        }
        Self {
            state,
            _directory: directory,
        }
    }

    async fn insert(&self, event: &CalendarEvent) {
        let conn = self.state.db.writer().await;
        db::calendar::insert_event(&conn, event).unwrap();
        conn.execute(
            "UPDATE calendar_events SET
                 pending_rsvp_status = 'tentative',
                 manually_managed_at = '2026-09-01T08:00:00Z',
                 created_at = '2026-09-01T07:00:00Z',
                 updated_at = '2026-09-01T08:00:00Z'
             WHERE id = ?1",
            params![event.id],
        )
        .unwrap();
    }

    async fn attach_meeting_and_pending(&self, event: &CalendarEvent) -> MeetBindingInput {
        let conn = self.state.db.writer().await;
        db::meet_meetings::upsert(
            &conn,
            &db::meet_meetings::MeetMeeting {
                event_id: event.id.clone(),
                account_id: "account-a".into(),
                protocol: "zoom".into(),
                meeting_id: format!("owned-{}", event.id),
                join_url: format!("https://example.test/owned-{}", event.id),
            },
        )
        .unwrap();
        let binding = MeetBindingInput {
            lifecycle_id: uuid::Uuid::new_v4().to_string(),
            account_id: "account-a".into(),
            protocol: "zoom".into(),
            meeting_id: format!("pending-{}", event.id),
            join_url: format!("https://example.test/pending-{}", event.id),
        };
        db::meet_pending_meetings::insert(
            &conn,
            &db::meet_pending_meetings::PendingMeeting {
                lifecycle_id: binding.lifecycle_id.clone(),
                account_id: binding.account_id.clone(),
                protocol: binding.protocol.clone(),
                meeting_id: binding.meeting_id.clone(),
                join_url: binding.join_url.clone(),
                created_at: "2026-09-01T09:00:00Z".into(),
                cleanup_requested: false,
            },
        )
        .unwrap();
        binding
    }

    async fn orphan(&self, event: &CalendarEvent) -> CalendarEvent {
        let conn = self.state.db.writer().await;
        // A deliberately inconsistent cached account reference makes an
        // account lookup fail before any credential or transport access.
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute(
            "UPDATE calendar_events SET account_id = 'missing-account' WHERE id = ?1",
            params![event.id],
        )
        .unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        db::calendar::get_event(&conn, &event.id).unwrap()
    }

    fn event(&self, id: &str) -> CalendarEvent {
        db::calendar::get_event(&self.state.db.reader(), id).unwrap()
    }

    fn snapshot(&self) -> StoredRows {
        StoredRows::read(&self.state.db.reader())
    }
}

/// Include columns outside CalendarEvent (timestamps, RSVP state), and every
/// row, so a rejected copy or ownership transfer cannot hide behind row counts.
#[derive(Debug, PartialEq)]
struct StoredRows {
    events: Vec<Vec<Value>>,
    calendars: Vec<Vec<Value>>,
    meetings: Vec<Vec<Value>>,
    pending: Vec<Vec<Value>>,
}

impl StoredRows {
    fn read(conn: &Connection) -> Self {
        fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
            let mut statement = conn.prepare(sql).unwrap();
            let columns = statement.column_count();
            statement
                .query_map([], |row| (0..columns).map(|index| row.get(index)).collect())
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        }
        Self {
            events: rows(conn, "SELECT * FROM calendar_events ORDER BY id"),
            calendars: rows(conn, "SELECT * FROM calendars ORDER BY id"),
            meetings: rows(conn, "SELECT * FROM meet_meetings ORDER BY event_id"),
            pending: rows(
                conn,
                "SELECT * FROM meet_pending_meetings ORDER BY lifecycle_id",
            ),
        }
    }
}

fn attendee(email: &str) -> Attendee {
    Attendee {
        email: email.into(),
        name: Some("Original attendee".into()),
        status: "accepted".into(),
        is_self: Some(false),
    }
}

fn stored_event(id: &str, kind: RecurrenceKind, rule: Option<&str>) -> CalendarEvent {
    CalendarEvent {
        id: id.into(),
        account_id: "account-a".into(),
        calendar_id: "source".into(),
        uid: Some(format!("{id}@example.test")),
        title: "Original title".into(),
        description: Some("Original description".into()),
        location: Some("Original room".into()),
        start_time: "2026-09-14T09:00:00Z".into(),
        end_time: "2026-09-14T10:00:00Z".into(),
        all_day: false,
        timezone: Some("Europe/Stockholm".into()),
        recurrence_rule: rule.map(str::to_owned),
        recurrence_kind: kind,
        organizer_email: Some("organizer@example.test".into()),
        attendees_json: Some(serde_json::to_string(&vec![attendee("old@example.test")]).unwrap()),
        my_status: Some("accepted".into()),
        source_message_id: Some("original-message".into()),
        ical_data: Some("BEGIN:VCALENDAR\r\nX-PROVIDER-METADATA:original\r\nEND:VCALENDAR".into()),
        remote_id: Some(format!("remote-{id}")),
        etag: Some("original-etag".into()),
    }
}

fn blocked_events() -> Vec<(CalendarEvent, CalendarMutationBlockReason)> {
    use CalendarMutationBlockReason::{Recurring, UnknownRecurrence};
    use RecurrenceKind::{Occurrence, Series, Standalone, Unknown};
    [
        ("series", Series, Some("FREQ=WEEKLY"), Recurring),
        ("series-without-rule", Series, None, Recurring),
        ("occurrence", Occurrence, None, Recurring),
        ("unknown", Unknown, None, UnknownRecurrence),
        ("unknown-with-rule", Unknown, Some("FREQ=DAILY"), Recurring),
        (
            "forged-standalone",
            Standalone,
            Some("FREQ=DAILY"),
            Recurring,
        ),
    ]
    .into_iter()
    .map(|(id, kind, rule, reason)| (stored_event(id, kind, rule), reason))
    .collect()
}

fn assert_blocked(error: Error, reason: CalendarMutationBlockReason) {
    assert!(
        matches!(&error, Error::CalendarMutationBlocked(actual) if *actual == reason),
        "expected {reason:?}, got {error:?}"
    );
}

fn assert_stale(error: Error) {
    assert!(
        matches!(&error, Error::Other(message) if message.contains("changed during the move")),
        "expected stale-source rejection, got {error:?}"
    );
}

fn new_event(account: &str, calendar: &str, rule: Option<&str>) -> NewEventInput {
    NewEventInput {
        account_id: account.into(),
        calendar_id: calendar.into(),
        title: "Locally created".into(),
        description: Some("Local description".into()),
        location: Some("Local room".into()),
        start_time: "2026-09-15T11:00:00Z".into(),
        end_time: "2026-09-15T12:00:00Z".into(),
        all_day: false,
        timezone: Some("Europe/Stockholm".into()),
        recurrence_rule: rule.map(str::to_owned),
        attendees: vec![],
        meet_binding: None,
    }
}

#[tokio::test]
async fn update_rejects_nonstandalone_patches_without_changing_rows_or_meetings() {
    let fixture = Fixture::new().await;
    for (event, reason) in blocked_events() {
        fixture.insert(&event).await;
        let binding = fixture.attach_meeting_and_pending(&event).await;
        let mut forged = binding.clone();
        forged.meeting_id = "forged-meeting".into();
        let mut invalid_lifecycle = binding;
        invalid_lifecycle.lifecycle_id = "not-a-uuid".into();
        let patches = [
            (
                "title",
                UpdateEventInput {
                    title: Some("Changed".into()),
                    ..Default::default()
                },
            ),
            (
                "times",
                UpdateEventInput {
                    start_time: Some("2026-09-16T12:00:00Z".into()),
                    end_time: Some("2026-09-16T13:30:00Z".into()),
                    ..Default::default()
                },
            ),
            (
                "calendar",
                UpdateEventInput {
                    calendar_id: Some("same-account".into()),
                    ..Default::default()
                },
            ),
            (
                "attendees",
                UpdateEventInput {
                    attendees: Some(vec![attendee("new@example.test")]),
                    ..Default::default()
                },
            ),
            (
                "clear-rule",
                UpdateEventInput {
                    recurrence_rule: Some(String::new()),
                    ..Default::default()
                },
            ),
            (
                "forged-meeting",
                UpdateEventInput {
                    title: Some("Changed with forged meeting".into()),
                    meet_binding: Some(forged),
                    ..Default::default()
                },
            ),
            (
                "invalid-lifecycle",
                UpdateEventInput {
                    meet_binding: Some(invalid_lifecycle),
                    ..Default::default()
                },
            ),
        ];
        let before = fixture.snapshot();
        for (patch_name, patch) in patches {
            let error = update_event_inner(&fixture.state, event.id.clone(), patch)
                .await
                .unwrap_err();
            assert_blocked(error, reason);
            assert_eq!(fixture.snapshot(), before, "{}: {patch_name}", event.id);
        }
    }
}

#[tokio::test]
async fn delete_rejects_nonstandalone_without_queueing_meeting_cleanup() {
    let fixture = Fixture::new().await;
    for (event, reason) in blocked_events() {
        fixture.insert(&event).await;
        fixture.attach_meeting_and_pending(&event).await;
        let before = fixture.snapshot();

        let error = delete_event_inner(&fixture.state, event.id.clone(), None)
            .await
            .unwrap_err();

        assert_blocked(error, reason);
        assert_eq!(fixture.snapshot(), before, "{}", event.id);
        assert!(
            db::meet_pending_meetings::list_cleanup_requested(&fixture.state.db.reader())
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn move_rejects_nonstandalone_before_same_or_cross_account_writes() {
    let fixture = Fixture::new().await;
    for (event, reason) in blocked_events() {
        fixture.insert(&event).await;
        fixture.attach_meeting_and_pending(&event).await;
        let before = fixture.snapshot();
        for (calendar, account) in [
            ("source", "account-a"),
            ("same-account", "account-a"),
            ("cross-account", "account-b"),
        ] {
            let error = move_event_to_calendar_inner(
                &fixture.state,
                event.id.clone(),
                calendar.into(),
                account.into(),
            )
            .await
            .unwrap_err();

            assert_blocked(error, reason);
            assert_eq!(fixture.snapshot(), before, "{} -> {calendar}", event.id);
        }
    }
}

#[tokio::test]
async fn mutation_guards_precede_missing_source_account_resolution() {
    let fixture = Fixture::new().await;
    for (event, reason) in blocked_events() {
        fixture.insert(&event).await;
        fixture.attach_meeting_and_pending(&event).await;
        let orphan = fixture.orphan(&event).await;
        assert!(matches!(
            db::accounts::get_account_full(&fixture.state.db.reader(), &orphan.account_id),
            Err(Error::AccountNotFound(_))
        ));
        let before = fixture.snapshot();

        assert_blocked(
            update_event_inner(
                &fixture.state,
                orphan.id.clone(),
                UpdateEventInput {
                    title: Some("Changed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err(),
            reason,
        );
        assert_eq!(fixture.snapshot(), before);
        assert_blocked(
            delete_event_inner(&fixture.state, orphan.id.clone(), None)
                .await
                .unwrap_err(),
            reason,
        );
        assert_eq!(fixture.snapshot(), before);
        assert_blocked(
            move_event_to_calendar_inner(
                &fixture.state,
                orphan.id,
                "cross-account".into(),
                "account-b".into(),
            )
            .await
            .unwrap_err(),
            reason,
        );
        assert_eq!(fixture.snapshot(), before);
    }
}

#[tokio::test]
async fn send_invites_rejects_unsafe_targets_before_account_lookup_or_attendee_mutation() {
    let fixture = Fixture::new().await;
    for (event, reason) in blocked_events() {
        if event.recurrence_kind == RecurrenceKind::Series {
            continue;
        }
        fixture.insert(&event).await;
        fixture.attach_meeting_and_pending(&event).await;
        let orphan = fixture.orphan(&event).await;
        let before = fixture.snapshot();

        let error = send_invites_inner(
            &fixture.state,
            orphan.account_id,
            orphan.id,
            vec!["new@example.test".into()],
        )
        .await
        .unwrap_err();

        assert_blocked(error, reason);
        assert_eq!(fixture.snapshot(), before, "{}", event.id);
    }
}

#[tokio::test]
async fn send_invites_rejects_another_accounts_event_without_writes() {
    let fixture = Fixture::new().await;
    for kind in [RecurrenceKind::Standalone, RecurrenceKind::Series] {
        let event = stored_event(kind.as_str(), kind, None);
        fixture.insert(&event).await;
        let before = fixture.snapshot();

        let error = send_invites_inner(
            &fixture.state,
            "account-b".into(),
            event.id,
            vec!["new@example.test".into()],
        )
        .await
        .unwrap_err();

        assert!(matches!(error, Error::Other(message) if message.contains("another account")));
        assert_eq!(fixture.snapshot(), before);
    }
}

#[tokio::test]
async fn local_creation_derives_standalone_or_series_kind_from_input_rule() {
    let fixture = Fixture::new().await;
    for (rule, kind) in [
        (None, RecurrenceKind::Standalone),
        (Some(""), RecurrenceKind::Standalone),
        (Some("FREQ=WEEKLY;COUNT=3"), RecurrenceKind::Series),
    ] {
        let id = create_event_inner(&fixture.state, new_event("account-a", "source", rule), None)
            .await
            .unwrap();
        let event = fixture.event(&id);

        assert_eq!(event.recurrence_kind, kind);
        assert_eq!(event.recurrence_rule.as_deref(), rule);
        assert_eq!(
            event.organizer_email.as_deref(),
            Some("account-a@example.test")
        );
        assert_eq!(event.calendar_id, "source");
        assert!(event.remote_id.is_none());
    }
    assert_eq!(fixture.snapshot().events.len(), 3);
}

#[tokio::test]
async fn known_series_creation_invitation_is_allowed_while_editing_stays_blocked() {
    let fixture = Fixture::new().await;
    let id = create_event_inner(
        &fixture.state,
        new_event("account-a", "source", Some("FREQ=WEEKLY")),
        None,
    )
    .await
    .unwrap();
    let event = fixture.event(&id);
    assert_eq!(
        checked_invitation_target(
            &fixture.state.db.reader(),
            "account-a",
            &id,
            super::InvitationPurpose::Creation
        )
        .unwrap(),
        event,
    );
    // Empty recipients exercise command acceptance without any mail transport.
    send_invites_inner(&fixture.state, "account-a".into(), id.clone(), vec![])
        .await
        .unwrap();
    let before = fixture.snapshot();
    assert_blocked(
        update_event_inner(
            &fixture.state,
            id,
            UpdateEventInput {
                title: Some("Not a supported edit".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err(),
        CalendarMutationBlockReason::Recurring,
    );
    assert_eq!(fixture.snapshot(), before);
    assert_eq!(
        fixture.event(&event.id).recurrence_rule,
        event.recurrence_rule
    );
}

#[tokio::test]
async fn standalone_edit_updates_supported_fields_and_preserves_provider_metadata() {
    let fixture = Fixture::new().await;
    let event = stored_event("standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&event).await;
    fixture.attach_meeting_and_pending(&event).await;
    let before = fixture.snapshot();
    let mut expected = event.clone();
    expected.title = "Edited title".into();
    expected.description = Some("Edited description".into());
    expected.location = Some("Edited room".into());
    expected.start_time = "2026-09-16T11:00:00Z".into();
    expected.end_time = "2026-09-16T12:30:00Z".into();
    expected.timezone = Some("UTC".into());
    expected.calendar_id = "same-account".into();
    let attendees = vec![attendee("new@example.test")];
    expected.attendees_json = Some(serde_json::to_string(&attendees).unwrap());

    update_event_inner(
        &fixture.state,
        event.id.clone(),
        UpdateEventInput {
            calendar_id: Some(expected.calendar_id.clone()),
            title: Some(expected.title.clone()),
            description: expected.description.clone(),
            location: expected.location.clone(),
            start_time: Some(expected.start_time.clone()),
            end_time: Some(expected.end_time.clone()),
            timezone: expected.timezone.clone(),
            attendees: Some(attendees),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(fixture.event(&event.id), expected);
    let after = fixture.snapshot();
    assert_eq!(after.events.len(), 1);
    assert_eq!(after.meetings, before.meetings);
    assert_eq!(after.pending, before.pending);
    assert_eq!(after.calendars, before.calendars);
}

#[tokio::test]
async fn standalone_edit_cannot_introduce_a_recurrence_rule() {
    let fixture = Fixture::new().await;
    let event = stored_event("standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&event).await;
    fixture.attach_meeting_and_pending(&event).await;
    let before = fixture.snapshot();

    let error = update_event_inner(
        &fixture.state,
        event.id,
        UpdateEventInput {
            title: Some("Attempted series conversion".into()),
            recurrence_rule: Some("FREQ=DAILY".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();

    assert_blocked(error, CalendarMutationBlockReason::Recurring);
    assert_eq!(fixture.snapshot(), before);
}

#[tokio::test]
async fn standalone_delete_removes_event_and_queues_only_its_owned_meeting() {
    let fixture = Fixture::new().await;
    let event = stored_event("standalone", RecurrenceKind::Standalone, None);
    let other = stored_event("other", RecurrenceKind::Standalone, None);
    fixture.insert(&event).await;
    fixture.insert(&other).await;
    let pending = fixture.attach_meeting_and_pending(&event).await;

    delete_event_inner(&fixture.state, event.id.clone(), None)
        .await
        .unwrap();

    let conn = fixture.state.db.reader();
    assert!(db::calendar::get_event(&conn, &event.id).is_err());
    assert_eq!(db::calendar::get_event(&conn, &other.id).unwrap(), other);
    assert!(db::meet_meetings::get(&conn, &event.id).unwrap().is_none());
    assert!(
        !db::meet_pending_meetings::get(&conn, &pending.lifecycle_id)
            .unwrap()
            .unwrap()
            .cleanup_requested
    );
    let cleanup = db::meet_pending_meetings::list_cleanup_requested(&conn).unwrap();
    assert_eq!(cleanup.len(), 1);
    assert_eq!(cleanup[0].meeting_id, "owned-standalone");
    assert_eq!(cleanup[0].account_id, "account-a");
    assert_eq!(cleanup[0].protocol, "zoom");
    assert_eq!(cleanup[0].join_url, "https://example.test/owned-standalone");
}

#[tokio::test]
async fn standalone_same_account_move_preserves_identity_metadata_and_meeting_ownership() {
    let fixture = Fixture::new().await;
    let mut event = stored_event("standalone", RecurrenceKind::Standalone, Some(""));
    fixture.insert(&event).await;
    fixture.attach_meeting_and_pending(&event).await;
    let before = fixture.snapshot();

    let id = move_event_to_calendar_inner(
        &fixture.state,
        event.id.clone(),
        "same-account".into(),
        "account-a".into(),
    )
    .await
    .unwrap();

    assert_eq!(id, event.id);
    event.calendar_id = "same-account".into();
    assert_eq!(fixture.event(&id), event);
    let after = fixture.snapshot();
    assert_eq!(after.events.len(), 1);
    assert_eq!(after.meetings, before.meetings);
    assert_eq!(after.pending, before.pending);
    assert_eq!(after.calendars, before.calendars);
}

#[tokio::test]
async fn standalone_move_to_current_calendar_is_a_write_free_noop() {
    let fixture = Fixture::new().await;
    let event = stored_event("standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&event).await;
    let before = fixture.snapshot();

    let id = move_event_to_calendar_inner(
        &fixture.state,
        event.id.clone(),
        "source".into(),
        "account-a".into(),
    )
    .await
    .unwrap();

    assert_eq!(id, event.id);
    assert_eq!(fixture.snapshot(), before);
}

#[tokio::test]
async fn standalone_cross_account_move_copies_authoritative_content_then_deletes_source() {
    let fixture = Fixture::new().await;
    let source = stored_event("standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&source).await;

    let id = move_event_to_calendar_inner(
        &fixture.state,
        source.id.clone(),
        "cross-account".into(),
        "account-b".into(),
    )
    .await
    .unwrap();

    let copied = fixture.event(&id);
    assert_ne!(id, source.id);
    assert_eq!(copied.account_id, "account-b");
    assert_eq!(copied.calendar_id, "cross-account");
    assert_eq!(copied.title, source.title);
    assert_eq!(copied.description, source.description);
    assert_eq!(copied.location, source.location);
    assert_eq!(copied.start_time, source.start_time);
    assert_eq!(copied.end_time, source.end_time);
    assert_eq!(copied.all_day, source.all_day);
    assert_eq!(copied.timezone, source.timezone);
    assert_eq!(copied.attendees_json, source.attendees_json);
    assert_eq!(copied.recurrence_kind, RecurrenceKind::Standalone);
    assert!(copied.recurrence_rule.is_none());
    assert_eq!(
        copied.organizer_email.as_deref(),
        Some("account-b@example.test")
    );
    assert!(copied.uid.is_some());
    assert_ne!(copied.uid, source.uid);
    assert!(copied.remote_id.is_none());
    assert!(copied.etag.is_none());
    assert!(copied.ical_data.is_none());
    assert!(copied.source_message_id.is_none());
    assert!(db::calendar::get_event(&fixture.state.db.reader(), &source.id).is_err());
    assert_eq!(fixture.snapshot().events.len(), 1);
}

#[tokio::test]
async fn move_rejects_missing_or_mismatched_target_calendar_and_account_without_writes() {
    let fixture = Fixture::new().await;
    let event = stored_event("standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&event).await;
    fixture.attach_meeting_and_pending(&event).await;
    let before = fixture.snapshot();
    for (calendar, account) in [
        ("cross-account", "account-a"),
        ("same-account", "account-b"),
        ("source", "account-b"),
        ("missing-calendar", "account-a"),
        ("missing-calendar", "account-b"),
        ("cross-account", "missing-account"),
    ] {
        let error = move_event_to_calendar_inner(
            &fixture.state,
            event.id.clone(),
            calendar.into(),
            account.into(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, Error::Other(_)), "{calendar}: {error:?}");
        assert_eq!(fixture.snapshot(), before, "{calendar} / {account}");
    }
}

#[tokio::test]
async fn update_rejects_missing_or_cross_account_calendar_without_writes() {
    let fixture = Fixture::new().await;
    let event = stored_event("standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&event).await;
    fixture.attach_meeting_and_pending(&event).await;
    let before = fixture.snapshot();
    for calendar in ["missing-calendar", "cross-account"] {
        update_event_inner(
            &fixture.state,
            event.id.clone(),
            UpdateEventInput {
                calendar_id: Some(calendar.into()),
                title: Some("Changed".into()),
                attendees: Some(vec![]),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(fixture.snapshot(), before, "{calendar}");
    }
}

fn refreshed_versions(source: &CalendarEvent) -> Vec<CalendarEvent> {
    let mut title = source.clone();
    title.title = "Newer provider content".into();
    let mut remote_id = source.clone();
    remote_id.remote_id = Some("newer-remote-id".into());
    let mut etag = source.clone();
    etag.etag = Some("newer-etag".into());
    let mut ical_data = source.clone();
    ical_data.ical_data = Some("Newer opaque provider data".into());
    vec![title, remote_id, etag, ical_data]
}

#[tokio::test]
async fn destination_creation_rejects_stale_source_content_and_metadata_without_writes() {
    let fixture = Fixture::new().await;
    let source = stored_event("standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&source).await;
    fixture.attach_meeting_and_pending(&source).await;
    for refreshed in refreshed_versions(&source) {
        {
            let conn = fixture.state.db.writer().await;
            db::calendar::update_event(&conn, &refreshed).unwrap();
        }
        let before = fixture.snapshot();

        let error = create_event_inner(
            &fixture.state,
            new_event("account-b", "cross-account", None),
            Some(&source),
        )
        .await
        .unwrap_err();

        assert_stale(error);
        assert_eq!(fixture.event(&source.id), refreshed);
        assert_eq!(fixture.snapshot(), before);
    }
}

#[tokio::test]
async fn source_deletion_rejects_stale_content_and_metadata_without_cleanup() {
    let fixture = Fixture::new().await;
    let source = stored_event("standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&source).await;
    fixture.attach_meeting_and_pending(&source).await;
    for refreshed in refreshed_versions(&source) {
        {
            let conn = fixture.state.db.writer().await;
            db::calendar::update_event(&conn, &refreshed).unwrap();
        }
        let before = fixture.snapshot();

        let error = delete_event_inner(&fixture.state, source.id.clone(), Some(&source))
            .await
            .unwrap_err();

        assert_stale(error);
        assert_eq!(fixture.event(&source.id), refreshed);
        assert_eq!(fixture.snapshot(), before);
    }
}

#[tokio::test]
async fn move_helpers_reject_new_recurrence_evidence_instead_of_trusting_source_snapshot() {
    let fixture = Fixture::new().await;
    for (refreshed, reason) in blocked_events() {
        let source = stored_event(&refreshed.id, RecurrenceKind::Standalone, None);
        fixture.insert(&source).await;
        fixture.attach_meeting_and_pending(&source).await;
        {
            let conn = fixture.state.db.writer().await;
            db::calendar::update_event(&conn, &refreshed).unwrap();
        }
        let before = fixture.snapshot();

        assert_blocked(
            checked_mutation_target(&fixture.state.db.reader(), &source.id, Some(&source))
                .unwrap_err(),
            reason,
        );
        assert_blocked(
            create_event_inner(
                &fixture.state,
                new_event("account-b", "cross-account", None),
                Some(&source),
            )
            .await
            .unwrap_err(),
            reason,
        );
        assert_eq!(fixture.snapshot(), before);
        assert_blocked(
            delete_event_inner(&fixture.state, source.id.clone(), Some(&source))
                .await
                .unwrap_err(),
            reason,
        );
        assert_eq!(fixture.snapshot(), before);
    }
}

#[tokio::test]
async fn update_rechecks_recurrence_after_waiting_for_meeting_lifecycle_lock() {
    let fixture = Fixture::new().await;
    let source = stored_event("standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&source).await;
    let binding = fixture.attach_meeting_and_pending(&source).await;
    let lock = fixture
        .state
        .meet_lifecycle
        .acquire(&binding.lifecycle_id)
        .unwrap();
    let guard = lock.lock().await;
    let update = update_event_inner(
        &fixture.state,
        source.id.clone(),
        UpdateEventInput {
            title: Some("Stale renderer edit".into()),
            attendees: Some(vec![]),
            meet_binding: Some(binding),
            ..Default::default()
        },
    );
    tokio::pin!(update);
    assert!(futures::poll!(update.as_mut()).is_pending());
    let mut refreshed = source.clone();
    refreshed.recurrence_kind = RecurrenceKind::Occurrence;
    refreshed.title = "Provider refresh while meeting claim waits".into();
    refreshed.etag = Some("newer-etag".into());
    {
        let conn = fixture.state.db.writer().await;
        db::calendar::update_event(&conn, &refreshed).unwrap();
    }
    let before_release = fixture.snapshot();
    drop(guard);

    assert_blocked(
        update.await.unwrap_err(),
        CalendarMutationBlockReason::Recurring,
    );
    assert_eq!(fixture.event(&source.id), refreshed);
    assert_eq!(fixture.snapshot(), before_release);
}

#[tokio::test]
async fn move_rechecks_source_after_waiting_for_destination_write_transaction() {
    let fixture = Fixture::new().await;
    for (calendar, account) in [
        ("same-account", "account-a"),
        ("cross-account", "account-b"),
    ] {
        let source = stored_event(calendar, RecurrenceKind::Standalone, None);
        fixture.insert(&source).await;
        fixture.attach_meeting_and_pending(&source).await;
        let conn = fixture.state.db.writer().await;
        let moving = move_event_to_calendar_inner(
            &fixture.state,
            source.id.clone(),
            calendar.into(),
            account.into(),
        );
        tokio::pin!(moving);
        assert!(futures::poll!(moving.as_mut()).is_pending());
        let mut refreshed = source.clone();
        refreshed.recurrence_kind = RecurrenceKind::Occurrence;
        refreshed.title = "Newly classified occurrence".into();
        db::calendar::update_event(&conn, &refreshed).unwrap();
        let before_release = StoredRows::read(&conn);
        drop(conn);

        assert_blocked(
            moving.await.unwrap_err(),
            CalendarMutationBlockReason::Recurring,
        );
        assert_eq!(fixture.event(&source.id), refreshed);
        assert_eq!(fixture.snapshot(), before_release);
    }
}

#[tokio::test]
async fn cross_account_move_reports_partial_copy_and_preserves_refreshed_source() {
    let fixture = Fixture::new().await;
    let source = stored_event("standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&source).await;
    fixture.attach_meeting_and_pending(&source).await;
    {
        let conn = fixture.state.db.writer().await;
        // Model a refresh at the copy boundary, after the insert's source
        // check but before deletion, without provider calls or timing sleeps.
        conn.execute_batch(
            "CREATE TRIGGER refresh_source_after_copy
             AFTER INSERT ON calendar_events
             WHEN NEW.calendar_id = 'cross-account'
             BEGIN
                 UPDATE calendar_events
                 SET title = 'Refreshed after copy', etag = 'refreshed-after-copy'
                 WHERE id = 'standalone';
             END;",
        )
        .unwrap();
    }
    let before = fixture.snapshot();

    let error = move_event_to_calendar_inner(
        &fixture.state,
        source.id.clone(),
        "cross-account".into(),
        "account-b".into(),
    )
    .await
    .unwrap_err();

    let copied_id: String = fixture
        .state
        .db
        .reader()
        .query_row(
            "SELECT id FROM calendar_events WHERE calendar_id = 'cross-account'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(matches!(
        error,
        Error::Other(message)
            if message.contains(&copied_id)
                && message.contains("source was not removed")
                && message.contains("changed during the move")
    ));
    let mut expected = source.clone();
    expected.title = "Refreshed after copy".into();
    expected.etag = Some("refreshed-after-copy".into());
    assert_eq!(fixture.event(&source.id), expected);
    let copied = fixture.event(&copied_id);
    assert_eq!(copied.title, source.title);
    assert_eq!(copied.account_id, "account-b");
    let after = fixture.snapshot();
    assert_eq!(after.events.len(), 2);
    assert_eq!(after.meetings, before.meetings);
    assert_eq!(after.pending, before.pending);
    assert_eq!(after.calendars, before.calendars);
}

#[tokio::test]
async fn restart_preserves_unknown_classification_and_command_rejections() {
    let fixture = Fixture::new().await;
    let event = stored_event("cached-unknown", RecurrenceKind::Unknown, None);
    fixture.insert(&event).await;
    fixture.attach_meeting_and_pending(&event).await;
    // Match an old caller omitting recurrence_kind: the schema default must
    // remain unknown after startup rather than being inferred from no RRULE.
    {
        let conn = fixture.state.db.writer().await;
        conn.execute(
            "INSERT INTO calendar_events
                 (id, account_id, calendar_id, title, start_time, end_time)
             VALUES ('default-unknown', 'account-a', 'source', 'Legacy cached event',
                     '2026-09-14T09:00:00Z', '2026-09-14T10:00:00Z')",
            [],
        )
        .unwrap();
    }
    let before = fixture.snapshot();
    let Fixture {
        state,
        _directory: directory,
    } = fixture;
    drop(state);
    let restarted = AppState::new(directory.path().to_path_buf()).unwrap();
    assert_eq!(StoredRows::read(&restarted.db.reader()), before);

    for id in ["cached-unknown", "default-unknown"] {
        let restored = db::calendar::get_event(&restarted.db.reader(), id).unwrap();
        assert_eq!(restored.recurrence_kind, RecurrenceKind::Unknown);
        assert!(restored.recurrence_rule.is_none());
        assert_blocked(
            update_event_inner(
                &restarted,
                id.into(),
                UpdateEventInput {
                    title: Some("Changed after restart".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err(),
            CalendarMutationBlockReason::UnknownRecurrence,
        );
        assert_blocked(
            delete_event_inner(&restarted, id.into(), None)
                .await
                .unwrap_err(),
            CalendarMutationBlockReason::UnknownRecurrence,
        );
        assert_blocked(
            move_event_to_calendar_inner(
                &restarted,
                id.into(),
                "cross-account".into(),
                "account-b".into(),
            )
            .await
            .unwrap_err(),
            CalendarMutationBlockReason::UnknownRecurrence,
        );
        assert_eq!(StoredRows::read(&restarted.db.reader()), before);
    }
}

fn delivery_recurrence_refreshes(
    source: &CalendarEvent,
) -> Vec<(CalendarEvent, CalendarMutationBlockReason)> {
    blocked_events()
        .into_iter()
        .map(|(classification, reason)| {
            let mut refreshed = source.clone();
            refreshed.recurrence_kind = classification.recurrence_kind;
            refreshed.recurrence_rule = classification.recurrence_rule;
            (refreshed, reason)
        })
        .collect()
}

fn delivery_content_refreshes(source: &CalendarEvent) -> Vec<CalendarEvent> {
    let mut refreshed = refreshed_versions(source);
    let mut attendees = source.clone();
    attendees.attendees_json =
        Some(serde_json::to_string(&vec![attendee("provider-added@example.test")]).unwrap());
    refreshed.push(attendees);
    let mut time = source.clone();
    time.start_time = "2026-09-17T09:00:00Z".into();
    time.end_time = "2026-09-17T10:00:00Z".into();
    refreshed.push(time);
    let mut uid = source.clone();
    uid.uid = Some("newer-provider-uid".into());
    refreshed.push(uid);
    refreshed
}

fn assert_delivery_refresh_rejected(
    error: Error,
    purpose: InvitationPurpose,
    refreshed: &CalendarEvent,
    reason: Option<CalendarMutationBlockReason>,
) {
    match (purpose, refreshed.recurrence_kind, reason) {
        (InvitationPurpose::Creation, RecurrenceKind::Series, _) | (_, _, None) => {
            assert!(
                matches!(&error, Error::Other(message)
                    if message.contains("changed while preparing the notification")),
                "expected a changed-delivery-snapshot rejection, got {error:?}"
            );
        }
        (_, _, Some(reason)) => assert_blocked(error, reason),
    }
}

/// Suspend the production boundary in its preparation future, then let that
/// future refresh SQLite before returning a simulated transport capability.
async fn reject_refresh_during_transport_preparation(
    fixture: &Fixture,
    expected: &CalendarEvent,
    refreshed: &CalendarEvent,
    purpose: InvitationPurpose,
) -> Error {
    let preparations = Cell::new(0);
    let sends = Cell::new(0);
    let after_refresh = RefCell::new(None);
    let writer = fixture.state.db.writer().await;
    let delivery = async {
        let transport = prepare_invitation_transport(&fixture.state, expected, purpose, async {
            preparations.set(preparations.get() + 1);
            let conn = fixture.state.db.writer().await;
            db::calendar::update_event(&conn, refreshed)?;
            after_refresh.replace(Some(StoredRows::read(&conn)));
            Ok("prepared transport")
        })
        .await?;
        sends.set(sends.get() + 1);
        Ok::<_, Error>(transport)
    };
    tokio::pin!(delivery);
    assert!(futures::poll!(delivery.as_mut()).is_pending());
    assert_eq!(preparations.get(), 1);
    drop(writer);

    let error = delivery.await.unwrap_err();
    assert_eq!(sends.get(), 0);
    assert_eq!(fixture.event(&expected.id), *refreshed);
    assert_eq!(
        &fixture.snapshot(),
        after_refresh.borrow().as_ref().unwrap(),
    );
    error
}

#[tokio::test]
async fn notification_rejects_stale_standalone_view_of_nonstandalone_backend_event() {
    let fixture = Fixture::new().await;
    for (refreshed, reason) in blocked_events() {
        let initial = stored_event(&refreshed.id, RecurrenceKind::Standalone, None);
        fixture.insert(&initial).await;
        fixture.attach_meeting_and_pending(&initial).await;
        let stale_ui_event = fixture.event(&initial.id);
        assert_eq!(stale_ui_event.recurrence_kind, RecurrenceKind::Standalone);
        {
            let conn = fixture.state.db.writer().await;
            db::calendar::update_event(&conn, &refreshed).unwrap();
        }
        let before = fixture.snapshot();

        let error = notify_calendar_event_inner(
            &fixture.state,
            stale_ui_event.account_id,
            stale_ui_event.id,
            vec![],
        )
        .await
        .unwrap_err();

        assert_blocked(error, reason);
        assert_eq!(fixture.event(&refreshed.id), refreshed);
        assert_eq!(fixture.snapshot(), before);
    }
}

#[tokio::test]
async fn transport_preparation_rejects_reclassification_without_returning_transport_or_sending() {
    for purpose in [
        InvitationPurpose::Creation,
        InvitationPurpose::MutationNotification,
    ] {
        let fixture = Fixture::new().await;
        let source = stored_event("preparing", RecurrenceKind::Standalone, None);
        fixture.insert(&source).await;
        fixture.attach_meeting_and_pending(&source).await;
        for (refreshed, reason) in delivery_recurrence_refreshes(&source) {
            {
                let conn = fixture.state.db.writer().await;
                db::calendar::update_event(&conn, &source).unwrap();
            }
            let error =
                reject_refresh_during_transport_preparation(&fixture, &source, &refreshed, purpose)
                    .await;

            assert_delivery_refresh_rejected(error, purpose, &refreshed, Some(reason));
        }
    }
}

#[tokio::test]
async fn transport_preparation_rejects_same_kind_content_and_metadata_changes_without_sending() {
    for purpose in [
        InvitationPurpose::Creation,
        InvitationPurpose::MutationNotification,
    ] {
        let fixture = Fixture::new().await;
        let source = stored_event("preparing", RecurrenceKind::Standalone, None);
        fixture.insert(&source).await;
        fixture.attach_meeting_and_pending(&source).await;
        for refreshed in delivery_content_refreshes(&source) {
            {
                let conn = fixture.state.db.writer().await;
                db::calendar::update_event(&conn, &source).unwrap();
            }
            let error =
                reject_refresh_during_transport_preparation(&fixture, &source, &refreshed, purpose)
                    .await;

            assert_delivery_refresh_rejected(error, purpose, &refreshed, None);
        }
    }
}

#[tokio::test]
async fn recipient_boundary_rechecks_before_polling_the_next_transport_preparation() {
    for purpose in [
        InvitationPurpose::Creation,
        InvitationPurpose::MutationNotification,
    ] {
        let fixture = Fixture::new().await;
        let source = stored_event("between-recipients", RecurrenceKind::Standalone, None);
        fixture.insert(&source).await;
        fixture.attach_meeting_and_pending(&source).await;
        let refreshes = delivery_recurrence_refreshes(&source)
            .into_iter()
            .map(|(event, reason)| (event, Some(reason)))
            .chain(
                delivery_content_refreshes(&source)
                    .into_iter()
                    .map(|event| (event, None)),
            );
        for (refreshed, reason) in refreshes {
            {
                let conn = fixture.state.db.writer().await;
                db::calendar::update_event(&conn, &source).unwrap();
            }
            let preparations = Cell::new(0);
            let sends = Cell::new(0);
            prepare_invitation_transport(&fixture.state, &source, purpose, async {
                preparations.set(preparations.get() + 1);
                Ok("first recipient transport")
            })
            .await
            .unwrap();
            sends.set(sends.get() + 1);
            {
                let conn = fixture.state.db.writer().await;
                db::calendar::update_event(&conn, &refreshed).unwrap();
            }
            let before_second_recipient = fixture.snapshot();

            let error = prepare_invitation_transport(&fixture.state, &source, purpose, async {
                preparations.set(preparations.get() + 1);
                Ok("second recipient transport")
            })
            .await
            .inspect(|_| sends.set(sends.get() + 1))
            .unwrap_err();

            assert_delivery_refresh_rejected(error, purpose, &refreshed, reason);
            assert_eq!(
                preparations.get(),
                1,
                "second preparation must not be polled"
            );
            assert_eq!(sends.get(), 1, "only the first recipient may be sent");
            assert_eq!(fixture.snapshot(), before_second_recipient);
        }
    }
}

#[tokio::test]
async fn transport_preparation_preserves_errors_without_sending_or_overwriting_refreshes() {
    for purpose in [
        InvitationPurpose::Creation,
        InvitationPurpose::MutationNotification,
    ] {
        let fixture = Fixture::new().await;
        let source = stored_event("preparation-error", RecurrenceKind::Standalone, None);
        fixture.insert(&source).await;
        fixture.attach_meeting_and_pending(&source).await;
        for reclassify in [false, true] {
            {
                let conn = fixture.state.db.writer().await;
                db::calendar::update_event(&conn, &source).unwrap();
            }
            let preparations = Cell::new(0);
            let sends = Cell::new(0);
            let after_preparation = RefCell::new(None);
            let error = prepare_invitation_transport(&fixture.state, &source, purpose, async {
                preparations.set(preparations.get() + 1);
                let conn = fixture.state.db.writer().await;
                if reclassify {
                    let mut refreshed = source.clone();
                    refreshed.recurrence_kind = RecurrenceKind::Unknown;
                    db::calendar::update_event(&conn, &refreshed)?;
                }
                after_preparation.replace(Some(StoredRows::read(&conn)));
                Err::<(), _>(Error::AuthRequired("original preparation failure".into()))
            })
            .await
            .map(|()| sends.set(sends.get() + 1))
            .unwrap_err();

            assert!(matches!(error, Error::AuthRequired(message)
                if message == "original preparation failure"));
            assert_eq!(preparations.get(), 1);
            assert_eq!(sends.get(), 0);
            assert_eq!(
                &fixture.snapshot(),
                after_preparation.borrow().as_ref().unwrap()
            );
        }
    }
}

#[tokio::test]
async fn stable_standalone_transport_preparation_returns_capability_for_both_purposes() {
    let fixture = Fixture::new().await;
    let source = stored_event("stable-standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&source).await;
    fixture.attach_meeting_and_pending(&source).await;
    let before = fixture.snapshot();
    for purpose in [
        InvitationPurpose::Creation,
        InvitationPurpose::MutationNotification,
    ] {
        let preparations = Cell::new(0);
        let transport = prepare_invitation_transport(&fixture.state, &source, purpose, async {
            preparations.set(preparations.get() + 1);
            let conn = fixture.state.db.writer().await;
            assert_eq!(db::calendar::get_event(&conn, &source.id)?, source);
            Ok((42, "prepared session"))
        })
        .await
        .unwrap();

        assert_eq!(transport, (42, "prepared session"));
        assert_eq!(preparations.get(), 1);
        checked_delivery_snapshot(&fixture.state.db.reader(), &source, purpose).unwrap();
        assert_eq!(fixture.snapshot(), before);
    }
}

#[tokio::test]
async fn stable_series_transport_preparation_retains_only_the_creation_exemption() {
    let fixture = Fixture::new().await;
    let source = stored_event("stable-series", RecurrenceKind::Series, Some("FREQ=WEEKLY"));
    fixture.insert(&source).await;
    fixture.attach_meeting_and_pending(&source).await;
    let before = fixture.snapshot();
    let preparations = Cell::new(0);
    let transport = prepare_invitation_transport(
        &fixture.state,
        &source,
        InvitationPurpose::Creation,
        async {
            preparations.set(preparations.get() + 1);
            let conn = fixture.state.db.writer().await;
            assert_eq!(db::calendar::get_event(&conn, &source.id)?, source);
            Ok("series creation transport")
        },
    )
    .await
    .unwrap();

    assert_eq!(transport, "series creation transport");
    checked_delivery_snapshot(
        &fixture.state.db.reader(),
        &source,
        InvitationPurpose::Creation,
    )
    .unwrap();
    let error = prepare_invitation_transport(
        &fixture.state,
        &source,
        InvitationPurpose::MutationNotification,
        async {
            preparations.set(preparations.get() + 1);
            Ok("must not prepare series notification")
        },
    )
    .await
    .unwrap_err();

    assert_blocked(error, CalendarMutationBlockReason::Recurring);
    assert_eq!(preparations.get(), 1);
    assert_eq!(fixture.snapshot(), before);
}

#[tokio::test]
async fn delivery_snapshot_rejects_post_preparation_refresh_before_final_attendee_write() {
    for purpose in [
        InvitationPurpose::Creation,
        InvitationPurpose::MutationNotification,
    ] {
        let fixture = Fixture::new().await;
        let source = stored_event("final-attendees", RecurrenceKind::Standalone, None);
        fixture.insert(&source).await;
        fixture.attach_meeting_and_pending(&source).await;
        let refreshes = delivery_recurrence_refreshes(&source)
            .into_iter()
            .map(|(event, reason)| (event, Some(reason)))
            .chain(
                delivery_content_refreshes(&source)
                    .into_iter()
                    .map(|event| (event, None)),
            );
        for (refreshed, reason) in refreshes {
            {
                let conn = fixture.state.db.writer().await;
                db::calendar::update_event(&conn, &source).unwrap();
            }
            prepare_invitation_transport(&fixture.state, &source, purpose, async { Ok(()) })
                .await
                .unwrap();
            let conn = fixture.state.db.writer().await;
            db::calendar::update_event(&conn, &refreshed).unwrap();
            let before_final_write = StoredRows::read(&conn);

            let error = checked_delivery_snapshot(&conn, &source, purpose)
                .and_then(|()| {
                    conn.execute(
                        "UPDATE calendar_events SET attendees_json = '[]' WHERE id = ?1",
                        params![source.id],
                    )?;
                    Ok(())
                })
                .unwrap_err();

            assert_delivery_refresh_rejected(error, purpose, &refreshed, reason);
            assert_eq!(StoredRows::read(&conn), before_final_write);
            assert_eq!(
                db::calendar::get_event(&conn, &source.id).unwrap(),
                refreshed
            );
        }
    }
}

#[tokio::test]
async fn standalone_notification_accepts_empty_recipients_and_preserves_event_metadata() {
    let fixture = Fixture::new().await;
    let mut expected = stored_event("notified-standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&expected).await;
    fixture.attach_meeting_and_pending(&expected).await;
    let before = fixture.snapshot();

    notify_calendar_event_inner(
        &fixture.state,
        expected.account_id.clone(),
        expected.id.clone(),
        vec![],
    )
    .await
    .unwrap();

    expected.attendees_json = Some("[]".into());
    assert_eq!(fixture.event(&expected.id), expected);
    let after = fixture.snapshot();
    assert_eq!(after.events.len(), 1);
    assert_eq!(after.calendars, before.calendars);
    assert_eq!(after.meetings, before.meetings);
    assert_eq!(after.pending, before.pending);
}
