use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy, Default)]
enum Fault {
    #[default]
    None,
    LostCreate,
    Conflict,
    PartialSet,
    MissingCanonical,
    WrongAccount,
    WrongState,
    MissingResult,
    MissingOverride,
}

#[derive(Default)]
struct Store {
    objects: BTreeMap<String, Value>,
    revision: usize,
    requests: Vec<Value>,
    fault: Fault,
    wrote: bool,
}

struct Server {
    root: String,
    store: Arc<Mutex<Store>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn start(objects: Vec<Value>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let store = Arc::new(Mutex::new(Store {
            objects: objects
                .into_iter()
                .map(|object| (object["id"].as_str().unwrap().into(), object))
                .collect(),
            ..Store::default()
        }));
        let captured = store.clone();
        let base = root.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
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
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let response = if headers.starts_with("GET ") {
                    Some(json!({"apiUrl": format!("{base}/api"),
                        "downloadUrl": format!("{base}/download"), "uploadUrl": format!("{base}/upload"),
                        "primaryAccounts": {"urn:ietf:params:jmap:mail": "mail-account", "urn:ietf:params:jmap:calendars": "calendar-account"},
                        "accounts": {"mail-account": {"accountCapabilities": {"urn:ietf:params:jmap:mail": {}}},
                            "calendar-account": {"accountCapabilities": {"urn:ietf:params:jmap:calendars": {}}}},
                        "capabilities": {"urn:ietf:params:jmap:core": {"maxObjectsInGet": 2, "maxObjectsInSet": 20}}}))
                } else {
                    let request: Value =
                        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                    respond(&mut captured.lock().unwrap(), request)
                };
                if let Some(response) = response {
                    let body = response.to_string();
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
            }
        });
        Self { root, store, task }
    }

    fn services(&self) -> crate::provider::ProviderServices {
        let mut services = crate::provider::ProviderServices::production().unwrap();
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        services.transports.jmap_discovery_http = http.clone();
        services.transports.jmap_api_http = http;
        services
    }

    fn account(&self) -> crate::db::accounts::AccountFull {
        let mut account = crate::backend::testutil::account("calendar", "jmap");
        account.jmap_url = self.root.clone();
        account.jmap_auth_method = "basic".into();
        account
    }

    async fn connection(&self) -> (JmapConfig, JmapConnection) {
        let (config, mut connection) = self.services().jmap_client(&self.account()).await.unwrap();
        connection.select_calendar_account(&config).await.unwrap();
        (config, connection)
    }

    fn object(&self, id: &str) -> Value {
        self.store.lock().unwrap().objects[id].clone()
    }

    fn writes(&self) -> Vec<Value> {
        self.store
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|request| request["methodCalls"][0][0] == "CalendarEvent/set")
            .cloned()
            .collect()
    }
}

