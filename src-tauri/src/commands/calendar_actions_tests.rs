use super::*;
use crate::backend::calendar::{CalendarBackend, PushedEvent};
use crate::calendar::event_set::NativeCalendarResource;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Remote {
    sets: HashMap<(String, String), CalendarEventSet>,
    creates: HashMap<String, CalendarEventSet>,
    create_count: usize,
    delete_count: usize,
    lose_create_response: bool,
    fail_delete: bool,
    native: bool,
    lose_move_response: bool,
    lose_update_response: bool,
    update_count: usize,
    fetched_ids: Vec<Option<String>>,
}

struct Fake {
    protocol: &'static str,
    remote: Arc<Mutex<Remote>>,
}

fn participants(event: &CalendarEvent) -> Vec<serde_json::Value> {
    serde_json::from_str(event.attendees_json.as_deref().unwrap_or("[]")).unwrap()
}

fn ical_text(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace(';', "\\;")
        .replace(',', "\\,")
}

fn ical_position(event: &CalendarEvent, value: &str) -> String {
    if event.all_day {
        format!(";VALUE=DATE:{}", value.replace('-', ""))
    } else {
        format!(
            ":{}",
            chrono::DateTime::parse_from_rfc3339(value)
                .unwrap()
                .with_timezone(&chrono::Utc)
                .format("%Y%m%dT%H%M%SZ")
        )
    }
}

fn ical_event(event: &CalendarEvent, key: Option<&str>) -> String {
    let mut lines = vec![
        "BEGIN:VEVENT".into(),
        format!("UID:{}", ical_text(event.uid.as_deref().unwrap())),
        "DTSTAMP:20260914T000000Z".into(),
        format!("SUMMARY:{}", ical_text(&event.title)),
        format!("DTSTART{}", ical_position(event, &event.start_time)),
        format!("DTEND{}", ical_position(event, &event.end_time)),
        format!(
            "DESCRIPTION:{}",
            ical_text(event.description.as_deref().unwrap_or(""))
        ),
    ];
    if let Some(key) = key {
        lines.push(format!("RECURRENCE-ID{}", ical_position(event, key)));
    }
    if let Some(rule) = &event.recurrence_rule {
        lines.push(format!("RRULE:{rule}"));
    }
    if let Some(location) = &event.location {
        lines.push(format!("LOCATION:{}", ical_text(location)));
    }
    if let Some(organizer) = &event.organizer_email {
        lines.push(format!("ORGANIZER:mailto:{organizer}"));
    }
    for attendee in participants(event) {
        let (role, kind) = match attendee["role"].as_str().unwrap_or("required") {
            "required" => ("REQ-PARTICIPANT", "INDIVIDUAL"),
            "optional" => ("OPT-PARTICIPANT", "INDIVIDUAL"),
            "resource" => ("REQ-PARTICIPANT", "RESOURCE"),
            "chair" => ("CHAIR", "INDIVIDUAL"),
            "non-participant" => ("NON-PARTICIPANT", "INDIVIDUAL"),
            role => panic!("unsupported fixture role: {role}"),
        };
        lines.push(format!(
            "ATTENDEE;ROLE={role};CUTYPE={kind};PARTSTAT={}:mailto:{}",
            attendee["status"]
                .as_str()
                .unwrap_or("needs-action")
                .to_ascii_uppercase(),
            attendee["email"].as_str().unwrap()
        ));
    }
    lines.push("END:VEVENT".into());
    lines.join("\r\n") + "\r\n"
}

fn native_data(protocol: &str, event: &CalendarEvent, content_type: &str) -> String {
    use serde_json::json;
    let attendees = participants(event);
    let body = event.description.as_deref().unwrap_or("");
    match protocol {
        "caldav" => format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Chithi//Fixture//EN\r\n{}END:VCALENDAR\r\n",
            ical_event(event, None)
        ),
        "jmap" => {
            let mut people = serde_json::Map::new();
            for attendee in &attendees {
                let email = attendee["email"].as_str().unwrap();
                let role = attendee["role"].as_str().unwrap_or("required");
                let mut roles = json!({"attendee": true});
                match role {
                    "required" | "resource" => {}
                    "optional" | "chair" => roles[role] = json!(true),
                    "non-participant" => roles["informational"] = json!(true),
                    _ => panic!("unsupported fixture role: {role}"),
                }
                people.insert(email.into(), json!({
                    "calendarAddress": format!("mailto:{email}"),
                    "roles": roles,
                    "kind": if role == "resource" { "resource" } else { "individual" },
                    "participationStatus": attendee["status"].as_str().unwrap_or("needs-action")
                }));
            }
            if let Some(organizer) = &event.organizer_email {
                let owner = people.entry(organizer.clone()).or_insert_with(|| json!({
                    "calendarAddress": format!("mailto:{organizer}"),
                    "roles": {}, "participationStatus": "accepted"
                }));
                owner["roles"]["owner"] = json!(true);
            }
            json!({"event": {
                "@type": "Event", "uid": event.uid, "title": event.title,
                "description": body, "descriptionContentType": content_type,
                "participants": people
            }}).to_string()
        }
        "graph" => json!({
            "subject": event.title,
            "body": {"content": body,
                "contentType": if content_type == "text/html" { "html" } else { "text" }},
            "organizer": event.organizer_email.as_ref().map(|email|
                json!({"emailAddress": {"address": email}})),
            "attendees": attendees.iter().map(|a| json!({
                "emailAddress": {"address": a["email"], "name": a["name"]},
                "type": a["role"].as_str().unwrap_or("required"),
                "status": {"response": match a["status"].as_str().unwrap_or("needs-action") {
                    "needs-action" => "notResponded",
                    "tentative" => "tentativelyAccepted",
                    status => status,
                }}
            })).collect::<Vec<_>>()
        }).to_string(),
        "google" => json!({
            "summary": event.title, "description": body,
            "organizer": event.organizer_email.as_ref().map(|email| json!({"email": email})),
            "attendees": attendees.iter().map(|a| {
                let role = a["role"].as_str().unwrap_or("required");
                assert!(matches!(role, "required" | "optional" | "resource"));
                json!({"email": a["email"], "displayName": a["name"],
                    "optional": role == "optional", "resource": role == "resource",
                    "responseStatus": match a["status"].as_str().unwrap_or("needs-action") {
                        "needs-action" => "needsAction",
                        status => status,
                    }})
            }).collect::<Vec<_>>()
        }).to_string(),
        _ => unreachable!(),
    }
}

fn assert_native_projections(set: &CalendarEventSet) {
    assert!(
        set.content.is_none(),
        "canonical fixtures must derive native content"
    );
    for key in std::iter::once(None).chain(
        set.overrides
            .iter()
            .filter(|item| item.event.is_some())
            .map(|item| Some(item.original_start.as_str())),
    ) {
        set.semantic_content(key)
            .unwrap_or_else(|error| panic!("invalid fixture projection at {key:?}: {error}"));
    }
}

/// Model a provider read after a write, including embedded effective exceptions.
fn refresh_native(set: &mut CalendarEventSet, protocol: &str) {
    let formats: Vec<_> = std::iter::once(set.description_content_type(None).unwrap())
        .chain(set.overrides.iter().map(|item| {
            item.event.as_ref().map_or("text/plain", |_| {
                set.description_content_type(Some(&item.original_start))
                    .unwrap()
            })
        }))
        .collect();
    for (event, format) in std::iter::once(Some(&mut set.event))
        .chain(set.overrides.iter_mut().map(|item| item.event.as_mut()))
        .zip(&formats)
    {
        if protocol == "google" && *format == "text/plain" {
            if let Some(body) = event.and_then(|event| event.description.as_mut()) {
                *body = body
                    .replace("\r\n", "\n")
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('\n', "<br>");
            }
        }
    }
    let mut data = native_data(protocol, &set.event, formats[0]);
    if protocol == "caldav" {
        data = data
            .strip_suffix("END:VEVENT\r\nEND:VCALENDAR\r\n")
            .unwrap()
            .into();
        for item in &set.overrides {
            if item.event.is_none() {
                data.push_str(&format!(
                    "EXDATE{}\r\n",
                    ical_position(&set.event, &item.original_start)
                ));
            }
        }
        data.push_str("END:VEVENT\r\n");
        for item in &set.overrides {
            if let Some(event) = &item.event {
                data.push_str(&ical_event(event, Some(&item.original_start)));
            }
        }
        data.push_str("END:VCALENDAR\r\n");
    } else if protocol == "jmap" {
        let mut envelope: serde_json::Value = serde_json::from_str(&data).unwrap();
        let mut overrides = serde_json::Map::new();
        for (item, format) in set.overrides.iter().zip(&formats[1..]) {
            let key = if set.event.all_day {
                format!("{}T00:00:00", item.original_start)
            } else {
                let zone: chrono_tz::Tz = set
                    .event
                    .timezone
                    .as_deref()
                    .unwrap_or("UTC")
                    .parse()
                    .unwrap();
                chrono::DateTime::parse_from_rfc3339(&item.original_start)
                    .unwrap()
                    .with_timezone(&zone)
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string()
            };
            let patch = item
                .event
                .as_ref()
                .map(|event| {
                    let mut value: serde_json::Value =
                        serde_json::from_str(&native_data(protocol, event, format)).unwrap();
                    let mut patch = value["event"].take();
                    patch.as_object_mut().unwrap().remove("@type");
                    patch.as_object_mut().unwrap().remove("uid");
                    patch
                })
                .unwrap_or_else(|| serde_json::json!({"excluded": true}));
            overrides.insert(key, patch);
        }
        envelope["event"]["timeZone"] =
            serde_json::json!(set.event.timezone.as_deref().unwrap_or("UTC"));
        envelope["event"]["recurrenceOverrides"] = overrides.into();
        data = envelope.to_string();
    }
    let native = set.native.as_mut().unwrap();
    native.protocol = protocol.into();
    native.data = data;
    for (index, (item, format)) in set.overrides.iter_mut().zip(&formats[1..]).enumerate() {
        if let Some(event) = &mut item.event {
            let mut resource = native.clone();
            if matches!(protocol, "google" | "graph") {
                resource.event_id = item
                    .native
                    .as_ref()
                    .filter(|old| old.protocol == protocol)
                    .map(|old| old.event_id.clone())
                    .unwrap_or_else(|| format!("{}-exception-{index}", native.event_id));
                resource.data = native_data(protocol, event, format);
            }
            event.remote_id = Some(resource.event_id.clone());
            event.etag = resource.revision.clone();
            item.native = Some(resource);
        } else {
            item.native = None;
        }
    }
    set.content = None;
    assert_native_projections(set);
}

