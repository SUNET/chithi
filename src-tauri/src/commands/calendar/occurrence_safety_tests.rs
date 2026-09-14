//! Command-level recurrence guards, ownership preservation, and move races.

use std::cell::{Cell, RefCell};

use rusqlite::{params, types::Value, Connection};

use super::{
    attach_created_event_identity, capture_move_source, checked_delivery_snapshot,
    checked_invitation_snapshot, checked_invitation_target, checked_mutation_target,
    configured_invite_destination, create_event_inner, create_event_with_metadata,
    create_event_with_receipt, delete_event_inner, delete_event_with_destination,
    ensure_cross_account_invitation_copy, import_calendar_groups_inner,
    move_event_to_calendar_inner, notify_calendar_event_inner, prepare_invitation_transport,
    send_invites_inner, update_event_inner, ImportedEventMetadata, InvitationPurpose,
    MeetBindingInput, MoveSourceSnapshot, NewEventInput, UpdateEventInput,
};
use crate::calendar::{ical, Attendee, CalendarEvent, RecurrenceKind};
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

    fn move_source(&self, id: &str) -> MoveSourceSnapshot {
        let conn = self.state.db.reader();
        let transaction = conn.unchecked_transaction().unwrap();
        let snapshot = capture_move_source(&transaction, id).unwrap();
        transaction.commit().unwrap();
        snapshot
    }

    async fn create_series(&self) -> CalendarEvent {
        let id = create_event_inner(
            &self.state,
            new_event("account-a", "source", Some("FREQ=WEEKLY;COUNT=3")),
            None,
        )
        .await
        .unwrap();
        self.event(&id)
    }

    fn snapshot(&self) -> StoredRows {
        StoredRows::read(&self.state.db.reader())
    }

    async fn calendar_protocol(&self, protocol: &str) {
        db::service_bindings::insert(
            &*self.state.db.writer().await,
            &db::service_bindings::ServiceBinding {
                id: format!("calendar-{protocol}"),
                account_id: "account-a".into(),
                service: "calendar".into(),
                protocol: protocol.into(),
                enabled: true,
                sync_interval_seconds: None,
                config_json: "{}".into(),
            },
        )
        .unwrap();
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
    revisions: Vec<Vec<Value>>,
    invitation_proofs: Vec<Vec<Value>>,
    invitation_sources: Vec<Vec<Value>>,
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
            revisions: rows(
                conn,
                "SELECT * FROM calendar_event_revisions ORDER BY event_id",
            ),
            invitation_proofs: rows(
                conn,
                "SELECT * FROM calendar_invitation_recurrence ORDER BY event_id",
            ),
            invitation_sources: rows(
                conn,
                "SELECT * FROM calendar_invitation_sources ORDER BY event_id",
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

#[tokio::test]
async fn concurrent_calendar_imports_create_only_one_local_copy() {
    let fixture = Fixture::new().await;
    let groups = ical::parse_ical_event_groups(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n\
         UID:import@example.test\r\nSUMMARY:Imported event\r\n\
         DTSTART:20260914T080000Z\r\nDTEND:20260914T090000Z\r\n\
         END:VEVENT\r\nEND:VCALENDAR\r\n",
    )
    .unwrap();
    let first = import_calendar_groups_inner(
        &fixture.state,
        "message",
        "source",
        vec!["import@example.test".into()],
        groups.clone(),
    );
    let second = import_calendar_groups_inner(
        &fixture.state,
        "message",
        "source",
        vec!["import@example.test".into()],
        groups,
    );

    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.imported + second.imported, 1);
    assert_eq!(first.skipped_existing + second.skipped_existing, 1);
    assert_eq!(fixture.snapshot().events.len(), 1);
}

fn rsvp_invite(properties: &str) -> crate::calendar::ical::ParsedInvite {
    let raw = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
         UID:rsvp@example.test\r\nDTSTAMP:20260901T080000Z\r\n\
         DTSTART:20260914T090000Z\r\nDTEND:20260914T100000Z\r\n\
         SUMMARY:Incoming title\r\n{properties}END:VEVENT\r\nEND:VCALENDAR\r\n"
    );
    crate::calendar::ical::parse_ical_data(&raw).remove(0)
}