fn respond(store: &mut Store, request: Value) -> Option<Value> {
    store.requests.push(request.clone());
    let call = &request["methodCalls"][0];
    let args = &call[1];
    assert_eq!(
        args["accountId"], "calendar-account",
        "mail primary must not scope calendar writes"
    );
    let mut method = call[0].as_str().unwrap();
    let revision = format!("data-{}", store.revision);
    let mut body = match method {
        "CalendarEvent/query" => {
            let position = args["position"].as_u64().unwrap() as usize;
            let limit = args["limit"].as_u64().unwrap() as usize;
            json!({"accountId": "calendar-account", "queryState": format!("query-{}", store.revision),
                "position": position, "total": store.objects.len(),
                "ids": store.objects.keys().skip(position).take(limit).collect::<Vec<_>>()})
        }
        "CalendarEvent/get" => {
            let ids = args["ids"].as_array().unwrap();
            let mut list = ids
                .iter()
                .filter_map(|id| store.objects.get(id.as_str().unwrap()))
                .cloned()
                .collect::<Vec<_>>();
            if store.wrote && matches!(store.fault, Fault::MissingCanonical) {
                list.clear();
            }
            if store.wrote && matches!(store.fault, Fault::MissingOverride) {
                for event in &mut list {
                    event.as_object_mut().unwrap().remove("recurrenceOverrides");
                }
            }
            json!({"accountId": "calendar-account", "state": if store.wrote && matches!(store.fault, Fault::WrongState) { "wrong-state" } else { &revision },
                "list": list, "notFound": ids.iter().filter(|id| !store.objects.contains_key(id.as_str().unwrap())).collect::<Vec<_>>()})
        }
        "CalendarEvent/set" => {
            if matches!(store.fault, Fault::Conflict) || args["ifInState"] != revision {
                method = "error";
                json!({"type": "stateMismatch"})
            } else {
                assert_eq!(args["sendSchedulingMessages"], false);
                let mut created = Map::new();
                let mut updated = Map::new();
                let mut destroyed = Vec::new();
                let mut rejected = Map::new();
                if let Some(create) = args["create"].as_object() {
                    for (key, object) in create {
                        let id = format!("created-{}", store.revision);
                        let mut object = object.clone();
                        assert!(object.get("id").is_none());
                        object["id"] = json!(id);
                        store.objects.insert(id.clone(), object);
                        created.insert(key.clone(), json!({"id": id}));
                    }
                }
                if let Some(update) = args["update"].as_object() {
                    for (index, (id, patch)) in update.iter().enumerate() {
                        if index > 0 && matches!(store.fault, Fault::PartialSet) {
                            rejected.insert(id.clone(), json!({"type": "forbidden"}));
                        } else {
                            let object = store.objects.get_mut(id).unwrap();
                            *object =
                                apply_patch_object(object, patch.as_object().unwrap()).unwrap();
                            updated.insert(id.clone(), Value::Null);
                        }
                    }
                }
                if let Some(destroy) = args["destroy"].as_array() {
                    for id in destroy {
                        store.objects.remove(id.as_str().unwrap());
                        destroyed.push(id.clone());
                    }
                }
                store.revision += 1;
                store.wrote = true;
                if matches!(store.fault, Fault::LostCreate) && !created.is_empty() {
                    store.fault = Fault::None;
                    return None;
                }
                if matches!(store.fault, Fault::MissingResult) {
                    updated.clear();
                    created.clear();
                    destroyed.clear();
                }
                json!({"accountId": "calendar-account", "oldState": revision, "newState": format!("data-{}", store.revision),
                    "created": created, "updated": updated, "destroyed": destroyed, "notUpdated": rejected})
            }
        }
        _ => panic!("unexpected method {method}"),
    };
    if matches!(store.fault, Fault::WrongAccount) {
        body["accountId"] = json!("wrong-account");
    }
    Some(json!({"methodResponses": [[method, body, call[2]]], "sessionState": "session-not-data"}))
}

fn master() -> Value {
    json!({"@type": "Event", "id": "master", "uid": "series-uid", "calendarIds": {"source": true},
        "title": "Planning", "description": "Agenda", "start": "2026-09-13T10:00:00", "duration": "PT1H", "timeZone": "Europe/Stockholm",
        "locations": {"room": {"@type": "Location", "name": "Room", "coordinates": "geo:1,2"}},
        "recurrenceRules": [{"@type": "RecurrenceRule", "frequency": "weekly", "count": 10}],
        "recurrenceOverrides": {"2026-09-20T10:00:00": {"start": "2026-09-20T12:00:00", "title": "Moved", "x-custom": {"retain": true}},
            "2026-09-27T10:00:00": {"excluded": true, "x-reason": "holiday"}},
        "participants": {"person": {"@type": "Participant", "calendarAddress": "mailto:owner@test.invalid", "roles": {"owner": true}, "x-custom": 42}},
        "alerts": {"alarm": {"action": "display"}}, "x-provider": {"secret": "complete-native"}})
}

fn template() -> CalendarEvent {
    CalendarEvent {
        remote_id: Some("master".into()),
        uid: Some("series-uid".into()),
        ..crate::backend::testutil::event()
    }
}

fn standalone() -> Value {
    let mut object = master();
    object.as_object_mut().unwrap().remove("recurrenceRules");
    object
        .as_object_mut()
        .unwrap()
        .remove("recurrenceOverrides");
    object
}

fn detached() -> Value {
    let mut object = standalone();
    object["id"] = json!("detached");
    object["baseEventId"] = json!("master");
    object["recurrenceId"] = json!("2026-10-04T10:00:00");
    object["start"] = json!("2026-10-04T12:00:00");
    object["title"] = json!("Detached");
    object
}