#[test]
fn fake_native_projections_cover_bodies_people_and_selected_overrides() {
    let key = "2026-09-15";
    for protocol in ["google", "graph", "jmap", "caldav"] {
        let event = CalendarEvent {
            uid: Some("fixture-uid".into()),
            start_time: "2026-09-14".into(),
            end_time: "2026-09-15".into(),
            all_day: true,
            recurrence_rule: Some("FREQ=DAILY;COUNT=3".into()),
            recurrence_kind: RecurrenceKind::Series,
            description: Some("Agenda <one> & two; three, four\\five\nNext line".into()),
            organizer_email: Some("owner@example.test".into()),
            attendees_json: Some(
                serde_json::json!([
                    {"email": "required@example.test", "role": "required", "status": "accepted"},
                    {"email": "optional@example.test", "role": "optional", "status": "tentative"},
                    {"email": "room@example.test", "role": "resource", "status": "declined"}
                ])
                .to_string(),
            ),
            ..crate::backend::testutil::event()
        };
        let mut replacement = event.clone();
        replacement.recurrence_rule = None;
        replacement.recurrence_kind = RecurrenceKind::Occurrence;
        replacement.start_time = key.into();
        replacement.end_time = "2026-09-16".into();
        replacement.description = Some("Different <body> & punctuation; , \\\nOverride".into());
        replacement.organizer_email = Some("override-owner@example.test".into());
        replacement.attendees_json = Some(
            serde_json::json!([
                {"email": "override@example.test", "role": "optional", "status": "needs-action"}
            ])
            .to_string(),
        );
        let intended = CalendarEventSet {
            event,
            overrides: vec![CalendarOverride {
                original_start: key.into(),
                event: Some(replacement),
                native: None,
            }],
            native: None,
            content: None,
        };
        let mut canonical = intended.clone();
        canonical.capture_content().unwrap();
        canonical.native = Some(NativeCalendarResource {
            protocol: protocol.into(),
            calendar_id: "calendar".into(),
            event_id: "master".into(),
            revision: Some("revision".into()),
            data: String::new(),
        });
        refresh_native(&mut canonical, protocol);
        assert!(semantic_eq(&canonical, &intended), "{protocol}");
        if protocol == "google" {
            assert!(canonical
                .event
                .description
                .as_ref()
                .unwrap()
                .contains("&lt;one&gt; &amp;"));
        }

        // Fresh native data, not frozen intent, must reject projection drift.
        for selected in [None, Some(key)] {
            for field in ["body", "organizer", "response", "role"] {
                let mut broken = canonical.clone();
                let event = if selected.is_some() {
                    broken.overrides[0].event.as_mut().unwrap()
                } else {
                    &mut broken.event
                };
                match field {
                    "body" => event.description = Some("stale preview".into()),
                    "organizer" => event.organizer_email = None,
                    "response" | "role" => {
                        let mut people = participants(event);
                        if field == "response" {
                            people[0]["status"] = serde_json::json!("declined");
                        } else {
                            people[0]["role"] = serde_json::json!("resource");
                        }
                        event.attendees_json = Some(serde_json::to_string(&people).unwrap());
                    }
                    _ => unreachable!(),
                }
                assert!(
                    broken.semantic_content(selected).is_err(),
                    "{protocol} {selected:?} {field}"
                );
            }
        }

        let selected = canonical.standalone_at(key).unwrap();
        let mut returned = selected.clone();
        refresh_native(&mut returned, protocol);
        assert!(semantic_eq(&returned, &selected), "selected {protocol}");

        let mut new_meeting = canonical.clone();
        new_meeting
            .prepare_new_meeting("destination@example.test")
            .unwrap();
        let mut returned = new_meeting.clone();
        refresh_native(&mut returned, protocol);
        assert!(
            semantic_eq(&returned, &new_meeting),
            "new meeting {protocol}"
        );
        for (event, former_owner, count) in [
            (&returned.event, "owner@example.test", 4),
            (
                returned.overrides[0].event.as_ref().unwrap(),
                "override-owner@example.test",
                2,
            ),
        ] {
            assert_eq!(
                event.organizer_email.as_deref(),
                Some("destination@example.test")
            );
            let people = participants(event);
            assert_eq!(people.len(), count);
            assert!(people.iter().any(|a| a["email"] == former_owner));
            assert!(people.iter().all(|a| a["status"] == "needs-action"));
        }
    }
}

#[async_trait::async_trait]
impl CalendarBackend for Fake {
    fn protocol(&self) -> &'static str {
        self.protocol
    }
    async fn fetch_event_set(
        &self,
        _: &CalendarBackendCtx<'_>,
        account: &db::accounts::AccountFull,
        anchor: &CalendarEvent,
        calendar: &str,
    ) -> Result<CalendarEventSet> {
        let mut remote = self.remote.lock().unwrap();
        remote.fetched_ids.push(anchor.remote_id.clone());
        remote
            .sets
            .get(&(account.id.clone(), calendar.to_string()))
            .cloned()
            .ok_or_else(|| invalid("remote event missing"))
    }
    async fn update_event_set(
        &self,
        _: &CalendarBackendCtx<'_>,
        account: &db::accounts::AccountFull,
        before: &CalendarEventSet,
        desired: &CalendarEventSet,
    ) -> Result<CalendarEventSet> {
        let mut remote = self.remote.lock().unwrap();
        let calendar = before.native.as_ref().unwrap().calendar_id.clone();
        let key = (account.id.clone(), calendar);
        if remote.sets.get(&key) != Some(before) {
            return Err(invalid("revision conflict"));
        }
        let mut canonical = desired.clone();
        canonical.native.as_mut().unwrap().revision = Some("updated".into());
        canonical.event.etag = Some("updated".into());
        refresh_native(&mut canonical, self.protocol);
        remote.sets.insert(key, canonical.clone());
        remote.update_count += 1;
        if remote.lose_update_response {
            remote.lose_update_response = false;
            return Err(invalid("simulated lost update response"));
        }
        Ok(canonical)
    }
    async fn create_event_set(
        &self,
        _: &CalendarBackendCtx<'_>,
        account: &db::accounts::AccountFull,
        calendar: &str,
        desired: &CalendarEventSet,
        operation_id: &str,
    ) -> Result<CalendarEventSet> {
        let mut remote = self.remote.lock().unwrap();
        if let Some(existing) = remote.creates.get(operation_id) {
            return Ok(existing.clone());
        }
        let mut canonical = desired.clone();
        canonical.capture_content().unwrap();
        canonical.event.remote_id = Some(operation_id.to_string());
        canonical.event.etag = Some("created".into());
        canonical.native = Some(NativeCalendarResource {
            protocol: self.protocol.into(),
            calendar_id: calendar.into(),
            event_id: operation_id.into(),
            revision: Some("created".into()),
            data: String::new(),
        });
        for item in &mut canonical.overrides {
            item.native = None;
        }
        refresh_native(&mut canonical, self.protocol);
        remote
            .sets
            .insert((account.id.clone(), calendar.into()), canonical.clone());
        remote
            .creates
            .insert(operation_id.into(), canonical.clone());
        remote.create_count += 1;
        if remote.lose_create_response {
            remote.lose_create_response = false;
            return Err(invalid("simulated lost creation response"));
        }
        Ok(canonical)
    }
    async fn delete_event_set(
        &self,
        _: &CalendarBackendCtx<'_>,
        account: &db::accounts::AccountFull,
        before: &CalendarEventSet,
    ) -> Result<()> {
        let mut remote = self.remote.lock().unwrap();
        if remote.fail_delete {
            return Err(invalid("simulated source removal failure"));
        }
        remote.sets.remove(&(
            account.id.clone(),
            before.native.as_ref().unwrap().calendar_id.clone(),
        ));
        remote.delete_count += 1;
        Ok(())
    }
    async fn move_event_set_native(
        &self,
        _: &CalendarBackendCtx<'_>,
        account: &db::accounts::AccountFull,
        before: &CalendarEventSet,
        calendar: &str,
    ) -> Result<CalendarCapability<CalendarEventSet>> {
        let mut remote = self.remote.lock().unwrap();
        if !remote.native {
            return Ok(CalendarCapability::Unsupported);
        }
        let mut canonical = remote
            .sets
            .remove(&(
                account.id.clone(),
                before.native.as_ref().unwrap().calendar_id.clone(),
            ))
            .unwrap();
        canonical.native.as_mut().unwrap().calendar_id = calendar.into();
        for item in &mut canonical.overrides {
            if let Some(native) = &mut item.native {
                native.calendar_id = calendar.into();
            }
        }
        assert_native_projections(&canonical);
        remote
            .sets
            .insert((account.id.clone(), calendar.into()), canonical.clone());
        if remote.lose_move_response {
            remote.lose_move_response = false;
            return Err(invalid("simulated lost native move response"));
        }
        Ok(CalendarCapability::Supported(canonical))
    }
    async fn sync(&self, _: &CalendarBackendCtx<'_>, _: &db::accounts::AccountFull) -> Result<()> {
        unreachable!()
    }
    fn validate_event_creation(&self, _: &CalendarEvent, _: &str) -> Result<()> {
        unreachable!()
    }
    async fn push_created_event(
        &self,
        _: &CalendarBackendCtx<'_>,
        _: &db::accounts::AccountFull,
        _: &CalendarEvent,
        _: &str,
    ) -> Result<Option<PushedEvent>> {
        unreachable!()
    }
    async fn push_deleted_event(
        &self,
        _: &CalendarBackendCtx<'_>,
        _: &db::accounts::AccountFull,
        _: &str,
        _: &str,
    ) -> Result<()> {
        unreachable!()
    }
    async fn push_calendar_rename(
        &self,
        _: &CalendarBackendCtx<'_>,
        _: &db::accounts::AccountFull,
        _: &str,
        _: &str,
    ) -> Result<()> {
        unreachable!()
    }
    async fn push_calendar_color(
        &self,
        _: &CalendarBackendCtx<'_>,
        _: &db::accounts::AccountFull,
        _: &str,
        _: &str,
    ) -> Result<()> {
        unreachable!()
    }
}