#[tokio::test]
async fn rsvp_existing_standalone_persists_incoming_recurrence_evidence() {
    for (properties, kind) in [
        ("RRULE:FREQ=WEEKLY\r\n", RecurrenceKind::Series),
        ("RDATE:20260921T090000Z\r\n", RecurrenceKind::Series),
        (
            "RECURRENCE-ID:20260914T090000Z\r\n",
            RecurrenceKind::Occurrence,
        ),
        ("RRULE:\r\n", RecurrenceKind::Unknown),
    ] {
        let fixture = Fixture::new().await;
        let original = stored_event("rsvp", RecurrenceKind::Standalone, None);
        fixture.insert(&original).await;
        let invite = rsvp_invite(properties);
        assert_eq!(invite.recurrence_kind, kind);
        let conn = fixture.state.db.writer().await;
        let mut existing = db::calendar::get_event_by_uid_and_start(
            &conn,
            &original.account_id,
            &invite.uid,
            &invite.dtstart,
        )
        .unwrap()
        .unwrap();
        super::persist_existing_invite_response(
            &conn,
            &mut existing,
            &invite,
            "tentative".into(),
            Some("[]".into()),
        )
        .unwrap();
        let persisted = db::calendar::get_event(&conn, &original.id).unwrap();
        assert_eq!(persisted.recurrence_kind, kind);
        assert_eq!(persisted.recurrence_rule, invite.recurrence_rule);
        assert_eq!(
            persisted.ical_data.as_deref(),
            Some(invite.ical_raw.as_str())
        );
        assert!(persisted.ensure_mutable().is_err());
        assert!(db::calendar_invitation::validated_series_rule(&conn, &persisted).is_err());
        let mut expected = original;
        expected.recurrence_kind = kind;
        expected.recurrence_rule = invite.recurrence_rule;
        expected.ical_data = Some(invite.ical_raw);
        expected.my_status = Some("tentative".into());
        expected.attendees_json = Some("[]".into());
        assert_eq!(
            serde_json::to_value(persisted).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
}

#[tokio::test]
async fn rsvp_existing_protected_event_never_loses_recurrence_evidence() {
    for kind in [
        RecurrenceKind::Series,
        RecurrenceKind::Occurrence,
        RecurrenceKind::Unknown,
    ] {
        for properties in ["", "RRULE:\r\n"] {
            let fixture = Fixture::new().await;
            let original = stored_event("rsvp", kind, Some("FREQ=WEEKLY"));
            fixture.insert(&original).await;
            let conn = fixture.state.db.writer().await;
            let mut existing = original.clone();
            super::persist_existing_invite_response(
                &conn,
                &mut existing,
                &rsvp_invite(properties),
                "declined".into(),
                None,
            )
            .unwrap();
            let persisted = db::calendar::get_event(&conn, &original.id).unwrap();
            assert_eq!(persisted.recurrence_kind, kind);
            assert_eq!(persisted.recurrence_rule, original.recurrence_rule);
            assert_eq!(persisted.ical_data, original.ical_data);
            assert!(persisted.ensure_mutable().is_err());
        }
    }
}

#[tokio::test]
async fn rsvp_existing_local_series_invalidates_invitation_proof() {
    let fixture = Fixture::new().await;
    let mut existing = fixture.create_series().await;
    let conn = fixture.state.db.writer().await;
    assert!(db::calendar_invitation::validated_series_rule(&conn, &existing).is_ok());
    super::persist_existing_invite_response(
        &conn,
        &mut existing,
        &rsvp_invite("RRULE:FREQ=WEEKLY\r\n"),
        "accepted".into(),
        None,
    )
    .unwrap();
    assert!(db::calendar_invitation::validated_series_rule(&conn, &existing).is_err());
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

fn assert_unproven_series(error: Error) {
    assert!(
        matches!(&error, Error::Other(message)
            if message.contains("Cannot send this recurring invitation")),
        "expected unrepresentable series invitation rejection, got {error:?}"
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
async fn imported_creation_preserves_source_identity_and_personal_resource() {
    let fixture = Fixture::new().await;
    let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\n\
               BEGIN:VEVENT\r\nUID:source@test\r\nDTSTART:20260915T110000Z\r\n\
               DTEND:20260915T120000Z\r\nSUMMARY:Imported\r\nEND:VEVENT\r\n\
               END:VCALENDAR\r\n";
    let mut input = new_event("account-a", "source", None);
    input.title = "Imported".into();
    let created = create_event_with_metadata(
        &fixture.state,
        input,
        None,
        Some(ImportedEventMetadata {
            uid: "source@test".into(),
            recurrence_kind: RecurrenceKind::Standalone,
            ical_data: raw.into(),
            source_message_id: "message-1".into(),
            organizer_email: None,
            attendees_json: None,
            my_status: None,
            invitation_source: None,
            personal_copy: true,
            require_remote_creation: false,
        }),
    )
    .await
    .unwrap()
    .event;

    assert_eq!(created.uid.as_deref(), Some("source@test"));
    assert_eq!(created.source_message_id.as_deref(), Some("message-1"));
    assert_eq!(created.ical_data.as_deref(), Some(raw));
    assert_eq!(
        created.organizer_email.as_deref(),
        Some("account-a@example.test")
    );
    assert!(created.attendees_json.is_none());
}

#[tokio::test]
async fn imported_invitation_records_source_provenance_for_status_lookup() {
    let fixture = Fixture::new().await;
    let mut input = new_event("account-b", "cross-account", None);
    input.title = "Invitation copy".into();
    let created = create_event_with_metadata(
        &fixture.state,
        input,
        None,
        Some(ImportedEventMetadata {
            uid: "invite@example.test".into(),
            recurrence_kind: RecurrenceKind::Standalone,
            ical_data: "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".into(),
            source_message_id: "message-1".into(),
            organizer_email: Some("organizer@example.test".into()),
            attendees_json: None,
            my_status: Some("accepted".into()),
            invitation_source: Some(db::calendar_invitation_source::InvitationSource {
                source_account_id: "account-a".into(),
                source_message_id: "message-1".into(),
                invitation_uid: "invite@example.test".into(),
            }),
            personal_copy: true,
            require_remote_creation: false,
        }),
    )
    .await
    .unwrap()
    .event;

    let conn = fixture.state.db.reader();
    assert_eq!(
        db::calendar_invitation_source::event_id(&conn, "account-a", "invite@example.test")
            .unwrap()
            .as_deref(),
        Some(created.id.as_str())
    );
    assert_eq!(
        db::calendar_invitation_source::response_status(&conn, "account-a", "invite@example.test")
            .unwrap()
            .as_deref(),
        Some("accepted")
    );
}

#[tokio::test]
async fn invitation_destination_comes_from_the_source_mail_binding() {
    let fixture = Fixture::new().await;
    let conn = fixture.state.db.writer().await;
    db::service_bindings::insert(
        &conn,
        &db::service_bindings::ServiceBinding {
            id: "source-mail".into(),
            account_id: "account-a".into(),
            service: "mail".into(),
            protocol: "imap".into(),
            enabled: true,
            sync_interval_seconds: None,
            config_json: "{}".into(),
        },
    )
    .unwrap();
    db::service_bindings::set_default_import_calendar(&conn, "account-a", Some("cross-account"))
        .unwrap();

    let (calendar, account) = configured_invite_destination(&conn, "account-a")
        .unwrap()
        .unwrap();
    assert_eq!(calendar.id, "cross-account");
    assert_eq!(account.id, "account-b");
}

#[tokio::test]
async fn cross_account_copy_is_idempotent_and_unanswered_until_delivery() {
    let fixture = Fixture::new().await;
    let (calendar, destination_account) = {
        let conn = fixture.state.db.reader();
        (
            db::calendar::get_calendar(&conn, "cross-account").unwrap(),
            db::accounts::get_account_full(&conn, "account-b").unwrap(),
        )
    };
    let invite = ical::ParsedInvite {
        method: "REQUEST".into(),
        uid: "cross-account@example.test".into(),
        summary: Some("Cross-account invite".into()),
        description: None,
        location: None,
        dtstart: "2026-09-15T11:00:00Z".into(),
        dtend: "2026-09-15T12:00:00Z".into(),
        all_day: false,
        timezone: Some("Europe/Stockholm".into()),
        organizer_email: Some("organizer@example.test".into()),
        organizer_name: None,
        attendees: vec![attendee("account-a@example.test")],
        recurrence_rule: None,
        recurrence_kind: RecurrenceKind::Standalone,
        sequence: 0,
        ical_raw: "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".into(),
    };

    let first = ensure_cross_account_invitation_copy(
        &fixture.state,
        "account-a",
        Some("message-1"),
        &invite.uid,
        &invite,
        &calendar,
        &destination_account,
    )
    .await
    .unwrap();
    let second = ensure_cross_account_invitation_copy(
        &fixture.state,
        "account-a",
        Some("message-1"),
        &invite.uid,
        &invite,
        &calendar,
        &destination_account,
    )
    .await
    .unwrap();

    assert_eq!(second, first);
    let conn = fixture.state.db.reader();
    let event = db::calendar::get_event(&conn, &first).unwrap();
    assert_eq!(event.account_id, "account-b");
    assert_eq!(event.calendar_id, "cross-account");
    assert!(event.my_status.is_none());
    assert_eq!(
        db::calendar_invitation_source::response_status(
            &conn,
            "account-a",
            "cross-account@example.test"
        )
        .unwrap(),
        None
    );
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

        if event.recurrence_kind == RecurrenceKind::Series {
            assert_unproven_series(error);
        } else {
            assert_blocked(error, reason);
        }
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
        assert_eq!(
            event.recurrence_rule.as_deref(),
            rule.filter(|rule| !rule.is_empty())
        );
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
async fn imported_and_provider_series_without_proof_cannot_prepare_or_write_attendees() {
    let fixture = Fixture::new().await;
    let mut events = Vec::new();
    for (id, recurrence) in [
        ("imported-rrule", "RRULE:FREQ=WEEKLY;COUNT=3"),
        ("rdate-only", "RDATE:20260921T090000Z"),
        (
            "rrule-exdate",
            "RRULE:FREQ=WEEKLY;COUNT=3\r\nEXDATE:20260921T090000Z",
        ),
    ] {
        let raw = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nMETHOD:REQUEST\r\n\
             BEGIN:VEVENT\r\nUID:{id}\r\nDTSTAMP:20260901T080000Z\r\n\
             DTSTART:20260914T090000Z\r\nDTEND:20260914T100000Z\r\n\
             {recurrence}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let parsed = crate::calendar::ical::parse_ical_data(&raw);
        assert_eq!(parsed.len(), 1, "{id}");
        assert_eq!(parsed[0].recurrence_kind, RecurrenceKind::Series, "{id}");
        let mut event = stored_event(
            id,
            parsed[0].recurrence_kind,
            parsed[0].recurrence_rule.as_deref(),
        );
        event.ical_data = Some(raw);
        event.remote_id = None;
        events.push(event);
    }
    // These are the lossy provider/cache shapes: overrides are absent from
    // CalendarEvent, and neither missing raw ICS nor a missing remote ID proves
    // that the RRULE is the complete recurrence definition.
    for (id, rule, remote_id) in [
        (
            "provider-series",
            Some("FREQ=WEEKLY"),
            Some("remote-series"),
        ),
        (
            "provider-overrides",
            Some("FREQ=WEEKLY"),
            Some("remote-overrides"),
        ),
        ("provider-added-dates", None, Some("remote-added-dates")),
        ("cached-without-provenance", Some("FREQ=WEEKLY"), None),
    ] {
        let mut event = stored_event(id, RecurrenceKind::Series, rule);
        event.ical_data = None;
        event.source_message_id = None;
        event.remote_id = remote_id.map(str::to_owned);
        events.push(event);
    }
    for event in events {
        fixture.insert(&event).await;
        fixture.attach_meeting_and_pending(&event).await;
        let before = fixture.snapshot();
        assert!(before.invitation_proofs.is_empty());
        let preparations = Cell::new(0);
        let error = prepare_invitation_transport(
            &fixture.state,
            &event,
            InvitationPurpose::Creation,
            async {
                preparations.set(preparations.get() + 1);
                Ok("credentials must not be requested")
            },
        )
        .await
        .unwrap_err();
        if event.ical_data.is_none() && event.recurrence_rule.is_some() {
            assert!(
                matches!(&error, Error::Other(message)
                if message.contains("complete local recurrence proof is missing or stale")),
                "{}: {error:?}",
                event.id
            );
        }
        assert_unproven_series(error);
        assert_eq!(preparations.get(), 0, "{}", event.id);
        assert_unproven_series(
            send_invites_inner(
                &fixture.state,
                event.account_id.clone(),
                event.id.clone(),
                vec!["replacement@example.test".into()],
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(fixture.snapshot(), before, "{}", event.id);
    }
}

#[tokio::test]
async fn local_series_creation_normalizes_the_rule_used_by_generated_invitations() {
    let fixture = Fixture::new().await;
    for (rule, normalized) in [
        (
            "FREQ=WEEKLY;COUNT=3;BYDAY=MO,WE",
            "FREQ=WEEKLY;COUNT=3;BYDAY=MO,WE",
        ),
        (
            " RRULE:freq=weekly; count=3; byday=mo, we ",
            "FREQ=WEEKLY;COUNT=3;BYDAY=MO,WE",
        ),
        (
            "rrule:freq=weekly;until=20260928T090000z",
            "FREQ=WEEKLY;UNTIL=20260928T090000Z",
        ),
    ] {
        let id = create_event_inner(
            &fixture.state,
            new_event("account-a", "source", Some(rule)),
            None,
        )
        .await
        .unwrap();
        let (_, event, recipients) = checked_invitation_snapshot(
            &fixture.state.db.reader(),
            &id,
            Some(("account-a".into(), vec!["guest@example.test".into()])),
            InvitationPurpose::Creation,
        )
        .unwrap();
        assert_eq!(event.recurrence_rule.as_deref(), Some(normalized));
        let ical = crate::calendar::ical::generate_invite(
            event.uid.as_deref().unwrap(),
            &event.title,
            &event.start_time,
            &event.end_time,
            event.location.as_deref(),
            event.description.as_deref(),
            event.organizer_email.as_deref().unwrap(),
            None,
            &recipients,
            event.recurrence_rule.as_deref(),
            event.timezone.as_deref(),
        );
        assert!(!ical.contains("RRULE:RRULE:"), "{ical}");
        let rules: Vec<_> = ical
            .lines()
            .filter(|line| line.starts_with("RRULE:"))
            .collect();
        assert_eq!(rules, [format!("RRULE:{normalized}")]);
        let parsed = crate::calendar::ical::parse_ical_data(&ical);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].recurrence_rule, event.recurrence_rule);
        assert_eq!(parsed[0].recurrence_kind, RecurrenceKind::Series);
    }
    assert_eq!(fixture.snapshot().invitation_proofs.len(), 3);
}

#[tokio::test]
async fn malformed_creation_rules_fail_before_event_or_meeting_ownership_writes() {
    let fixture = Fixture::new().await;
    let existing = stored_event("existing", RecurrenceKind::Standalone, None);
    fixture.insert(&existing).await;
    let binding = fixture.attach_meeting_and_pending(&existing).await;
    let before = fixture.snapshot();
    for rule in [
        "RRULE:RRULE:FREQ=WEEKLY",
        "FREQ=WEEKLY\r\nEXDATE:20260921T090000Z",
        "FREQ=WEEKLY;COUNT=3\0",
        "FREQ=WEEKLY;\tCOUNT=3",
        "FREQ=WEEKLY;COUNT=bad",
        "FREQ=WEEKLY;COUNT=0",
        "FREQ=WEEKLY;COUNT=-1",
        "FREQ=WEEKLY;COUNT=4294967296",
        "FREQ=WEEKLY;COUNT=3;COUNT=4",
        "FREQ=WEEKLY;UNTIL=bad",
        "FREQ=WEEKLY;UNTIL=20260230T090000Z",
        "FREQ=WEEKLY;UNTIL=2026-09-21T09:00:00Z",
        "FREQ=WEEKLY;COUNT=3;UNTIL=20260921T090000Z",
    ] {
        let mut input = new_event("account-a", "source", Some(rule));
        input.meet_binding = Some(binding.clone());
        let error = create_event_inner(&fixture.state, input, None)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, Error::Other(message)
            if message.contains("recurrence rule cannot be represented safely")),
            "{rule:?}: {error:?}"
        );
        assert_eq!(fixture.snapshot(), before, "{rule:?}");
    }
}

#[tokio::test]
async fn failed_invitation_proof_recording_rolls_back_series_creation() {
    let fixture = Fixture::new().await;
    let existing = stored_event("existing", RecurrenceKind::Standalone, None);
    fixture.insert(&existing).await;
    let binding = fixture.attach_meeting_and_pending(&existing).await;
    fixture
        .state
        .db
        .writer()
        .await
        .execute_batch(
            "CREATE TRIGGER reject_invitation_proof AFTER INSERT ON calendar_invitation_recurrence
         BEGIN SELECT RAISE(ABORT, 'fixture proof insertion failure'); END;",
        )
        .unwrap();
    let mut input = new_event("account-a", "source", Some("FREQ=WEEKLY"));
    input.meet_binding = Some(binding);
    let before = fixture.snapshot();
    let error = create_event_inner(&fixture.state, input, None)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("fixture proof insertion failure"),
        "{error:?}"
    );
    assert_eq!(fixture.snapshot(), before);
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
async fn move_destination_is_rechecked_after_waiting_for_source_deletion_transaction() {
    let fixture = Fixture::new().await;
    let source = stored_event("standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&source).await;
    fixture.attach_meeting_and_pending(&source).await;
    let snapshot = fixture.move_source(&source.id);
    let copied = create_event_with_receipt(
        &fixture.state,
        new_event("account-b", "cross-account", None),
        Some(&snapshot),
    )
    .await
    .unwrap();
    assert!(copied.ensure_current(&fixture.state.db.reader()).is_err());

    let mut conn = fixture.state.db.writer().await;
    let deletion = delete_event_with_destination(
        &fixture.state,
        source.id.clone(),
        Some(&snapshot),
        Some(&copied),
    );
    tokio::pin!(deletion);
    assert!(futures::poll!(deletion.as_mut()).is_pending());
    let transaction = conn.transaction().unwrap();
    db::calendar_event_deletion::delete_event(&transaction, &copied.event.id).unwrap();
    transaction.commit().unwrap();
    let before_release = StoredRows::read(&conn);
    drop(conn);

    assert!(deletion.await.is_err());
    assert_eq!(fixture.event(&source.id), source);
    assert_eq!(fixture.snapshot(), before_release);
}

#[tokio::test]
async fn creation_receipt_tracks_canonical_identity_without_losing_content_or_series_proof() {
    for rule in [None, Some("FREQ=WEEKLY;COUNT=3")] {
        let fixture = Fixture::new().await;
        let mut created = create_event_with_receipt(
            &fixture.state,
            new_event("account-b", "cross-account", rule),
            None,
        )
        .await
        .unwrap();
        let before = fixture.snapshot();
        let revision = created.revision;
        let mut expected = created.event.clone();
        expected.remote_id = Some("remote-copy".into());
        expected.uid = Some("canonical@example.test".into());
        attach_created_event_identity(
            &fixture.state,
            &mut created,
            crate::backend::calendar::PushedEvent {
                remote_id: "remote-copy".into(),
                canonical_uid: Some("canonical@example.test".into()),
            },
        )
        .await
        .unwrap();

        assert_eq!(created.event, expected);
        assert_eq!(fixture.event(&expected.id), expected);
        assert!(created.revision > revision);
        let conn = fixture.state.db.reader();
        let transaction = conn.unchecked_transaction().unwrap();
        created.ensure_current(&transaction).unwrap();
        transaction.commit().unwrap();
        let after = fixture.snapshot();
        assert_eq!(after.invitation_proofs, before.invitation_proofs);
        assert_eq!(after.calendars, before.calendars);
        assert_eq!(after.meetings, before.meetings);
        assert_eq!(after.pending, before.pending);
    }
}

#[tokio::test]
async fn failed_identity_attachment_rolls_back_both_identifiers_and_keeps_original_receipt() {
    let fixture = Fixture::new().await;
    let mut created = create_event_with_receipt(
        &fixture.state,
        new_event("account-b", "cross-account", None),
        None,
    )
    .await
    .unwrap();
    let event = created.event.clone();
    let revision = created.revision;
    fixture
        .state
        .db
        .writer()
        .await
        .execute_batch(
            "CREATE TRIGGER reject_canonical_uid BEFORE UPDATE OF uid ON calendar_events
         BEGIN SELECT RAISE(ABORT, 'identity write failed'); END;",
        )
        .unwrap();
    let before = fixture.snapshot();
    let error = attach_created_event_identity(
        &fixture.state,
        &mut created,
        crate::backend::calendar::PushedEvent {
            remote_id: "remote-copy".into(),
            canonical_uid: Some("canonical@example.test".into()),
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("identity write failed"));
    assert_eq!(created.event, event);
    assert_eq!(created.revision, revision);
    assert_eq!(fixture.snapshot(), before);
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
        db::calendar::update_event(&*fixture.state.db.writer().await, &source).unwrap();
        let snapshot = fixture.move_source(&source.id);
        {
            let conn = fixture.state.db.writer().await;
            db::calendar::update_event(&conn, &refreshed).unwrap();
        }
        let before = fixture.snapshot();

        let error = create_event_inner(
            &fixture.state,
            new_event("account-b", "cross-account", None),
            Some(&snapshot),
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
        db::calendar::update_event(&*fixture.state.db.writer().await, &source).unwrap();
        let snapshot = fixture.move_source(&source.id);
        {
            let conn = fixture.state.db.writer().await;
            db::calendar::update_event(&conn, &refreshed).unwrap();
        }
        let before = fixture.snapshot();

        let error = delete_event_inner(&fixture.state, source.id.clone(), Some(&snapshot))
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
        let snapshot = fixture.move_source(&source.id);
        {
            let conn = fixture.state.db.writer().await;
            db::calendar::update_event(&conn, &refreshed).unwrap();
        }
        let before = fixture.snapshot();

        {
            let conn = fixture.state.db.reader();
            let transaction = conn.unchecked_transaction().unwrap();
            assert_blocked(
                checked_mutation_target(&transaction, &source.id, Some(&snapshot)).unwrap_err(),
                reason,
            );
        }
        assert_blocked(
            create_event_inner(
                &fixture.state,
                new_event("account-b", "cross-account", None),
                Some(&snapshot),
            )
            .await
            .unwrap_err(),
            reason,
        );
        assert_eq!(fixture.snapshot(), before);
        assert_blocked(
            delete_event_inner(&fixture.state, source.id.clone(), Some(&snapshot))
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

/// Changes invisible to CalendarEvent equality still invalidate a move. The SQL
/// is also used inside the copy-boundary trigger, so neither probe needs sleeps.
struct InvisibleMoveRace {
    name: &'static str,
    setup: &'static str,
    change: &'static str,
}

fn invisible_move_races() -> Vec<InvisibleMoveRace> {
    [
        (
            "meeting attachment",
            "DELETE FROM meet_meetings WHERE event_id = 'standalone';",
            "INSERT INTO meet_meetings
                 (event_id, account_id, protocol, meeting_id, join_url)
             VALUES ('standalone', 'account-a', 'zoom', 'newly-owned',
                     'https://example.test/newly-owned');",
        ),
        (
            "meeting replacement",
            "",
            "INSERT OR REPLACE INTO meet_meetings
                 (event_id, account_id, protocol, meeting_id, join_url)
             VALUES ('standalone', 'account-a', 'zoom', 'replacement',
                     'https://example.test/replacement');",
        ),
        (
            "meeting reassigned away",
            "",
            "UPDATE meet_meetings SET event_id = 'other' WHERE event_id = 'standalone';",
        ),
        (
            "meeting reassigned here",
            "UPDATE meet_meetings SET event_id = 'other' WHERE event_id = 'standalone';",
            "UPDATE meet_meetings SET event_id = 'standalone' WHERE event_id = 'other';",
        ),
        (
            "meeting detached",
            "",
            "DELETE FROM meet_meetings WHERE event_id = 'standalone';",
        ),
        (
            "pending RSVP only",
            "",
            "UPDATE calendar_events SET pending_rsvp_status = 'declined'
             WHERE id = 'standalone';",
        ),
        (
            "manual management only",
            "",
            "UPDATE calendar_events SET manually_managed_at = NULL WHERE id = 'standalone';",
        ),
        (
            "creation timestamp only",
            "",
            "UPDATE calendar_events SET created_at = '2026-09-13T09:00:00Z'
             WHERE id = 'standalone';",
        ),
        (
            "update timestamp only",
            "",
            "UPDATE calendar_events SET updated_at = '2026-09-13T09:00:00Z'
             WHERE id = 'standalone';",
        ),
        (
            "same-second hidden writes",
            "",
            "UPDATE calendar_events SET pending_rsvp_status = 'accepted'
             WHERE id = 'standalone';
             UPDATE calendar_events SET pending_rsvp_status = 'declined'
             WHERE id = 'standalone';",
        ),
        (
            "no-op event write",
            "",
            "UPDATE calendar_events SET title = title WHERE id = 'standalone';",
        ),
        (
            "A to B to A",
            "",
            "UPDATE calendar_events SET title = 'Intermediate title' WHERE id = 'standalone';
             UPDATE calendar_events SET title = 'Original title' WHERE id = 'standalone';",
        ),
        (
            "same-ID delete and reinsert",
            "CREATE TABLE saved_event AS SELECT * FROM calendar_events WHERE id = 'standalone';
             CREATE TABLE saved_meeting AS SELECT * FROM meet_meetings WHERE event_id = 'standalone';",
            "DELETE FROM calendar_events WHERE id = 'standalone';
             INSERT INTO calendar_events SELECT * FROM saved_event;
             INSERT INTO meet_meetings SELECT * FROM saved_meeting;",
        ),
    ]
    .into_iter()
    .map(|(name, setup, change)| InvisibleMoveRace { name, setup, change })
    .collect()
}

async fn invisible_move_fixture(race: &InvisibleMoveRace) -> (Fixture, MoveSourceSnapshot) {
    let fixture = Fixture::new().await;
    let source = stored_event("standalone", RecurrenceKind::Standalone, None);
    fixture.insert(&source).await;
    fixture
        .insert(&stored_event("other", RecurrenceKind::Standalone, None))
        .await;
    fixture.attach_meeting_and_pending(&source).await;
    fixture
        .state
        .db
        .writer()
        .await
        .execute_batch(race.setup)
        .unwrap();
    let snapshot = fixture.move_source(&source.id);
    (fixture, snapshot)
}

#[tokio::test]
async fn cross_account_move_rejects_invisible_races_before_copy_without_writes_or_cleanup() {
    for race in invisible_move_races() {
        let (fixture, source) = invisible_move_fixture(&race).await;
        let conn = fixture.state.db.writer().await;
        let moving = move_event_to_calendar_inner(
            &fixture.state,
            source.event.id.clone(),
            "cross-account".into(),
            "account-b".into(),
        );
        tokio::pin!(moving);
        assert!(
            futures::poll!(moving.as_mut()).is_pending(),
            "{}",
            race.name
        );
        conn.execute_batch(race.change).unwrap();
        let changed = StoredRows::read(&conn);
        assert_eq!(
            db::calendar::get_event(&conn, &source.event.id).unwrap(),
            source.event
        );
        assert!(db::calendar_revision::get(&conn, &source.event.id).unwrap() > source.revision);
        drop(conn);

        assert_stale(moving.await.unwrap_err());
        assert_eq!(fixture.snapshot(), changed, "{}", race.name);
        assert!(
            db::meet_pending_meetings::list_cleanup_requested(&fixture.state.db.reader())
                .unwrap()
                .is_empty(),
            "{}",
            race.name
        );
        assert!(
            fixture
                .snapshot()
                .events
                .iter()
                .all(|row| !row.contains(&Value::Text("cross-account".into()))),
            "{}",
            race.name
        );
    }
}

#[tokio::test]
async fn cross_account_move_preserves_invisible_after_copy_races_without_new_cleanup() {
    for race in invisible_move_races() {
        let (fixture, source) = invisible_move_fixture(&race).await;
        let expected = {
            let conn = fixture.state.db.writer().await;
            // Obtain exact post-race rows independently, then roll back. The
            // real move must preserve these bytes while retaining its copy.
            let transaction = conn.unchecked_transaction().unwrap();
            transaction.execute_batch(race.change).unwrap();
            let expected = StoredRows::read(&transaction);
            transaction.rollback().unwrap();
            conn.execute_batch(&format!(
                "CREATE TRIGGER change_source_after_copy
                 AFTER INSERT ON calendar_events
                 WHEN NEW.calendar_id = 'cross-account'
                 BEGIN {} END;",
                race.change
            ))
            .unwrap();
            expected
        };

        let error = move_event_to_calendar_inner(
            &fixture.state,
            source.event.id.clone(),
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
        assert!(
            matches!(&error, Error::Other(message)
            if message.contains(&copied_id)
                && message.contains("source was not removed")
                && message.contains("changed during the move")),
            "{}: {error:?}",
            race.name
        );
        assert_eq!(
            fixture.event(&source.event.id),
            source.event,
            "{}",
            race.name
        );
        let copied = fixture.event(&copied_id);
        assert_eq!(copied.title, source.event.title);
        assert_eq!(copied.account_id, "account-b");
        assert_eq!(copied.attendees_json, source.event.attendees_json);
        let mut actual = fixture.snapshot();
        assert_eq!(actual.events.len(), expected.events.len() + 1);
        actual
            .events
            .retain(|row| row[0] != Value::Text(copied_id.clone()));
        assert_eq!(actual.events, expected.events, "{}", race.name);
        assert_eq!(actual.meetings, expected.meetings, "{}", race.name);
        assert_eq!(actual.pending, expected.pending, "{}", race.name);
        assert_eq!(actual.calendars, expected.calendars, "{}", race.name);
        assert_eq!(
            actual.invitation_proofs, expected.invitation_proofs,
            "{}",
            race.name
        );
        assert!(
            db::meet_pending_meetings::list_cleanup_requested(&fixture.state.db.reader())
                .unwrap()
                .is_empty(),
            "{}",
            race.name
        );
    }
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
        (InvitationPurpose::Creation, RecurrenceKind::Series, _) => assert_unproven_series(error),
        (_, _, None) => {
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

        let error = notify_calendar_event_inner(&fixture.state, stale_ui_event.id)
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
    let source = fixture.create_series().await;
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
    expected.organizer_email = Some("account-a@example.test".into());
    expected.attendees_json = None;
    fixture.insert(&expected).await;
    fixture.attach_meeting_and_pending(&expected).await;
    let before = fixture.snapshot();

    notify_calendar_event_inner(&fixture.state, expected.id.clone())
        .await
        .unwrap();

    assert_eq!(fixture.event(&expected.id), expected);
    let after = fixture.snapshot();
    assert_eq!(after.events.len(), 1);
    assert_eq!(after.calendars, before.calendars);
    assert_eq!(after.meetings, before.meetings);
    assert_eq!(after.pending, before.pending);
}

#[tokio::test]
async fn unsupported_creation_preserves_all_events_and_pending_meeting_ownership() {
    let fixture = Fixture::new().await;
    fixture.calendar_protocol("google").await;
    let existing = stored_event("existing", RecurrenceKind::Standalone, None);
    fixture.insert(&existing).await;
    let binding = fixture.attach_meeting_and_pending(&existing).await;
    let mut input = new_event("account-a", "source", Some("FREQ=WEEKLY"));
    input.meet_binding = Some(binding);
    let before = fixture.snapshot();
    let error = create_event_inner(&fixture.state, input, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("recurr"), "google: {error}");
    assert_eq!(fixture.snapshot(), before, "google");
}

#[tokio::test]
async fn supported_series_creation_commits_and_claims_pending_meeting() {
    for protocol in ["local", "jmap", "caldav"] {
        let fixture = Fixture::new().await;
        if protocol != "local" {
            fixture.calendar_protocol(protocol).await;
        }
        db::service_bindings::insert(
            &*fixture.state.db.writer().await,
            &db::service_bindings::ServiceBinding {
                id: "meet-zoom".into(),
                account_id: "account-a".into(),
                service: "meet".into(),
                protocol: "zoom".into(),
                enabled: true,
                sync_interval_seconds: None,
                config_json: "{}".into(),
            },
        )
        .unwrap();
        let existing = stored_event("existing", RecurrenceKind::Standalone, None);
        fixture.insert(&existing).await;
        let binding = fixture.attach_meeting_and_pending(&existing).await;
        let meeting_id = binding.meeting_id.clone();
        let mut input = new_event("account-a", "source", Some("FREQ=WEEKLY"));
        input.meet_binding = Some(binding);
        let id = create_event_inner(&fixture.state, input, None)
            .await
            .unwrap();
        let created = fixture.event(&id);
        assert_eq!(created.recurrence_kind, RecurrenceKind::Series);
        assert_eq!(created.recurrence_rule.as_deref(), Some("FREQ=WEEKLY"));
        let rows = fixture.snapshot();
        assert_eq!(rows.events.len(), 2);
        assert!(rows.pending.is_empty());
        assert!(rows
            .meetings
            .iter()
            .any(|row| row.contains(&Value::Text(id.clone()))
                && row.contains(&Value::Text(meeting_id.clone()))));
    }
}

#[tokio::test]
async fn ordinary_notification_uses_fresh_stored_recipients_without_metadata_writes() {
    for protocol in ["google", "graph"] {
        let fixture = Fixture::new().await;
        fixture.calendar_protocol(protocol).await;
        let mut event = stored_event("notification", RecurrenceKind::Standalone, None);
        fixture.insert(&event).await;
        event.organizer_email = Some("ACCOUNT-A@example.test".into());
        event.attendees_json = Some(r#"[{"email":"fresh@example.test","name":"Fresh Guest","status":"accepted","is_self":true}]"#.into());
        db::calendar::update_event(&*fixture.state.db.writer().await, &event).unwrap();
        let before = fixture.snapshot();
        let (account, snapshot, recipients) = checked_invitation_snapshot(
            &fixture.state.db.reader(),
            &event.id,
            None,
            InvitationPurpose::MutationNotification,
        )
        .unwrap();
        assert_eq!(account.id, event.account_id);
        assert_eq!(snapshot, event);
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].email, "fresh@example.test");
        assert_eq!(recipients[0].name.as_deref(), Some("Fresh Guest"));
        assert_eq!(recipients[0].status, "accepted");
        assert_eq!(recipients[0].is_self, Some(true));
        notify_calendar_event_inner(&fixture.state, event.id.clone())
            .await
            .unwrap();
        assert_eq!(fixture.snapshot(), before);
    }
}

#[tokio::test]
async fn ordinary_notification_rejects_changed_or_unknown_organizer_without_writes() {
    for organizer in [None, Some("someone-else@example.test")] {
        let fixture = Fixture::new().await;
        fixture.calendar_protocol("google").await;
        let mut event = stored_event("notification", RecurrenceKind::Standalone, None);
        event.organizer_email = organizer.map(str::to_owned);
        fixture.insert(&event).await;
        let before = fixture.snapshot();
        let error = notify_calendar_event_inner(&fixture.state, event.id)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("organizer"));
        assert_eq!(fixture.snapshot(), before);
    }
}

mod jmap_creation {
    use super::*;
    use serde_json::{json, Value as JsonValue};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Exercise the command's immediate provider push with the injected HTTP
    /// clients. Only discovery and CalendarEvent/set are accepted by this peer.
    struct CreationServer {
        root: String,
        task: tokio::task::JoinHandle<JsonValue>,
    }

    impl Drop for CreationServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl CreationServer {
        async fn start() -> Self {
            Self::start_with_response(None, true).await
        }

        async fn start_with_response(
            mut pause: Option<(
                tokio::sync::oneshot::Sender<()>,
                tokio::sync::oneshot::Receiver<()>,
            )>,
            succeeds: bool,
        ) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let root = format!("http://{}", listener.local_addr().unwrap());
            let base = root.clone();
            let task = tokio::spawn(async move {
                let mut created = JsonValue::Null;
                for request_index in 0..2 {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let mut chunk = [0; 4096];
                    let header_end = loop {
                        let count = stream.read(&mut chunk).await.unwrap();
                        assert!(count > 0);
                        bytes.extend_from_slice(&chunk[..count]);
                        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            break end + 4;
                        }
                    };
                    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    while bytes.len() < header_end + length {
                        let count = stream.read(&mut chunk).await.unwrap();
                        assert!(count > 0);
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    let response = if request_index == 0 {
                        assert!(headers.starts_with("GET /.well-known/jmap "), "{headers}");
                        json!({
                            "apiUrl": format!("{base}/jmap/api"),
                            "downloadUrl": format!("{base}/download/{{blobId}}"),
                            "uploadUrl": format!("{base}/upload/{{accountId}}"),
                            "primaryAccounts": {"urn:ietf:params:jmap:mail": "remote-account"},
                            "accounts": {"remote-account": {"accountCapabilities": {
                                "urn:ietf:params:jmap:mail": {},
                                "urn:ietf:params:jmap:calendars": {}
                            }}}
                        })
                    } else {
                        assert!(headers.starts_with("POST /jmap/api "), "{headers}");
                        let request: JsonValue =
                            serde_json::from_slice(&bytes[header_end..header_end + length])
                                .unwrap();
                        let calls = request["methodCalls"].as_array().unwrap();
                        assert_eq!(calls.len(), 1);
                        assert_eq!(calls[0][0], "CalendarEvent/set");
                        created = calls[0][1]["create"]["new1"].clone();
                        assert!(created.is_object());
                        if let Some((ready, release)) = pause.take() {
                            ready.send(()).unwrap();
                            release.await.unwrap();
                        }
                        let result = if succeeds {
                            json!({"created": {"new1": {"id": "immediate-remote-series"}}})
                        } else {
                            json!({"notCreated": {"new1": {"type": "forbidden"}}})
                        };
                        json!({"methodResponses": [["CalendarEvent/set", result, calls[0][2]]],
                            "sessionState": "state"})
                    };
                    let body = response.to_string();
                    stream.write_all(format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len(),
                    ).as_bytes()).await.unwrap();
                }
                created
            });
            Self { root, task }
        }
    }

    async fn configure_creation(
        fixture: &mut Fixture,
        server: &CreationServer,
        account_id: &str,
        calendar_id: &str,
    ) {
        {
            let conn = fixture.state.db.writer().await;
            db::service_bindings::insert(
                &conn,
                &db::service_bindings::ServiceBinding {
                    id: "calendar-jmap".into(),
                    account_id: account_id.into(),
                    service: "calendar".into(),
                    protocol: "jmap".into(),
                    enabled: true,
                    sync_interval_seconds: None,
                    config_json: "{}".into(),
                },
            )
            .unwrap();
            db::service_bindings::insert(
                &conn,
                &db::service_bindings::ServiceBinding {
                    id: "mail-jmap".into(),
                    account_id: account_id.into(),
                    service: "mail".into(),
                    protocol: "jmap".into(),
                    enabled: true,
                    sync_interval_seconds: None,
                    config_json: json!({"url": server.root, "auth_method": "basic"}).to_string(),
                },
            )
            .unwrap();
            conn.execute(
                "UPDATE calendars SET remote_id = 'remote-calendar' WHERE id = ?1",
                [calendar_id],
            )
            .unwrap();
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        let services = Arc::get_mut(&mut fixture.state.providers).unwrap();
        services.transports.jmap_discovery_http = http.clone();
        services.transports.jmap_api_http = http;
    }

    async fn create_pushed_series(fixture: &mut Fixture) -> CalendarEvent {
        let mut server = CreationServer::start().await;
        configure_creation(fixture, &server, "account-a", "source").await;
        let id = create_event_inner(
            &fixture.state,
            new_event("account-a", "source", Some("RRULE:freq=weekly;count=3")),
            None,
        )
        .await
        .unwrap();
        let event = fixture.event(&id);
        assert_eq!(event.remote_id.as_deref(), Some("immediate-remote-series"));
        let wire = (&mut server.task).await.unwrap();
        assert_eq!(wire["calendarIds"], json!({"remote-calendar": true}));
        assert_eq!(wire["recurrenceRules"][0]["frequency"], "weekly");
        assert_eq!(wire["recurrenceRules"][0]["count"], 3);
        assert_eq!(wire["uid"].as_str(), event.uid.as_deref());
        assert_eq!(
            event.recurrence_rule.as_deref(),
            Some("FREQ=WEEKLY;COUNT=3")
        );
        assert_eq!(
            fixture.snapshot().invitation_proofs,
            vec![vec![
                Value::Text(id),
                Value::Text("FREQ=WEEKLY;COUNT=3".into()),
            ]]
        );
        event
    }

    #[tokio::test]
    async fn cross_account_move_preserves_source_when_destination_changes_during_push() {
        for succeeds in [false, true] {
            for race in [
                "calendar-delete",
                "unsubscribe",
                "account-delete",
                "copy-delete",
                "copy-edit",
                "copy-move",
                "copy-replace",
                "copy-aba",
                "meeting-aba",
                "calendar-owner",
                "calendar-remote",
                "calendar-subscription",
                "missing-revision",
                "identity-write-failure",
            ] {
                if race == "identity-write-failure" && !succeeds {
                    continue;
                }
                let mut fixture = Fixture::new().await;
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                let mut server =
                    CreationServer::start_with_response(Some((ready_tx, release_rx)), succeeds)
                        .await;
                configure_creation(&mut fixture, &server, "account-b", "cross-account").await;
                let source = stored_event("standalone", RecurrenceKind::Standalone, None);
                fixture.insert(&source).await;
                fixture.attach_meeting_and_pending(&source).await;
                let before = fixture.snapshot();
                let moving = move_event_to_calendar_inner(
                    &fixture.state,
                    source.id.clone(),
                    "cross-account".into(),
                    "account-b".into(),
                );
                let change = async {
                    ready_rx.await.unwrap();
                    let mut conn = fixture.state.db.writer().await;
                    let transaction = conn.transaction().unwrap();
                    let id: String = transaction
                        .query_row(
                            "SELECT id FROM calendar_events WHERE calendar_id = 'cross-account'",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap();
                    match race {
                        "calendar-delete" | "unsubscribe" => {
                            db::calendar_event_deletion::delete_calendar_events(
                                &transaction,
                                "cross-account",
                            )
                            .unwrap();
                            if race == "calendar-delete" {
                                db::calendar::delete_calendar_row(&transaction, "cross-account")
                                    .unwrap();
                            } else {
                                db::calendar::set_calendar_subscribed(
                                    &transaction,
                                    "cross-account",
                                    false,
                                )
                                .unwrap();
                            }
                        }
                        "account-delete" => {
                            db::calendar_event_deletion::delete_account_events(
                                &transaction,
                                "account-b",
                            )
                            .unwrap();
                            transaction
                                .execute("DELETE FROM accounts WHERE id = 'account-b'", [])
                                .unwrap();
                        }
                        "copy-delete" | "copy-replace" => {
                            let copy = db::calendar::get_event(&transaction, &id).unwrap();
                            db::calendar_event_deletion::delete_event(&transaction, &id).unwrap();
                            if race == "copy-replace" {
                                db::calendar::insert_event(&transaction, &copy).unwrap();
                            }
                        }
                        "copy-edit" | "copy-aba" => {
                            let copy = db::calendar::get_event(&transaction, &id).unwrap();
                            transaction
                                .execute(
                                    "UPDATE calendar_events SET title = 'Changed' WHERE id = ?1",
                                    [&id],
                                )
                                .unwrap();
                            if race == "copy-aba" {
                                transaction
                                    .execute(
                                        "UPDATE calendar_events SET title = ?1 WHERE id = ?2",
                                        params![copy.title, id],
                                    )
                                    .unwrap();
                            }
                        }
                        "copy-move" => {
                            transaction.execute("UPDATE calendar_events SET calendar_id = 'same-account' WHERE id = ?1", [&id]).unwrap();
                        }
                        "meeting-aba" => {
                            transaction.execute(
                                "INSERT INTO meet_meetings (event_id, account_id, protocol, meeting_id, join_url)
                                 VALUES (?1, 'account-b', 'zoom', 'new-meeting', 'https://example.test/meeting')", [&id],
                            ).unwrap();
                            transaction
                                .execute("DELETE FROM meet_meetings WHERE event_id = ?1", [&id])
                                .unwrap();
                        }
                        "calendar-owner" => {
                            transaction.execute("UPDATE calendars SET account_id = 'account-a' WHERE id = 'cross-account'", []).unwrap();
                        }
                        "calendar-remote" => {
                            transaction.execute("UPDATE calendars SET remote_id = 'other-remote' WHERE id = 'cross-account'", []).unwrap();
                        }
                        "calendar-subscription" => {
                            db::calendar::set_calendar_subscribed(
                                &transaction,
                                "cross-account",
                                false,
                            )
                            .unwrap();
                        }
                        "missing-revision" => {
                            transaction
                                .execute(
                                    "DELETE FROM calendar_event_revisions WHERE event_id = ?1",
                                    [&id],
                                )
                                .unwrap();
                        }
                        "identity-write-failure" => {
                            transaction.execute_batch(
                                "CREATE TRIGGER reject_copy_identity BEFORE UPDATE OF remote_id ON calendar_events
                                 WHEN OLD.calendar_id = 'cross-account'
                                 BEGIN SELECT RAISE(ABORT, 'identity write failed'); END;",
                            ).unwrap();
                        }
                        _ => unreachable!(),
                    }
                    transaction.commit().unwrap();
                    let after_change = StoredRows::read(&conn);
                    drop(conn);
                    release_tx.send(()).unwrap();
                    (id, after_change)
                };
                let (result, (id, after_change)) =
                    tokio::time::timeout(std::time::Duration::from_secs(5), async {
                        tokio::join!(moving, change)
                    })
                    .await
                    .unwrap();
                let error = result.unwrap_err().to_string();
                assert!(
                    error.contains(&id) && error.contains("source was not removed"),
                    "{race}, {succeeds}: {error}"
                );
                assert_eq!(fixture.event(&source.id), source, "{race}, {succeeds}");
                assert_eq!(fixture.snapshot(), after_change, "{race}, {succeeds}");
                assert_eq!(after_change.meetings, before.meetings);
                assert_eq!(after_change.pending, before.pending);
                (&mut server.task).await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn unchanged_destination_allows_move_after_successful_or_failed_push() {
        for succeeds in [false, true] {
            let mut fixture = Fixture::new().await;
            let mut server = CreationServer::start_with_response(None, succeeds).await;
            configure_creation(&mut fixture, &server, "account-b", "cross-account").await;
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
            let copy = fixture.event(&id);
            assert_eq!(
                copy.remote_id.as_deref(),
                succeeds.then_some("immediate-remote-series")
            );
            assert_eq!(copy.title, source.title);
            assert!(db::calendar::get_event(&fixture.state.db.reader(), &source.id).is_err());
            (&mut server.task).await.unwrap();
        }
    }

    #[tokio::test]
    async fn immediate_jmap_remote_id_attachment_preserves_creation_invitation_proof() {
        let mut fixture = Fixture::new().await;
        let event = create_pushed_series(&mut fixture).await;
        let before = fixture.snapshot();
        let transport = prepare_invitation_transport(
            &fixture.state,
            &event,
            InvitationPurpose::Creation,
            async { Ok("series creation transport") },
        )
        .await
        .unwrap();
        assert_eq!(transport, "series creation transport");
        assert_eq!(fixture.snapshot(), before);
        send_invites_inner(
            &fixture.state,
            event.account_id.clone(),
            event.id.clone(),
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(
            fixture.snapshot().invitation_proofs,
            before.invitation_proofs
        );
        assert_blocked(
            notify_calendar_event_inner(&fixture.state, event.id)
                .await
                .unwrap_err(),
            CalendarMutationBlockReason::Recurring,
        );
    }

    #[tokio::test]
    async fn identical_provider_refresh_revokes_proof_during_transport_preparation() {
        let mut fixture = Fixture::new().await;
        let event = create_pushed_series(&mut fixture).await;
        fixture.attach_meeting_and_pending(&event).await;
        let preparations = Cell::new(0);
        let sends = Cell::new(0);
        let after_refresh = RefCell::new(None);
        let writer = fixture.state.db.writer().await;
        let delivery = prepare_invitation_transport(
            &fixture.state,
            &event,
            InvitationPurpose::Creation,
            async {
                preparations.set(preparations.get() + 1);
                let conn = fixture.state.db.writer().await;
                db::calendar::upsert_event_by_remote_id(&conn, &event)?;
                assert_eq!(db::calendar::get_event(&conn, &event.id)?, event);
                let refreshed = StoredRows::read(&conn);
                assert!(refreshed.invitation_proofs.is_empty());
                after_refresh.replace(Some(refreshed));
                Ok("prepared but no longer authorized transport")
            },
        );
        tokio::pin!(delivery);
        assert!(futures::poll!(delivery.as_mut()).is_pending());
        assert_eq!(preparations.get(), 1);
        drop(writer);
        let error = delivery
            .await
            .inspect(|_| sends.set(sends.get() + 1))
            .unwrap_err();
        assert_unproven_series(error);
        assert_eq!(sends.get(), 0);
        assert_eq!(fixture.event(&event.id), event);
        assert_eq!(
            &fixture.snapshot(),
            after_refresh.borrow().as_ref().unwrap()
        );

        assert_unproven_series(
            send_invites_inner(
                &fixture.state,
                event.account_id.clone(),
                event.id.clone(),
                vec!["new@example.test".into()],
            )
            .await
            .unwrap_err(),
        );
        let error = prepare_invitation_transport(
            &fixture.state,
            &event,
            InvitationPurpose::Creation,
            async {
                preparations.set(preparations.get() + 1);
                Ok("must not prepare the next recipient")
            },
        )
        .await
        .unwrap_err();
        assert_unproven_series(error);
        assert_eq!(preparations.get(), 1);
        assert_eq!(
            &fixture.snapshot(),
            after_refresh.borrow().as_ref().unwrap()
        );
    }
}