#[tokio::test]
async fn complete_native_read_has_finite_overrides_and_calendar_data_revision() {
    let server = Server::start(vec![master(), detached()]).await;
    let (config, connection) = server.connection().await;
    let set = connection
        .fetch_native_event_set(&config, &template(), "source")
        .await
        .unwrap();
    assert_eq!(set.overrides.len(), 3);
    assert_eq!(set.overrides[0].original_start, "2026-09-20T08:00:00Z");
    assert_eq!(
        set.overrides[0].event.as_ref().unwrap().start_time,
        "2026-09-20T10:00:00Z"
    );
    assert!(set.overrides[1].event.is_none());
    assert_eq!(
        set.overrides[2].native.as_ref().unwrap().event_id,
        "detached"
    );
    let native = set.native.unwrap();
    assert_eq!(native.revision.as_deref(), Some("data-0"));
    assert_eq!(unpack(&native, "calendar-account").unwrap(), master());
    assert!(unpack(&native, "mail-account").is_err());
    assert!(connection
        .fetch_native_event_set(&config, &template(), "unselected")
        .await
        .is_err());
    let mut instance = template();
    instance.remote_id = Some("detached".into());
    let from_instance = connection
        .fetch_native_event_set(&config, &instance, "source")
        .await
        .unwrap();
    assert_eq!(from_instance.event.remote_id.as_deref(), Some("master"));
    assert_eq!(from_instance.overrides.len(), 3);
}

#[tokio::test]
async fn generated_existing_and_detached_edits_preserve_native_fields() {
    let server = Server::start(vec![master(), detached()]).await;
    let (config, connection) = server.connection().await;
    let before = connection
        .fetch_native_event_set(&config, &template(), "source")
        .await
        .unwrap();
    let mut desired = before.clone();
    let fields = simple_recurrence::resolve(&before.event, "2026-10-11T08:00:00Z").unwrap();
    let mut generated = before.event.clone();
    apply_event_fields(&mut generated, &fields);
    generated.recurrence_rule = None;
    generated.recurrence_kind = RecurrenceKind::Occurrence;
    generated.title = "Generated edit".into();
    desired.overrides.push(CalendarOverride {
        original_start: "2026-10-11T08:00:00Z".into(),
        event: Some(generated),
        native: None,
    });
    desired.overrides[0].event.as_mut().unwrap().description = Some("Existing edit".into());
    desired.overrides[2].event.as_mut().unwrap().title = "Detached edit".into();
    let result = connection
        .update_native_event_set(&config, &before, &desired)
        .await
        .unwrap();
    assert_eq!(result.overrides.len(), 4);
    assert_eq!(result.native.unwrap().revision.as_deref(), Some("data-1"));
    let written = server.object("master");
    assert_eq!(written["x-provider"], master()["x-provider"]);
    assert_eq!(written["participants"], master()["participants"]);
    assert_eq!(
        written["recurrenceOverrides"]["2026-09-20T10:00:00"]["x-custom"],
        json!({"retain": true})
    );
    assert_eq!(
        written["recurrenceOverrides"]["2026-10-11T10:00:00"],
        json!({"title": "Generated edit"})
    );
    assert_eq!(server.object("detached")["title"], "Detached edit");
    assert_eq!(
        server.writes()[0]["methodCalls"][0][1]["ifInState"],
        "data-0"
    );
}

#[tokio::test]
async fn all_series_sparse_field_and_rule_changes_keep_exceptions_and_unknowns() {
    let server = Server::start(vec![master()]).await;
    let (config, connection) = server.connection().await;
    let before = connection
        .fetch_native_event_set(&config, &template(), "source")
        .await
        .unwrap();
    let mut desired = before.clone();
    desired.event.title = "Whole series".into();
    desired.event.recurrence_rule = Some("FREQ=WEEKLY;INTERVAL=2;COUNT=8".into());
    desired.event.location = Some("New room".into());
    desired.overrides[0].original_start = "2026-09-27T08:00:00Z".into();
    let moved = desired.overrides[0].event.as_mut().unwrap();
    moved.start_time = "2026-09-27T10:00:00Z".into();
    moved.end_time = "2026-09-27T11:00:00Z".into();
    moved.location = Some("New room".into());
    desired.overrides[1].original_start = "2026-10-11T08:00:00Z".into();
    let copy_payload = creation_object(&desired, "destination", "edited-copy").unwrap();
    assert_eq!(
        copy_payload["recurrenceOverrides"]["2026-09-27T10:00:00"]["x-custom"],
        json!({"retain": true})
    );
    assert_eq!(
        copy_payload["recurrenceOverrides"]["2026-10-11T10:00:00"]["x-reason"],
        "holiday"
    );
    let result = connection
        .update_native_event_set(&config, &before, &desired)
        .await
        .unwrap();
    assert_eq!(result.event.title, "Whole series");
    let written = server.object("master");
    assert_eq!(
        written["recurrenceOverrides"]["2026-09-27T10:00:00"]["x-custom"],
        json!({"retain": true})
    );
    assert_eq!(
        written["recurrenceOverrides"]["2026-10-11T10:00:00"],
        master()["recurrenceOverrides"]["2026-09-27T10:00:00"]
    );
    assert!(written["recurrenceOverrides"]
        .get("2026-09-20T10:00:00")
        .is_none());
    assert_eq!(written["locations"]["room"]["coordinates"], "geo:1,2");
    assert_eq!(written["recurrenceRules"][0]["interval"], 2);
    let writes = server.writes();
    let patch = &writes[0]["methodCalls"][0][1]["update"]["master"];
    assert!(patch.get("start").is_none());
    assert!(patch.get("participants").is_none());
    assert!(patch.get("recurrenceOverrides").is_some());
}