async fn fixture(
    source_protocol: Option<&str>,
    target_protocol: Option<&str>,
    same_account: bool,
) -> (
    tempfile::TempDir,
    AppState,
    store::Operation,
    Arc<Mutex<Remote>>,
) {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new(directory.path().to_path_buf()).unwrap();
    let mut conn = state.db.writer().await;
    conn.execute_batch("INSERT INTO accounts(id, display_name, email, username) VALUES ('source', 'Source', 's@example.test', 's'), ('target', 'Target', 't@example.test', 't');
        INSERT INTO calendars(id, account_id, name) VALUES ('source-calendar', 'source', 'Source'), ('target-calendar', 'target', 'Target');").unwrap();
    if same_account {
        conn.execute(
            "UPDATE calendars SET account_id = 'source' WHERE id = 'target-calendar'",
            [],
        )
        .unwrap();
    }
    for (account, calendar, protocol) in [
        ("source", "source-calendar", source_protocol),
        (
            if same_account { "source" } else { "target" },
            "target-calendar",
            target_protocol,
        ),
    ] {
        if let Some(protocol) = protocol {
            conn.execute(
                "UPDATE calendars SET remote_id = ?1 WHERE id = ?1",
                [calendar],
            )
            .unwrap();
            if calendar == "source-calendar" || !same_account {
                db::service_bindings::insert(
                    &conn,
                    &db::service_bindings::ServiceBinding {
                        id: format!("{account}-binding"),
                        account_id: account.into(),
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
    }
    let event = CalendarEvent {
        id: "source-event".into(),
        account_id: "source".into(),
        calendar_id: "source-calendar".into(),
        uid: Some("original-uid".into()),
        start_time: "2026-09-14".into(),
        end_time: "2026-09-16".into(),
        all_day: true,
        recurrence_rule: Some("FREQ=DAILY;COUNT=4".into()),
        recurrence_kind: RecurrenceKind::Series,
        remote_id: source_protocol.map(|_| "remote-source".into()),
        etag: source_protocol.map(|_| "before".into()),
        ..crate::backend::testutil::event()
    };
    let mut set = CalendarEventSet {
        event: event.clone(),
        content: None,
        overrides: vec![CalendarOverride {
            original_start: "2026-09-17".into(),
            event: None,
            native: None,
        }],
        native: source_protocol.map(|protocol| NativeCalendarResource {
            protocol: protocol.into(),
            calendar_id: "source-calendar".into(),
            event_id: "remote-source".into(),
            revision: Some("before".into()),
            data: native_data(protocol, &event, "text/plain"),
        }),
    };
    if let Some(protocol) = source_protocol {
        refresh_native(&mut set, protocol);
    }
    assert_native_projections(&set);
    let tx = conn.transaction().unwrap();
    db::calendar::insert_event(&tx, &event).unwrap();
    store::persist_embedded(&tx, &event, &set).unwrap();
    db::meet_meetings::upsert(
        &tx,
        &db::meet_meetings::MeetMeeting {
            event_id: event.id.clone(),
            account_id: "source".into(),
            protocol: "zoom".into(),
            meeting_id: "meeting".into(),
            join_url: "https://example.test/join".into(),
        },
    )
    .unwrap();
    let snapshot = store::Snapshot {
        calendar_revision: store::calendar_revision(&tx, "source-calendar").unwrap(),
        invitation_source: store::invitation_source(&tx, "source-event").unwrap(),
        account_route: store::account_route(&tx, "source").unwrap(),
        token: uuid::Uuid::new_v4().to_string(),
        anchor: event.clone(),
        members: vec![store::event_version(&tx, event).unwrap()],
        set: set.clone(),
        remote_calendar_id: source_protocol.map(|_| "source-calendar".into()),
    };
    let input = CalendarActionInput {
        selection: CalendarSelection {
            event_id: snapshot.anchor.id.clone(),
            token: snapshot.token.clone(),
            original_start: Some("2026-09-14".into()),
        },
        scope: RecurrenceMutationScope::EntireSeries,
        edit: CalendarEdit::default(),
        destination_calendar_id: Some("target-calendar".into()),
        reset_exceptions: false,
    };
    let operation = store::Operation {
        id: uuid::Uuid::new_v4().to_string(),
        source: snapshot,
        input,
        desired: set.clone(),
        destination: Some(destination(&tx, "target-calendar").unwrap()),
        destination_event_id: "destination-event".into(),
        stage: CalendarActionStage::Planned,
        canonical: None,
        source_after: None,
        native_move: false,
        auto_resume: false,
    };
    store::insert_operation(&tx, &operation).unwrap();
    tx.commit().unwrap();
    drop(conn);
    let remote = Arc::new(Mutex::new(Remote::default()));
    remote
        .lock()
        .unwrap()
        .sets
        .insert(("source".into(), "source-calendar".into()), set);
    (directory, state, operation, remote)
}

#[tokio::test]
async fn local_whole_transfer_is_transactional_and_preserves_shared_meeting() {
    let (_directory, state, operation, _) = fixture(None, None, false).await;
    let outcome = execute(&state, &operation.id, &CalendarConfirmations::default())
        .await
        .unwrap();
    assert_eq!(outcome.stage, CalendarActionStage::Completed);
    let conn = state.db.reader();
    assert!(db::calendar::get_event(&conn, "source-event").is_err());
    let event = db::calendar::get_event(&conn, "destination-event").unwrap();
    assert_eq!(event.account_id, "target");
    assert!(event.remote_id.is_none());
    let stored = store::local_set(&conn, &event).unwrap();
    assert_eq!(stored.overrides.len(), 1);
    assert!(stored.overrides[0].event.is_none());
    assert!(db::meet_meetings::get(&conn, "destination-event")
        .unwrap()
        .is_some());
    assert!(db::meet_pending_meetings::list(&conn).unwrap().is_empty());
    drop(conn);
    assert_eq!(
        execute(&state, &operation.id, &CalendarConfirmations::default())
            .await
            .unwrap()
            .stage,
        CalendarActionStage::Completed
    );
}

#[tokio::test]
async fn all_provider_protocol_pairs_transfer_full_finite_sets() {
    for source in ["google", "graph", "jmap", "caldav"] {
        for target in ["google", "graph", "jmap", "caldav"] {
            let (_directory, state, operation, remote) =
                fixture(Some(source), Some(target), false).await;
            let source_backend = Fake {
                protocol: source,
                remote: remote.clone(),
            };
            let target_backend = Fake {
                protocol: target,
                remote: remote.clone(),
            };
            let backends: [&dyn CalendarBackend; 2] = [&source_backend, &target_backend];
            execute_with_backends(
                &state,
                &operation.id,
                &CalendarConfirmations::default(),
                Some(&backends),
            )
            .await
            .unwrap();
            let remote = remote.lock().unwrap();
            let transferred = remote
                .sets
                .get(&("target".into(), "target-calendar".into()))
                .unwrap();
            assert_eq!(transferred.overrides.len(), 1, "{source} -> {target}");
            assert!(transferred.overrides[0].event.is_none());
            assert_eq!(transferred.event.uid, operation.source.set.event.uid);
            assert_eq!(remote.create_count, 1);
            assert_eq!(remote.delete_count, 1);
        }
    }
}

#[tokio::test]
async fn lost_create_response_reuses_durable_identity_and_retains_source_until_verified() {
    let (directory, state, operation, remote) = fixture(Some("caldav"), Some("jmap"), false).await;
    remote.lock().unwrap().lose_create_response = true;
    let source = Fake {
        protocol: "caldav",
        remote: remote.clone(),
    };
    let target = Fake {
        protocol: "jmap",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 2] = [&source, &target];
    assert!(execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends)
    )
    .await
    .is_err());
    assert!(db::calendar::get_event(&state.db.reader(), "source-event").is_ok());
    assert_eq!(remote.lock().unwrap().delete_count, 0);
    drop(state);
    let state = AppState::new(directory.path().to_path_buf()).unwrap();
    let outcome = execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends),
    )
    .await
    .unwrap();
    assert_eq!(outcome.stage, CalendarActionStage::Completed);
    assert_eq!(remote.lock().unwrap().create_count, 1);
    assert_eq!(remote.lock().unwrap().delete_count, 1);
}

#[tokio::test]
async fn source_removal_failure_is_durable_and_never_recreates_destination() {
    let (_directory, state, operation, remote) = fixture(Some("caldav"), Some("jmap"), false).await;
    remote.lock().unwrap().fail_delete = true;
    let source = Fake {
        protocol: "caldav",
        remote: remote.clone(),
    };
    let target = Fake {
        protocol: "jmap",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 2] = [&source, &target];
    assert!(execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends)
    )
    .await
    .is_err());
    assert_eq!(
        store::load_operation(&state.db.reader(), &operation.id)
            .unwrap()
            .stage,
        CalendarActionStage::SourceRemovalPending
    );
    assert!(db::calendar::get_event(&state.db.reader(), "source-event").is_ok());
    remote.lock().unwrap().fail_delete = false;
    execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends),
    )
    .await
    .unwrap();
    assert_eq!(remote.lock().unwrap().create_count, 1);
}

#[tokio::test]
async fn native_move_is_used_without_fallback_creation() {
    let (_directory, state, operation, remote) =
        fixture(Some("google"), Some("google"), true).await;
    remote.lock().unwrap().native = true;
    let backend = Fake {
        protocol: "google",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 1] = [&backend];
    execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends),
    )
    .await
    .unwrap();
    assert_eq!(remote.lock().unwrap().create_count, 0);
    assert_eq!(remote.lock().unwrap().delete_count, 0);
    assert!(
        store::load_operation(&state.db.reader(), &operation.id)
            .unwrap()
            .native_move
    );
}

#[tokio::test]
async fn lost_native_move_response_reconciles_destination_without_copying_again() {
    let (directory, state, operation, remote) = fixture(Some("google"), Some("google"), true).await;
    {
        let mut data = remote.lock().unwrap();
        data.native = true;
        data.lose_move_response = true;
    }
    let backend = Fake {
        protocol: "google",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 1] = [&backend];
    assert!(execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends)
    )
    .await
    .is_err());
    assert!(db::calendar::get_event(&state.db.reader(), "source-event").is_ok());
    drop(state);
    let state = AppState::new(directory.path().to_path_buf()).unwrap();
    let recovered = execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends),
    )
    .await
    .unwrap();
    assert_eq!(recovered.stage, CalendarActionStage::Completed);
    assert_eq!(remote.lock().unwrap().create_count, 0);
}

#[tokio::test]
async fn changed_verified_destination_keeps_source_on_retry() {
    let (_directory, state, operation, remote) = fixture(Some("caldav"), Some("jmap"), false).await;
    remote.lock().unwrap().fail_delete = true;
    let source = Fake {
        protocol: "caldav",
        remote: remote.clone(),
    };
    let target = Fake {
        protocol: "jmap",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 2] = [&source, &target];
    assert!(execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends)
    )
    .await
    .is_err());
    {
        let mut data = remote.lock().unwrap();
        data.fail_delete = false;
        data.sets
            .get_mut(&("target".into(), "target-calendar".into()))
            .unwrap()
            .event
            .title = "Externally changed destination".into();
    }
    assert!(execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends)
    )
    .await
    .is_err());
    assert!(db::calendar::get_event(&state.db.reader(), "source-event").is_ok());
    assert_eq!(remote.lock().unwrap().delete_count, 0);
    assert_eq!(remote.lock().unwrap().create_count, 1);
}

#[tokio::test]
async fn one_occurrence_transfer_creates_standalone_then_excludes_only_source_position() {
    let (_directory, state, mut operation, remote) =
        fixture(Some("caldav"), Some("jmap"), false).await;
    operation.input.scope = RecurrenceMutationScope::ThisOccurrence;
    operation.input.selection.original_start = Some("2026-09-15".into());
    checkpoint(&state, &operation).await.unwrap();
    let source = Fake {
        protocol: "caldav",
        remote: remote.clone(),
    };
    let target = Fake {
        protocol: "jmap",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 2] = [&source, &target];
    execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends),
    )
    .await
    .unwrap();
    let conn = state.db.reader();
    let source = db::calendar::get_event(&conn, "source-event").unwrap();
    assert_eq!(
        source.recurrence_rule,
        operation.source.set.event.recurrence_rule
    );
    assert_eq!(store::local_set(&conn, &source).unwrap().overrides.len(), 2);
    let target = db::calendar::get_event(&conn, "destination-event").unwrap();
    assert_eq!(target.recurrence_kind, RecurrenceKind::Standalone);
    assert_ne!(target.uid, source.uid);
    assert_eq!(target.start_time, "2026-09-15");
    assert!(db::meet_meetings::get(&conn, "source-event")
        .unwrap()
        .is_some());
    assert!(db::meet_meetings::get(&conn, "destination-event")
        .unwrap()
        .is_some());
    assert!(db::meet_pending_meetings::list(&conn).unwrap().is_empty());
}

#[tokio::test]
async fn stale_local_revision_prevents_all_remote_effects() {
    let (_directory, state, operation, remote) = fixture(Some("caldav"), Some("jmap"), false).await;
    state
        .db
        .writer()
        .await
        .execute(
            "UPDATE calendar_events SET title = 'racing edit' WHERE id = 'source-event'",
            [],
        )
        .unwrap();
    let source = Fake {
        protocol: "caldav",
        remote: remote.clone(),
    };
    let target = Fake {
        protocol: "jmap",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 2] = [&source, &target];
    assert!(execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends)
    )
    .await
    .is_err());
    assert_eq!(remote.lock().unwrap().create_count, 0);
    assert_eq!(remote.lock().unwrap().delete_count, 0);
}

#[tokio::test]
async fn local_creation_is_idempotent_and_not_an_unpublished_placeholder() {
    let (_directory, state, _, _) = fixture(None, None, false).await;
    let id = uuid::Uuid::new_v4().to_string();
    let input = || super::super::calendar::NewEventInput {
        account_id: "target".into(),
        calendar_id: "target-calendar".into(),
        title: "Created".into(),
        description: None,
        location: None,
        start_time: "2026-09-14".into(),
        end_time: "2026-09-15".into(),
        all_day: true,
        timezone: None,
        recurrence_rule: Some("FREQ=WEEKLY;COUNT=3".into()),
        attendees: vec![],
        meet_binding: None,
    };
    let first = create_completed(&state, input(), &id).await.unwrap();
    let second = create_completed(&state, input(), &id).await.unwrap();
    assert_eq!(first.event_id, second.event_id);
    assert_eq!(first.stage, CalendarActionStage::Completed);
    let conn = state.db.reader();
    let event = db::calendar::get_event(&conn, &first.event_id).unwrap();
    assert!(event.remote_id.is_none());
    assert_eq!(
        db::calendar::list_events(&conn, "target", None, "2026-01-01", "2027-01-01")
            .unwrap()
            .len(),
        1
    );
}

fn new_input() -> super::super::calendar::NewEventInput {
    super::super::calendar::NewEventInput {
        account_id: "target".into(),
        calendar_id: "target-calendar".into(),
        title: "Created".into(),
        description: None,
        location: None,
        start_time: "2026-09-14".into(),
        end_time: "2026-09-15".into(),
        all_day: true,
        timezone: None,
        recurrence_rule: Some("FREQ=WEEKLY;COUNT=3".into()),
        attendees: vec![],
        meet_binding: None,
    }
}