#[tokio::test]
async fn copies_keep_overrides_exclusions_extensions_and_new_identity() {
    let server = Server::start(vec![master(), detached()]).await;
    let (config, connection) = server.connection().await;
    let source = connection
        .fetch_native_event_set(&config, &template(), "source")
        .await
        .unwrap();
    let copied = connection
        .create_native_event_set(&config, "destination", &source, "operation-1")
        .await
        .unwrap();
    assert_ne!(copied.event.remote_id, source.event.remote_id);
    assert_ne!(copied.event.uid, source.event.uid);
    assert_eq!(copied.overrides.len(), 3);
    assert!(copied.overrides[1].event.is_none());
    let object = server.object(copied.event.remote_id.as_deref().unwrap());
    assert_eq!(object["calendarIds"], json!({"destination": true}));
    assert_eq!(object["x-provider"], master()["x-provider"]);
    assert_eq!(
        object["recurrenceOverrides"]["2026-09-27T10:00:00"],
        master()["recurrenceOverrides"]["2026-09-27T10:00:00"]
    );
    assert!(object["recurrenceOverrides"]["2026-10-04T10:00:00"]
        .get("id")
        .is_none());
    let again = connection
        .create_native_event_set(&config, "destination", &source, "operation-1")
        .await
        .unwrap();
    assert_eq!(again, copied);
    assert_eq!(server.writes().len(), 1);
}

#[tokio::test]
async fn lost_create_response_reconciles_operation_uid_without_second_write() {
    let server = Server::start(vec![master()]).await;
    let (config, connection) = server.connection().await;
    let source = connection
        .fetch_native_event_set(&config, &template(), "source")
        .await
        .unwrap();
    server.store.lock().unwrap().fault = Fault::LostCreate;
    assert!(connection
        .create_native_event_set(&config, "destination", &source, "lost-operation")
        .await
        .is_err());
    let result = connection
        .create_native_event_set(&config, "destination", &source, "lost-operation")
        .await
        .unwrap();
    assert_eq!(result.native.unwrap().calendar_id, "destination");
    assert_eq!(server.writes().len(), 1);
}

#[tokio::test]
async fn incomplete_canonical_copy_remains_an_error_on_retry_without_duplicate() {
    let server = Server::start(vec![master()]).await;
    let (config, connection) = server.connection().await;
    let source = connection
        .fetch_native_event_set(&config, &template(), "source")
        .await
        .unwrap();
    server.store.lock().unwrap().fault = Fault::MissingOverride;
    for _ in 0..2 {
        assert!(connection
            .create_native_event_set(&config, "destination", &source, "partial-canonical")
            .await
            .is_err());
    }
    assert_eq!(server.writes().len(), 1);
}

#[tokio::test]
async fn all_day_until_and_exception_reset_round_trip() {
    let server = Server::start(Vec::new()).await;
    let (config, connection) = server.connection().await;
    let mut event = template();
    event.all_day = true;
    event.start_time = "2026-09-13".into();
    event.end_time = "2026-09-14".into();
    event.timezone = Some("Europe/Stockholm".into());
    event.recurrence_kind = RecurrenceKind::Series;
    event.recurrence_rule = Some("FREQ=DAILY;UNTIL=20260916".into());
    let desired = CalendarEventSet {
        event,
        overrides: vec![CalendarOverride {
            original_start: "2026-09-14".into(),
            event: None,
            native: None,
        }],
        native: None,
        content: None,
    };
    let before = connection
        .create_native_event_set(&config, "chosen", &desired, "all-day-until")
        .await
        .unwrap();
    assert_eq!(before.event.recurrence_rule, desired.event.recurrence_rule);
    assert_eq!(
        simple_recurrence::position_at(&before.event, 3).unwrap(),
        "2026-09-16"
    );
    let mut reset = before.clone();
    reset.event.recurrence_kind = RecurrenceKind::Standalone;
    reset.event.recurrence_rule = None;
    reset.overrides.clear();
    let result = connection
        .update_native_event_set(&config, &before, &reset)
        .await
        .unwrap();
    assert_eq!(result.event.recurrence_kind, RecurrenceKind::Standalone);
    assert!(result.overrides.is_empty());
}

#[tokio::test]
async fn deleting_detached_resources_chunks_canonical_gets() {
    let mut second = detached();
    second["id"] = json!("detached-2");
    second["recurrenceId"] = json!("2026-10-11T10:00:00");
    second["start"] = json!("2026-10-11T12:00:00");
    let server = Server::start(vec![master(), detached(), second]).await;
    let (config, connection) = server.connection().await;
    let before = connection
        .fetch_native_event_set(&config, &template(), "source")
        .await
        .unwrap();
    connection
        .delete_native_event_set(&config, &before)
        .await
        .unwrap();
    let store = server.store.lock().unwrap();
    assert!(store.objects.is_empty());
    assert!(store
        .requests
        .iter()
        .filter(|request| request["methodCalls"][0][0] == "CalendarEvent/get")
        .all(|request| request["methodCalls"][0][1]["ids"]
            .as_array()
            .unwrap()
            .len()
            <= 2));
}

#[tokio::test]
async fn selected_calendar_creation_uses_local_time_and_date_duration() {
    for all_day in [false, true] {
        let server = Server::start(Vec::new()).await;
        let (config, connection) = server.connection().await;
        let mut event = template();
        event.recurrence_kind = RecurrenceKind::Standalone;
        event.recurrence_rule = None;
        event.timezone = Some("Europe/Stockholm".into());
        event.all_day = all_day;
        event.start_time = if all_day {
            "2026-09-13"
        } else {
            "2026-09-13T04:00:00-04:00"
        }
        .into();
        event.end_time = if all_day {
            "2026-09-16"
        } else {
            "2026-09-13T05:30:00-04:00"
        }
        .into();
        let desired = CalendarEventSet {
            event,
            overrides: Vec::new(),
            native: None,
            content: None,
        };
        let created = connection
            .create_native_event_set(&config, "chosen", &desired, "create-op")
            .await
            .unwrap();
        let object = server.object(created.event.remote_id.as_deref().unwrap());
        assert_eq!(object["calendarIds"], json!({"chosen": true}));
        assert_eq!(object["timeZone"], "Europe/Stockholm");
        assert_eq!(
            object["start"],
            if all_day {
                "2026-09-13T00:00:00"
            } else {
                "2026-09-13T10:00:00"
            }
        );
        assert_eq!(object["duration"], if all_day { "P3D" } else { "PT5400S" });
    }
}

#[tokio::test]
async fn move_and_transfer_removal_preserve_other_memberships() {
    let mut native = master();
    native["calendarIds"]["unrelated"] = json!(true);
    let server = Server::start(vec![native.clone()]).await;
    let (config, connection) = server.connection().await;
    let before = connection
        .fetch_native_event_set(&config, &template(), "source")
        .await
        .unwrap();
    let moved = connection
        .move_native_event_set(&config, &before, "destination")
        .await
        .unwrap();
    assert_eq!(
        server.object("master")["calendarIds"],
        json!({"unrelated": true, "destination": true})
    );
    assert_eq!(
        server.object("master")["recurrenceOverrides"],
        native["recurrenceOverrides"]
    );
    connection
        .delete_native_event_set(&config, &moved)
        .await
        .unwrap();
    assert_eq!(
        server.object("master")["calendarIds"],
        json!({"unrelated": true})
    );
    let remaining = connection
        .fetch_native_event_set(&config, &template(), "unrelated")
        .await
        .unwrap();
    connection
        .delete_native_event_set(&config, &remaining)
        .await
        .unwrap();
    assert!(server.store.lock().unwrap().objects.is_empty());
}