#[tokio::test]
async fn generated_database_selection_plans_and_executes_one_occurrence() {
    let (_directory, state, _, _) = fixture(None, None, false).await;
    let page = list_occurrences(
        &state,
        "source".into(),
        None,
        "2026-09-14".into(),
        "2026-09-20".into(),
        2,
    )
    .await
    .unwrap();
    assert_eq!(page.occurrences.len(), 2);
    assert!(page.has_more);
    let selection = page.occurrences[1].selection.clone();
    assert_eq!(selection.original_start.as_deref(), Some("2026-09-15"));
    let plan = plan_action(
        &state,
        CalendarActionInput {
            selection,
            scope: RecurrenceMutationScope::ThisOccurrence,
            edit: CalendarEdit {
                title: Some("Only this position".into()),
                ..Default::default()
            },
            destination_calendar_id: None,
            reset_exceptions: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(plan.preview.title, "Only this position");
    execute(
        &state,
        &plan.operation_id,
        &CalendarConfirmations::default(),
    )
    .await
    .unwrap();
    let event = db::calendar::get_event(&state.db.reader(), "source-event").unwrap();
    assert_eq!(event.title, "Standup");
    let page = list_occurrences(
        &state,
        "source".into(),
        None,
        "2026-09-14".into(),
        "2026-09-20".into(),
        10,
    )
    .await
    .unwrap();
    assert_eq!(page.occurrences.len(), 3);
    assert_eq!(
        page.occurrences
            .iter()
            .filter(|row| row.fields.title == "Only this position")
            .count(),
        1
    );
}

#[tokio::test]
async fn attendee_occurrence_is_rejected_before_operation_is_persisted() {
    let (_directory, state, _, _) = fixture(None, None, false).await;
    {
        let mut conn = state.db.writer().await;
        let tx = conn.transaction().unwrap();
        let event = db::calendar::get_event(&tx, "source-event").unwrap();
        let mut set = store::local_set(&tx, &event).unwrap();
        set.event.attendees_json =
            Some(serde_json::json!([{"email": "guest@example.test"}]).to_string());
        store::persist_embedded(&tx, &event, &set).unwrap();
        tx.commit().unwrap();
    }
    let view = read_event_set(&state, "source-event", "2026-09-14", "2026-09-20", 10, None)
        .await
        .unwrap();
    let before: i64 = state
        .db
        .reader()
        .query_row(
            "SELECT COUNT(*) FROM calendar_action_operations",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let error = plan_action(
        &state,
        CalendarActionInput {
            selection: view.page.occurrences[0].selection.clone(),
            scope: RecurrenceMutationScope::ThisOccurrence,
            edit: CalendarEdit {
                title: Some("Must not publish".into()),
                ..Default::default()
            },
            destination_calendar_id: None,
            reset_exceptions: false,
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("meeting with attendees"));
    let after: i64 = state
        .db
        .reader()
        .query_row(
            "SELECT COUNT(*) FROM calendar_action_operations",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after, before);
}

#[tokio::test]
async fn persisted_attendee_occurrence_plan_is_rejected_before_execution() {
    let (_directory, state, mut operation, remote) = fixture(None, None, false).await;
    operation.input.scope = RecurrenceMutationScope::ThisOccurrence;
    operation.source.set.event.attendees_json =
        Some(serde_json::json!([{"email": "guest@example.test"}]).to_string());
    let conn = state.db.writer().await;
    store::save_operation(&conn, &operation).unwrap();
    drop(conn);

    let error = execute(
        &state,
        &operation.id,
        &CalendarConfirmations {
            replacement_meeting_identity: true,
            reset_exceptions: false,
        },
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("meeting with attendees"));
    let remote = remote.lock().unwrap();
    assert_eq!(remote.update_count, 0);
    assert_eq!(remote.create_count, 0);
    assert_eq!(remote.delete_count, 0);
    drop(remote);
    assert_eq!(
        store::load_operation(&state.db.reader(), &operation.id)
            .unwrap()
            .stage,
        CalendarActionStage::Planned
    );
}

#[tokio::test]
async fn provider_sync_requires_rehydration_instead_of_resurrecting_excluded_positions() {
    let (_directory, state, operation, _) = fixture(Some("jmap"), Some("caldav"), false).await;
    let page = list_occurrences(
        &state,
        "source".into(),
        None,
        "2026-09-14".into(),
        "2026-09-20".into(),
        10,
    )
    .await
    .unwrap();
    assert!(page.occurrences.is_empty());
    assert_eq!(page.needs_hydration, ["source-event"]);
    {
        let conn = state.db.writer().await;
        store::cache_canonical(
            &conn,
            operation.source.anchor.clone(),
            &operation.source.set,
        )
        .unwrap();
    }
    let page = list_occurrences(
        &state,
        "source".into(),
        None,
        "2026-09-14".into(),
        "2026-09-20".into(),
        10,
    )
    .await
    .unwrap();
    assert_eq!(page.occurrences.len(), 3);
    assert!(page.needs_hydration.is_empty());
    {
        let conn = state.db.writer().await;
        db::calendar::upsert_event_by_remote_id(&conn, &operation.source.anchor).unwrap();
        conn.execute(
            "INSERT INTO calendar_action_members(event_id, owner_event_id)
             VALUES ('source-event', 'source-event')",
            [],
        )
        .unwrap();
    }
    let page = list_occurrences(
        &state,
        "source".into(),
        None,
        "2026-09-14".into(),
        "2026-09-20".into(),
        10,
    )
    .await
    .unwrap();
    assert!(page.occurrences.is_empty());
    assert_eq!(page.needs_hydration, ["source-event"]);
}

#[tokio::test]
async fn identityless_detached_row_withholds_its_series_but_not_other_events() {
    let (_directory, state, operation, _) = fixture(Some("graph"), None, false).await;
    {
        let conn = state.db.writer().await;
        let child = CalendarEvent {
            id: "unresolved-child".into(),
            uid: operation.source.anchor.uid.clone(),
            remote_id: Some("remote-child".into()),
            recurrence_kind: RecurrenceKind::Occurrence,
            recurrence_rule: None,
            start_time: "2026-09-15".into(),
            end_time: "2026-09-16".into(),
            ..operation.source.anchor.clone()
        };
        db::calendar::insert_event(&conn, &child).unwrap();
        let other = CalendarEvent {
            id: "other-event".into(),
            uid: Some("other-uid".into()),
            remote_id: None,
            recurrence_kind: RecurrenceKind::Standalone,
            recurrence_rule: None,
            start_time: "2026-09-15".into(),
            end_time: "2026-09-16".into(),
            ..operation.source.anchor.clone()
        };
        db::calendar::insert_event(&conn, &other).unwrap();
    }
    let page = list_occurrences(
        &state,
        "source".into(),
        None,
        "2026-09-14".into(),
        "2026-09-20".into(),
        10,
    )
    .await
    .unwrap();
    assert_eq!(page.unresolved.len(), 1);
    assert_eq!(page.unresolved[0].event_id, "unresolved-child");
    assert_eq!(page.unresolved[0].calendar_id, "source-calendar");
    assert_eq!(page.occurrences.len(), 1);
    assert_eq!(page.occurrences[0].event_id, "other-event");
    assert!(page.needs_hydration.is_empty());
    assert!(
        store::latest_snapshot(&state.db.reader(), "unresolved-child")
            .unwrap()
            .is_none()
    );

    // Without a UID, even the child's series cannot be identified. Keep
    // unrelated standalone events, but do not guess which master to expand.
    {
        let conn = state.db.writer().await;
        conn.execute(
            "UPDATE calendar_events SET uid = NULL WHERE id = 'unresolved-child'",
            [],
        )
        .unwrap();
    }
    let page = list_occurrences(
        &state,
        "source".into(),
        None,
        "2026-09-14".into(),
        "2026-09-20".into(),
        10,
    )
    .await
    .unwrap();
    assert_eq!(page.unresolved.len(), 1);
    assert_eq!(page.occurrences.len(), 1);
    assert_eq!(page.occurrences[0].event_id, "other-event");
}

#[tokio::test]
async fn unresolved_child_is_repaired_only_by_an_exact_verified_provider_override() {
    let (_directory, state, operation, remote) = fixture(Some("graph"), None, false).await;
    let mut child = CalendarEvent {
        id: "unresolved-child".into(),
        uid: operation.source.anchor.uid.clone(),
        remote_id: Some("remote-child".into()),
        recurrence_kind: RecurrenceKind::Occurrence,
        recurrence_rule: None,
        start_time: "2026-09-19".into(),
        end_time: "2026-09-21".into(),
        ..operation.source.anchor.clone()
    };
    {
        let conn = state.db.writer().await;
        db::calendar::insert_event(&conn, &child).unwrap();
    }
    let backend = Fake {
        protocol: "graph",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 1] = [&backend];
    let failure = repair_unresolved_occurrence(&state, &child.id, Some(&backends))
        .await
        .unwrap_err();
    assert!(failure
        .to_string()
        .contains("did not verify the detached occurrence"));
    assert_eq!(
        store::owner_id(&state.db.reader(), &child.id).unwrap(),
        child.id
    );

    let mut verified = operation.source.set.clone();
    child.title = "Moved occurrence".into();
    verified.overrides.push(CalendarOverride {
        original_start: "2026-09-15".into(),
        event: Some(child.clone()),
        native: None,
    });
    refresh_native(&mut verified, "graph");
    let moved = verified
        .overrides
        .iter_mut()
        .find(|item| item.original_start == "2026-09-15")
        .unwrap();
    moved.native.as_mut().unwrap().event_id = "remote-child".into();
    remote
        .lock()
        .unwrap()
        .sets
        .insert(("source".into(), "source-calendar".into()), verified);
    assert!(
        repair_unresolved_occurrence(&state, &child.id, Some(&backends),)
            .await
            .unwrap()
    );
    assert_eq!(
        store::owner_id(&state.db.reader(), &child.id).unwrap(),
        "source-event"
    );
    let page = list_occurrences(
        &state,
        "source".into(),
        None,
        "2026-09-14".into(),
        "2026-09-22".into(),
        20,
    )
    .await
    .unwrap();
    assert!(page.unresolved.is_empty());
    assert!(page.occurrences.iter().any(|item| {
        item.event_id == "source-event"
            && item.selection.original_start.as_deref() == Some("2026-09-15")
            && item.fields.start_time == "2026-09-19"
    }));
    assert!(
        !repair_unresolved_occurrence(&state, &child.id, Some(&backends))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn dirty_graph_set_remains_coherent_for_display_only() {
    let (_directory, state, operation, _) = fixture(Some("graph"), None, false).await;
    {
        let conn = state.db.writer().await;
        db::calendar::upsert_event_by_remote_id(&conn, &operation.source.anchor).unwrap();
    }

    let page = list_occurrences(
        &state,
        "source".into(),
        None,
        "2026-09-14".into(),
        "2026-09-20".into(),
        10,
    )
    .await
    .unwrap();

    assert_eq!(page.occurrences.len(), 3);
    assert_eq!(page.needs_hydration, ["source-event"]);
    assert!(store::latest_snapshot(&state.db.reader(), "source-event")
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn atomic_occurrence_plan_rehydrates_after_intermediate_sync() {
    let (_directory, state, operation, remote) = fixture(Some("jmap"), None, false).await;
    {
        let conn = state.db.writer().await;
        db::calendar::upsert_event_by_remote_id(&conn, &operation.source.anchor).unwrap();
    }
    assert!(store::load_snapshot(
        &state.db.reader(),
        &operation.source.token,
        &operation.source.anchor.id,
    )
    .is_err());
    let expected =
        event_fields(&selected_event(&operation.source.set, Some("2026-09-14")).unwrap());
    let backend = Fake {
        protocol: "jmap",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 1] = [&backend];

    let plan = plan_occurrence_with_backends(
        &state,
        "source-event".into(),
        "2026-09-14".into(),
        expected,
        CalendarEdit {
            title: Some("Atomic edit".into()),
            ..CalendarEdit::default()
        },
        Some(&backends),
    )
    .await
    .unwrap();

    let planned = store::load_operation(&state.db.reader(), &plan.operation_id).unwrap();
    assert_eq!(planned.stage, CalendarActionStage::Planned);
    assert!(planned.auto_resume);
    assert_eq!(planned.input.scope, RecurrenceMutationScope::ThisOccurrence);
    assert_eq!(
        planned.input.selection.original_start.as_deref(),
        Some("2026-09-14")
    );
    assert_eq!(planned.desired.overrides.len(), 2);
    let claims: i64 = state
        .db
        .reader()
        .query_row(
            "SELECT COUNT(*) FROM calendar_action_claims WHERE operation_id = ?1",
            [&plan.operation_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(claims, planned.source.members.len() as i64);
    assert_eq!(remote.lock().unwrap().fetched_ids.len(), 1);
    assert_eq!(remote.lock().unwrap().update_count, 0);

    let result = execute_with_backends(
        &state,
        &plan.operation_id,
        &CalendarConfirmations::default(),
        Some(&backends),
    )
    .await
    .unwrap();
    assert_eq!(result.stage, CalendarActionStage::Completed);
    assert_eq!(remote.lock().unwrap().fetched_ids.len(), 1);
    assert_eq!(remote.lock().unwrap().update_count, 1);
}

#[tokio::test]
async fn atomic_occurrence_plan_rejects_stale_display_before_claiming() {
    let (_directory, state, operation, remote) = fixture(Some("jmap"), None, false).await;
    let mut expected =
        event_fields(&selected_event(&operation.source.set, Some("2026-09-14")).unwrap());
    expected.title = "Stale title".into();
    let backend = Fake {
        protocol: "jmap",
        remote,
    };
    let backends: [&dyn CalendarBackend; 1] = [&backend];
    let before: i64 = state
        .db
        .reader()
        .query_row(
            "SELECT COUNT(*) FROM calendar_action_operations",
            [],
            |row| row.get(0),
        )
        .unwrap();

    let error = plan_occurrence_with_backends(
        &state,
        "source-event".into(),
        "2026-09-14".into(),
        expected,
        CalendarEdit {
            title: Some("Unsafe overwrite".into()),
            ..CalendarEdit::default()
        },
        Some(&backends),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("changed remotely"));
    let after: i64 = state
        .db
        .reader()
        .query_row(
            "SELECT COUNT(*) FROM calendar_action_operations",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after, before);
}

#[tokio::test]
async fn completed_caldav_occurrence_survives_sync_and_legacy_restart() {
    let (_directory, state, operation, remote) = fixture(Some("caldav"), None, false).await;
    let expected =
        event_fields(&selected_event(&operation.source.set, Some("2026-09-14")).unwrap());
    let backend = Fake {
        protocol: "caldav",
        remote,
    };
    let backends: [&dyn CalendarBackend; 1] = [&backend];
    let plan = plan_occurrence_with_backends(
        &state,
        "source-event".into(),
        "2026-09-14".into(),
        expected,
        CalendarEdit {
            title: Some("Durable CalDAV edit".into()),
            ..CalendarEdit::default()
        },
        Some(&backends),
    )
    .await
    .unwrap();
    execute_with_backends(
        &state,
        &plan.operation_id,
        &CalendarConfirmations::default(),
        Some(&backends),
    )
    .await
    .unwrap();

    {
        let conn = state.db.writer().await;
        let stored: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM calendar_action_sets
                 WHERE event_id = 'source-event' AND dirty = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, 1);
        db::calendar::upsert_event_by_remote_id(&conn, &operation.source.anchor).unwrap();
    }
    let page = list_occurrences(
        &state,
        "source".into(),
        None,
        "2026-09-14".into(),
        "2026-09-20".into(),
        10,
    )
    .await
    .unwrap();
    assert!(page.occurrences.iter().any(|occurrence| {
        occurrence.selection.original_start.as_deref() == Some("2026-09-14")
            && occurrence.fields.title == "Durable CalDAV edit"
    }));
    assert_eq!(page.needs_hydration, ["source-event"]);

    {
        let conn = state.db.writer().await;
        conn.execute(
            "DELETE FROM calendar_action_sets WHERE event_id = 'source-event'",
            [],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM calendar_action_snapshots WHERE event_id = 'source-event'",
            [],
        )
        .unwrap();
    }
    let recovered = list_occurrences(
        &state,
        "source".into(),
        None,
        "2026-09-14".into(),
        "2026-09-20".into(),
        10,
    )
    .await
    .unwrap();
    assert!(recovered.occurrences.iter().any(|occurrence| {
        occurrence.selection.original_start.as_deref() == Some("2026-09-14")
            && occurrence.fields.title == "Durable CalDAV edit"
    }));
    assert_eq!(recovered.needs_hydration, ["source-event"]);
}

#[tokio::test]
async fn remote_creation_lost_response_has_no_deferred_row_and_recovers_after_restart() {
    let (directory, state, _, remote) = fixture(Some("jmap"), Some("caldav"), false).await;
    remote.lock().unwrap().lose_create_response = true;
    let backend = Fake {
        protocol: "caldav",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 1] = [&backend];
    let id = uuid::Uuid::new_v4().to_string();
    assert!(
        create_with_backends(&state, new_input(), &id, Some(&backends))
            .await
            .is_err()
    );
    assert!(db::calendar::list_events(
        &state.db.reader(),
        "target",
        None,
        "2026-01-01",
        "2027-01-01"
    )
    .unwrap()
    .is_empty());
    assert!(store::creation_data(&state.db.reader(), &id)
        .unwrap()
        .is_some());
    drop(state);
    let state = AppState::new(directory.path().to_path_buf()).unwrap();
    let created = create_with_backends(&state, new_input(), &id, Some(&backends))
        .await
        .unwrap();
    assert_eq!(created.stage, CalendarActionStage::Completed);
    assert_eq!(remote.lock().unwrap().create_count, 1);
    let event = db::calendar::get_event(&state.db.reader(), &created.event_id).unwrap();
    assert_eq!(event.remote_id.as_deref(), Some(id.as_str()));
}

#[tokio::test]
async fn local_creation_claims_exact_pending_binding_and_preserves_invitation_proof() {
    let (_directory, state, _, _) = fixture(None, None, false).await;
    let lifecycle_id = uuid::Uuid::new_v4().to_string();
    {
        let conn = state.db.writer().await;
        db::service_bindings::insert(
            &conn,
            &db::service_bindings::ServiceBinding {
                id: "target-meet".into(),
                account_id: "target".into(),
                service: "meet".into(),
                protocol: "zoom".into(),
                enabled: true,
                sync_interval_seconds: None,
                config_json: "{}".into(),
            },
        )
        .unwrap();
        db::meet_pending_meetings::insert(
            &conn,
            &db::meet_pending_meetings::PendingMeeting {
                lifecycle_id: lifecycle_id.clone(),
                account_id: "target".into(),
                protocol: "zoom".into(),
                meeting_id: "pending-meeting".into(),
                join_url: "https://example.test/pending".into(),
                created_at: chrono::Utc::now().to_rfc3339(),
                cleanup_requested: false,
            },
        )
        .unwrap();
    }
    let mut input = new_input();
    input.meet_binding = Some(super::super::calendar::MeetBindingInput {
        lifecycle_id: lifecycle_id.clone(),
        account_id: "target".into(),
        protocol: "zoom".into(),
        meeting_id: "pending-meeting".into(),
        join_url: "https://example.test/pending".into(),
    });
    let created = create_completed(&state, input, &uuid::Uuid::new_v4().to_string())
        .await
        .unwrap();
    let conn = state.db.reader();
    assert!(db::meet_pending_meetings::get(&conn, &lifecycle_id)
        .unwrap()
        .is_none());
    assert_eq!(
        db::meet_meetings::get(&conn, &created.event_id)
            .unwrap()
            .unwrap()
            .meeting_id,
        "pending-meeting"
    );
    let event = db::calendar::get_event(&conn, &created.event_id).unwrap();
    assert_eq!(
        db::calendar_invitation::validated_series_rule(&conn, &event).unwrap(),
        "FREQ=WEEKLY;COUNT=3"
    );
}

#[tokio::test]
async fn concurrent_actions_cannot_both_accept_the_same_local_revision() {
    let (_directory, state, mut first, _) = fixture(None, None, false).await;
    first.destination = None;
    first.input.destination_calendar_id = None;
    first.desired.event.title = "First".into();
    checkpoint(&state, &first).await.unwrap();
    let mut second = first.clone();
    second.id = uuid::Uuid::new_v4().to_string();
    second.desired.event.title = "Second".into();
    {
        let mut conn = state.db.writer().await;
        let tx = conn.transaction().unwrap();
        store::insert_operation(&tx, &second).unwrap();
        tx.commit().unwrap();
    }
    let confirmations = CalendarConfirmations::default();
    let (left, right) = tokio::join!(
        execute(&state, &first.id, &confirmations),
        execute(&state, &second.id, &confirmations)
    );
    assert_ne!(left.is_ok(), right.is_ok());
}

async fn expanded_fixture(
    protocol: &'static str,
) -> (
    tempfile::TempDir,
    AppState,
    Arc<Mutex<Remote>>,
    Vec<(
        CalendarEvent,
        crate::calendar::recurrence_identity::RecurrenceIdentitySeed,
    )>,
) {
    use crate::calendar::recurrence_identity::{RecurrenceIdentitySeed, RecurrenceValueType};
    let (directory, state, operation, remote) =
        fixture(Some(protocol), Some(protocol), false).await;
    let mut conn = state.db.writer().await;
    let tx = conn.transaction().unwrap();
    tx.execute_batch("DELETE FROM calendar_action_operations; DELETE FROM calendar_action_sets; DELETE FROM calendar_action_addresses; DELETE FROM calendar_recurrence_objects;").unwrap();
    let mut rows = Vec::new();
    for (id, position, effective) in [
        ("source-event", "2026-09-14", "2026-09-14"),
        ("second-child", "2026-09-15", "2026-09-15"),
        ("outside-child", "2026-09-16", "2027-01-01"),
    ] {
        let mut event = operation.source.set.event.clone();
        event.id = id.into();
        event.remote_id = Some(format!("native-{id}"));
        event.recurrence_kind = RecurrenceKind::Occurrence;
        event.recurrence_rule = None;
        event.start_time = effective.into();
        event.end_time = (chrono::NaiveDate::parse_from_str(effective, "%Y-%m-%d").unwrap()
            + chrono::Duration::days(2))
        .to_string();
        if id == "source-event" {
            db::calendar::update_event(&tx, &event).unwrap();
        } else {
            db::calendar::insert_event(&tx, &event).unwrap();
        }
        let seed = RecurrenceIdentitySeed {
            local_series_event_id: None,
            provider_calendar_id: Some("source-calendar".into()),
            provider_series_id: Some("remote-source".into()),
            provider_occurrence_id: event.remote_id.clone(),
            recurrence_id: Some(position.into()),
            recurrence_value_type: Some(RecurrenceValueType::Date),
            recurrence_timezone: None,
            occurrence: event_fields(&event),
            provider_native_data: Some("native child".into()),
            provider_revision: Some("before".into()),
            kind: if id == "outside-child" {
                RecurrenceObjectKind::Exception
            } else {
                RecurrenceObjectKind::Occurrence
            },
        };
        db::calendar_recurrence::upsert(
            &tx,
            &seed
                .clone()
                .bind("source", id, format!("object-{id}"))
                .unwrap(),
        )
        .unwrap();
        rows.push((event, seed));
    }
    db::calendar_invitation_source::record(
        &tx,
        "source-event",
        &db::calendar_invitation_source::InvitationSource {
            source_account_id: "source".into(),
            source_message_id: "message".into(),
            invitation_uid: "invitation-uid".into(),
        },
    )
    .unwrap();
    tx.execute_batch("INSERT INTO calendars(id, account_id, name, remote_id) VALUES ('other-calendar', 'source', 'Other', 'other-native-calendar');").unwrap();
    for (id, account, calendar) in [
        ("unrelated-calendar", "source", "other-calendar"),
        ("unrelated-account", "target", "target-calendar"),
    ] {
        let mut event = rows[0].0.clone();
        event.id = id.into();
        event.account_id = account.into();
        event.calendar_id = calendar.into();
        db::calendar::insert_event(&tx, &event).unwrap();
    }
    tx.commit().unwrap();
    drop(conn);
    {
        let mut data = remote.lock().unwrap();
        let set = data
            .sets
            .get_mut(&("source".into(), "source-calendar".into()))
            .unwrap();
        set.overrides.push(CalendarOverride {
            original_start: "2026-09-16".into(),
            event: Some(rows[2].0.clone()),
            native: Some(NativeCalendarResource {
                protocol: protocol.into(),
                calendar_id: "source-calendar".into(),
                event_id: "native-outside-child".into(),
                revision: Some("before".into()),
                data: native_data(protocol, &rows[2].0, "text/plain"),
            }),
        });
        refresh_native(set, protocol);
    }
    (directory, state, remote, rows)
}

#[tokio::test]
async fn expanded_children_hydrate_plan_edit_and_survive_repeated_sync_without_duplicate_sets() {
    for protocol in ["google", "graph", "jmap", "caldav"] {
        for scope in [
            RecurrenceMutationScope::ThisOccurrence,
            RecurrenceMutationScope::EntireSeries,
        ] {
            for key in ["2026-09-14", "2026-09-16"] {
                let (_dir, state, remote, rows) = expanded_fixture(protocol).await;
                let backend = Fake {
                    protocol,
                    remote: remote.clone(),
                };
                let backends: [&dyn CalendarBackend; 1] = [&backend];
                let source = hydrate_with_backends(&state, "source-event", Some(&backends))
                    .await
                    .unwrap();
                assert_eq!(
                    source.anchor.remote_id.as_deref(),
                    Some("native-source-event")
                );
                assert_eq!(
                    source.set.native.as_ref().unwrap().event_id,
                    "remote-source"
                );
                assert_eq!(source.members.len(), 3);
                store::save_snapshot(&*state.db.writer().await, &source).unwrap();
                let plan = plan_with_backends(
                    &state,
                    CalendarActionInput {
                        selection: CalendarSelection {
                            event_id: "source-event".into(),
                            token: source.token,
                            original_start: Some(key.into()),
                        },
                        scope,
                        edit: CalendarEdit {
                            title: Some("Edited".into()),
                            ..Default::default()
                        },
                        destination_calendar_id: None,
                        reset_exceptions: false,
                    },
                    Some(&backends),
                )
                .await
                .unwrap();
                execute_with_backends(
                    &state,
                    &plan.operation_id,
                    &CalendarConfirmations::default(),
                    Some(&backends),
                )
                .await
                .unwrap();
                let owner = store::owner_id(&state.db.reader(), "source-event").unwrap();
                assert_ne!(owner, "source-event");
                let conn = state.db.reader();
                assert_eq!(
                    db::calendar::get_event(&conn, &owner)
                        .unwrap()
                        .remote_id
                        .as_deref(),
                    Some("remote-source")
                );
                assert_eq!(
                    db::calendar::get_event(&conn, "source-event")
                        .unwrap()
                        .recurrence_kind,
                    RecurrenceKind::Occurrence
                );
                assert!(db::meet_meetings::get(&conn, "source-event")
                    .unwrap()
                    .is_some());
                assert!(db::calendar_invitation_source::get(&conn, "source-event")
                    .unwrap()
                    .is_some());
                for (event, _) in &rows {
                    assert!(db::calendar::get_event(&conn, &event.id).is_ok());
                    assert!(db::calendar_recurrence::get_by_object_id(
                        &conn,
                        &format!("object-{}", event.id)
                    )
                    .unwrap()
                    .is_none());
                }
                assert_eq!(
                    db::calendar::get_event(&conn, "unrelated-calendar")
                        .unwrap()
                        .remote_id
                        .as_deref(),
                    Some("native-source-event")
                );
                assert_eq!(
                    db::calendar::get_event(&conn, "unrelated-account")
                        .unwrap()
                        .remote_id
                        .as_deref(),
                    Some("native-source-event")
                );
                drop(conn);
                let page = list_occurrences(
                    &state,
                    "source".into(),
                    Some("source-calendar".into()),
                    "2026-09-14".into(),
                    "2026-09-20".into(),
                    20,
                )
                .await
                .unwrap();
                assert_eq!(page.occurrences.len(), 2, "{protocol} {scope:?}");
                for _ in 0..3 {
                    let conn = state.db.writer().await;
                    for (event, seed) in &rows {
                        db::calendar::upsert_event_by_remote_id_with_recurrence(
                            &conn,
                            event,
                            std::slice::from_ref(seed),
                        )
                        .unwrap();
                    }
                    let (mut unseen, mut seed) = rows[1].clone();
                    unseen.id = "unseen-child".into();
                    unseen.remote_id = Some("native-unseen-child".into());
                    seed.provider_occurrence_id = unseen.remote_id.clone();
                    seed.recurrence_id = Some("2026-09-17".into());
                    db::calendar::upsert_event_by_remote_id_with_recurrence(
                        &conn,
                        &unseen,
                        &[seed],
                    )
                    .unwrap();
                    assert!(db::calendar::get_event(&conn, "unseen-child").is_err());
                    assert_eq!(
                        db::calendar::list_events(
                            &conn,
                            "source",
                            Some("source-calendar"),
                            "2026-01-01",
                            "2027-12-31"
                        )
                        .unwrap()
                        .len(),
                        1
                    );
                }
                let refreshed = hydrate_with_backends(&state, "source-event", Some(&backends))
                    .await
                    .unwrap();
                assert_eq!(
                    refreshed.set.event.remote_id.as_deref(),
                    Some("remote-source")
                );
                assert_eq!(
                    remote
                        .lock()
                        .unwrap()
                        .fetched_ids
                        .last()
                        .unwrap()
                        .as_deref(),
                    Some("remote-source")
                );
                let view = read_event_set(
                    &state,
                    "source-event",
                    "2026-09-14",
                    "2026-09-20",
                    20,
                    Some(&backends),
                )
                .await
                .unwrap();
                assert_eq!(view.master.event_id, "source-event");
                assert_eq!(view.page.occurrences.len(), 2);
                let page = list_occurrences(
                    &state,
                    "source".into(),
                    Some("source-calendar".into()),
                    "2026-09-14".into(),
                    "2026-09-20".into(),
                    20,
                )
                .await
                .unwrap();
                assert_eq!(page.occurrences.len(), 2);
                assert!(page.needs_hydration.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn expanded_whole_and_occurrence_moves_retire_all_source_addresses() {
    for protocol in ["google", "graph", "jmap", "caldav"] {
        for scope in [
            RecurrenceMutationScope::ThisOccurrence,
            RecurrenceMutationScope::EntireSeries,
        ] {
            let (_dir, state, remote, rows) = expanded_fixture(protocol).await;
            let backend = Fake {
                protocol,
                remote: remote.clone(),
            };
            let backends: [&dyn CalendarBackend; 1] = [&backend];
            let source = hydrate_with_backends(&state, "source-event", Some(&backends))
                .await
                .unwrap();
            store::save_snapshot(&*state.db.writer().await, &source).unwrap();
            let fetches_before_plan = remote.lock().unwrap().fetched_ids.len();
            let plan = plan_with_backends(
                &state,
                CalendarActionInput {
                    selection: CalendarSelection {
                        event_id: "source-event".into(),
                        token: source.token,
                        original_start: Some("2026-09-16".into()),
                    },
                    scope,
                    edit: CalendarEdit::default(),
                    destination_calendar_id: Some("target-calendar".into()),
                    reset_exceptions: false,
                },
                Some(&backends),
            )
            .await
            .unwrap();
            assert_eq!(
                remote.lock().unwrap().fetched_ids.len(),
                fetches_before_plan,
                "{protocol} {scope:?}"
            );
            let outcome = execute_with_backends(
                &state,
                &plan.operation_id,
                &CalendarConfirmations::default(),
                Some(&backends),
            )
            .await
            .unwrap();
            assert_eq!(outcome.stage, CalendarActionStage::Completed);
            let conn = state.db.writer().await;
            for _ in 0..3 {
                for (event, seed) in &rows {
                    db::calendar::upsert_event_by_remote_id_with_recurrence(
                        &conn,
                        event,
                        std::slice::from_ref(seed),
                    )
                    .unwrap();
                }
                let (mut unseen, mut seed) = rows[1].clone();
                unseen.id = "unseen-child".into();
                unseen.remote_id = Some("native-unseen-child".into());
                seed.provider_occurrence_id = unseen.remote_id.clone();
                seed.recurrence_id = Some("2026-09-17".into());
                db::calendar::upsert_event_by_remote_id_with_recurrence(&conn, &unseen, &[seed])
                    .unwrap();
                assert!(db::calendar::get_event(&conn, "unseen-child").is_err());
            }
            let source_events = db::calendar::list_events(
                &conn,
                "source",
                Some("source-calendar"),
                "2026-01-01",
                "2027-12-31",
            )
            .unwrap();
            assert_eq!(
                source_events.len(),
                usize::from(scope == RecurrenceMutationScope::ThisOccurrence)
            );
            assert!(db::meet_meetings::get(&conn, "source-event")
                .unwrap()
                .is_some());
            assert!(db::calendar_invitation_source::get(&conn, "source-event")
                .unwrap()
                .is_some());
            assert!(db::meet_pending_meetings::list(&conn).unwrap().is_empty());
            assert_eq!(
                db::calendar::list_events(
                    &conn,
                    "target",
                    Some("target-calendar"),
                    "2026-01-01",
                    "2027-12-31"
                )
                .unwrap()
                .len(),
                2
            );
        }
    }
}

#[tokio::test]
async fn update_response_loss_reconciles_without_reapplying_provider_write() {
    for protocol in ["google", "graph", "jmap", "caldav"] {
        let (_directory, state, mut operation, remote) =
            fixture(Some(protocol), Some(protocol), false).await;
        operation.destination = None;
        operation.input.destination_calendar_id = None;
        operation.desired.mark_description_plain(None).unwrap();
        operation.desired.event.title = "Recovered edit".into();
        operation.desired.event.description =
            Some("Updated <agenda> & notes; one, two\\three\nNext".into());
        checkpoint(&state, &operation).await.unwrap();
        remote.lock().unwrap().lose_update_response = true;
        let backend = Fake {
            protocol,
            remote: remote.clone(),
        };
        let backends: [&dyn CalendarBackend; 1] = [&backend];

        let result = execute_with_backends(
            &state,
            &operation.id,
            &CalendarConfirmations::default(),
            Some(&backends),
        )
        .await
        .unwrap();

        assert_eq!(result.stage, CalendarActionStage::Completed);
        assert_eq!(remote.lock().unwrap().update_count, 1);
        assert_eq!(
            db::calendar::get_event(&state.db.reader(), "source-event")
                .unwrap()
                .title,
            "Recovered edit"
        );
    }
}

#[tokio::test]
async fn applying_update_recovers_after_authoritative_read_projects_remote_result() {
    let (_directory, state, mut operation, remote) =
        fixture(Some("graph"), Some("graph"), false).await;
    operation.destination = None;
    operation.input.destination_calendar_id = None;
    operation.desired.event.title = "Recovered edit".into();
    operation.stage = CalendarActionStage::Applying;
    {
        let mut conn = state.db.writer().await;
        let tx = conn.transaction().unwrap();
        store::claim_operation(&tx, &operation).unwrap();
        store::save_operation(&tx, &operation).unwrap();
        tx.commit().unwrap();
    }
    {
        let mut canonical = operation.desired.clone();
        canonical.native.as_mut().unwrap().revision = Some("updated".into());
        canonical.event.etag = Some("updated".into());
        refresh_native(&mut canonical, "graph");
        remote
            .lock()
            .unwrap()
            .sets
            .insert(("source".into(), "source-calendar".into()), canonical);
    }
    let backend = Fake {
        protocol: "graph",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 1] = [&backend];

    let result = execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends),
    )
    .await
    .unwrap();

    assert_eq!(result.stage, CalendarActionStage::Completed);
    assert_eq!(remote.lock().unwrap().update_count, 0);
    assert_eq!(
        db::calendar::get_event(&state.db.reader(), "source-event")
            .unwrap()
            .title,
        "Recovered edit"
    );
}

#[tokio::test]
async fn pending_update_repairs_unchanged_description_provenance() {
    let (_directory, _state, mut operation, _remote) =
        fixture(Some("graph"), Some("graph"), false).await;
    operation.destination = None;
    operation.input.destination_calendar_id = None;
    operation.input.scope = RecurrenceMutationScope::ThisOccurrence;
    operation.source.set.event.description = Some("<b>Rich agenda</b>".into());
    operation.source.set.native.as_mut().unwrap().data =
        native_data("graph", &operation.source.set.event, "text/html");
    operation.input.edit.description = operation.source.set.event.description.clone();
    operation.desired = desired_set(&operation.source.set, &operation.input).unwrap();
    operation
        .desired
        .mark_description_plain(operation.input.selection.original_start.as_deref())
        .unwrap();
    let corrected = desired_set(&operation.source.set, &operation.input).unwrap();

    assert!(!semantic_eq(&operation.desired, &corrected));
    assert!(repair_unchanged_description_intent(&mut operation).unwrap());
    assert!(semantic_eq(&operation.desired, &corrected));
    assert!(!repair_unchanged_description_intent(&mut operation).unwrap());
}

#[tokio::test]
async fn applying_update_rebases_unchanged_remote_source_before_retry() {
    let (_directory, state, mut operation, remote) =
        fixture(Some("graph"), Some("graph"), false).await;
    operation.destination = None;
    operation.input.destination_calendar_id = None;
    operation.desired.event.title = "Retried edit".into();
    operation.stage = CalendarActionStage::Applying;
    {
        let mut conn = state.db.writer().await;
        let tx = conn.transaction().unwrap();
        store::claim_operation(&tx, &operation).unwrap();
        store::save_operation(&tx, &operation).unwrap();
        tx.commit().unwrap();
    }
    let backend = Fake {
        protocol: "graph",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 1] = [&backend];

    let result = execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends),
    )
    .await
    .unwrap();

    assert_eq!(result.stage, CalendarActionStage::Completed);
    assert_eq!(remote.lock().unwrap().update_count, 1);
    assert_eq!(
        db::calendar::get_event(&state.db.reader(), "source-event")
            .unwrap()
            .title,
        "Retried edit"
    );
}

#[tokio::test]
async fn applying_update_rejects_unrelated_remote_change_without_write() {
    let (_directory, state, mut operation, remote) =
        fixture(Some("graph"), Some("graph"), false).await;
    operation.destination = None;
    operation.input.destination_calendar_id = None;
    operation.desired.event.title = "Unsafe overwrite".into();
    operation.stage = CalendarActionStage::Applying;
    {
        let mut conn = state.db.writer().await;
        let tx = conn.transaction().unwrap();
        store::claim_operation(&tx, &operation).unwrap();
        store::save_operation(&tx, &operation).unwrap();
        tx.commit().unwrap();
    }
    {
        let mut data = remote.lock().unwrap();
        let current = data
            .sets
            .get_mut(&("source".into(), "source-calendar".into()))
            .unwrap();
        current.event.title = "Unrelated remote change".into();
        current.event.etag = Some("conflict".into());
        current.native.as_mut().unwrap().revision = Some("conflict".into());
        refresh_native(current, "graph");
    }
    let backend = Fake {
        protocol: "graph",
        remote: remote.clone(),
    };
    let backends: [&dyn CalendarBackend; 1] = [&backend];

    let error = execute_with_backends(
        &state,
        &operation.id,
        &CalendarConfirmations::default(),
        Some(&backends),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("selection is stale"));
    assert_eq!(remote.lock().unwrap().update_count, 0);
}

#[tokio::test]
async fn local_commit_failure_resumes_without_reapplying_provider_write() {
    for protocol in ["google", "graph", "jmap", "caldav"] {
        let (directory, state, mut operation, remote) =
            fixture(Some(protocol), Some(protocol), false).await;
        operation.destination = None;
        operation.input.destination_calendar_id = None;
        operation.desired.mark_description_plain(None).unwrap();
        operation.desired.event.title = "Recovered edit".into();
        operation.desired.event.description =
            Some("Updated <agenda> & notes; one, two\\three\nNext".into());
        checkpoint(&state, &operation).await.unwrap();
        let backend = Fake {
            protocol,
            remote: remote.clone(),
        };
        let backends: [&dyn CalendarBackend; 1] = [&backend];
        state.db.writer().await.execute_batch("CREATE TRIGGER fail_action_commit BEFORE UPDATE ON calendar_events BEGIN SELECT RAISE(ABORT, 'simulated commit failure'); END;").unwrap();

        assert!(execute_with_backends(
            &state,
            &operation.id,
            &CalendarConfirmations::default(),
            Some(&backends)
        )
        .await
        .is_err());
        assert_eq!(remote.lock().unwrap().update_count, 1);
        {
            let data = remote.lock().unwrap();
            let canonical = &data.sets[&("source".into(), "source-calendar".into())];
            assert_native_projections(canonical);
            assert!(semantic_eq(canonical, &operation.desired), "{protocol}");
        }
        assert_eq!(
            db::calendar::get_event(&state.db.reader(), "source-event")
                .unwrap()
                .title,
            "Standup"
        );
        state
            .db
            .writer()
            .await
            .execute_batch("DROP TRIGGER fail_action_commit;")
            .unwrap();
        {
            let conn = state.db.writer().await;
            let canonical = remote
                .lock()
                .unwrap()
                .sets
                .get(&("source".into(), "source-calendar".into()))
                .unwrap()
                .event
                .clone();
            db::calendar::upsert_event_by_remote_id(&conn, &canonical).unwrap();
            let mut unrelated = operation.source.anchor.clone();
            unrelated.id = "unrelated-sync-event".into();
            unrelated.remote_id = Some("unrelated-native-id".into());
            db::calendar::insert_event(&conn, &unrelated).unwrap();
        }
        drop(state);
        let state = AppState::new(directory.path().to_path_buf()).unwrap();
        let result = execute_with_backends(
            &state,
            &operation.id,
            &CalendarConfirmations::default(),
            Some(&backends),
        )
        .await
        .unwrap();
        assert_eq!(result.stage, CalendarActionStage::Completed);
        assert_eq!(remote.lock().unwrap().update_count, 1);
        assert_eq!(
            db::calendar::get_event(&state.db.reader(), "source-event")
                .unwrap()
                .title,
            "Recovered edit"
        );
    }
}

#[tokio::test]
async fn recurring_creation_completes_for_every_provider_protocol() {
    for protocol in ["google", "graph", "jmap", "caldav"] {
        let (_directory, state, _, remote) = fixture(Some(protocol), Some(protocol), false).await;
        let backend = Fake {
            protocol,
            remote: remote.clone(),
        };
        let backends: [&dyn CalendarBackend; 1] = [&backend];
        let id = uuid::Uuid::new_v4().to_string();
        let mut input = new_input();
        input.description = Some("Created <body> & notes; one, two\\three\nNext".into());
        input.attendees.push(crate::calendar::Attendee {
            email: "guest@example.test".into(),
            name: Some("Guest".into()),
            status: "needs-action".into(),
            is_self: None,
        });
        let outcome = create_with_backends(&state, input, &id, Some(&backends))
            .await
            .unwrap();
        assert_eq!(outcome.stage, CalendarActionStage::Completed);
        let event = db::calendar::get_event(&state.db.reader(), &outcome.event_id).unwrap();
        assert_eq!(event.recurrence_kind, RecurrenceKind::Series);
        assert_eq!(event.remote_id.as_deref(), Some(id.as_str()));
        assert_eq!(remote.lock().unwrap().create_count, 1);
    }
}