#[tokio::test]
async fn conflicts_partial_results_and_failed_canonical_reads_never_report_success() {
    for fault in [
        Fault::Conflict,
        Fault::PartialSet,
        Fault::MissingCanonical,
        Fault::WrongAccount,
        Fault::WrongState,
        Fault::MissingResult,
    ] {
        let server = Server::start(vec![master(), detached()]).await;
        let (config, connection) = server.connection().await;
        let before = connection
            .fetch_native_event_set(&config, &template(), "source")
            .await
            .unwrap();
        server.store.lock().unwrap().fault = fault;
        let result = connection
            .move_native_event_set(&config, &before, "destination")
            .await;
        assert!(result.is_err());
        if matches!(fault, Fault::PartialSet) {
            assert!(server.store.lock().unwrap().wrote);
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("may have succeeded"));
        }
        assert_eq!(server.writes().len(), 1, "no blind mutation retry");
    }
}

#[tokio::test]
async fn ordinary_backend_callback_performs_a_conditional_native_update() {
    use crate::backend::calendar::{
        jmap::JmapCalendarBackend, CalendarBackend, CalendarBackendCtx,
    };
    let server = Server::start(vec![standalone()]).await;
    let services = server.services();
    let account = server.account();
    let (_directory, db) = crate::backend::testutil::temp_pool();
    let mut event = template();
    event.account_id = account.id.clone();
    {
        let conn = db.writer().await;
        crate::db::schema::initialize(&conn).unwrap();
        conn.execute("INSERT INTO accounts (id, display_name, email, username) VALUES (?1, 'Test', 'u@test.invalid', 'u@test.invalid')", [&account.id]).unwrap();
        event.calendar_id = crate::db::calendar::upsert_calendar_by_remote_id(
            &conn,
            &account.id,
            "source",
            "Source",
            "#123456",
            true,
        )
        .unwrap();
    }
    event.start_time = "2026-09-13T08:00:00Z".into();
    event.end_time = "2026-09-13T09:00:00Z".into();
    event.timezone = Some("Europe/Stockholm".into());
    event.recurrence_kind = RecurrenceKind::Standalone;
    event.recurrence_rule = None;
    event.title = "Ordinary edit".into();
    JmapCalendarBackend
        .push_updated_event(
            &CalendarBackendCtx {
                db: &db,
                services: &services,
            },
            &account,
            "master",
            &event,
        )
        .await
        .unwrap();
    assert_eq!(server.object("master")["title"], "Ordinary edit");
    assert_eq!(
        server.object("master")["x-provider"],
        standalone()["x-provider"]
    );
    assert_eq!(server.writes().len(), 1);
}

#[test]
fn fractional_duration_and_native_nested_override_paths_are_lossless() {
    let mut object = master();
    object["recurrenceOverrides"]["2026-09-20T10:00:00"] =
        json!({"locations/room/name": "Patched room"});
    let set = decode_set(
        &template(),
        "calendar-account",
        "source",
        "revision",
        &object,
        &[],
    )
    .unwrap();
    assert_eq!(
        set.overrides[0].event.as_ref().unwrap().location.as_deref(),
        Some("Patched room")
    );
    let copy = creation_object(&set, "destination", "new-uid").unwrap();
    assert_eq!(copy["recurrenceOverrides"], object["recurrenceOverrides"]);
    let mut fields = event_fields(&set.event);
    fields.start_time = "2026-09-13T08:00:00.123456789Z".into();
    fields.end_time = "2026-09-13T09:00:00.987654321Z".into();
    assert_eq!(duration(&fields).unwrap(), "PT3600.864197532S");
    assert_eq!(
        local_start(&fields.start_time, false, Some("Europe/Stockholm")).unwrap(),
        "2026-09-13T10:00:00.123456789"
    );
}

#[test]
fn invalid_native_recurrence_identity_is_not_inferred_from_fallbacks() {
    let mut detached = detached();
    detached["recurrenceIdTimeZone"] = json!(false);
    assert!(decode_set(
        &template(),
        "calendar-account",
        "source",
        "revision",
        &master(),
        &[detached]
    )
    .is_err());
    let mut embedded = master();
    embedded["recurrenceOverrides"]["2026-09-20T10:00:00"]["recurrenceIdTimeZone"] =
        json!("America/New_York");
    assert!(decode_set(
        &template(),
        "calendar-account",
        "source",
        "revision",
        &embedded,
        &[]
    )
    .is_err());
}
