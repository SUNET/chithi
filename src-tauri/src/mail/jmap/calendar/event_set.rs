//! Complete native snapshots and conditional writes for the repository's
//! JSCalendar Event schema. The envelope is private persistence, never IPC.

use super::*;
use crate::calendar::event_set::{
    apply_event_fields, event_fields, CalendarEventSet, CalendarOverride, NativeCalendarResource,
};
use crate::calendar::{simple_recurrence, CalendarEvent};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

fn invalid(message: &str) -> Error {
    Error::Sync(format!("JMAP calendar: {message}; reconciliation required"))
}

/// Convert an instant, including its offset, rather than trimming a suffix.
pub(super) fn local_start(value: &str, all_day: bool, zone: Option<&str>) -> Result<String> {
    if all_day {
        let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map_err(|_| invalid("invalid all-day date"))?;
        return Ok(format!("{date}T00:00:00"));
    }
    let instant = chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|_| invalid("invalid start instant"))?;
    let zone = zone
        .unwrap_or("UTC")
        .parse::<chrono_tz::Tz>()
        .map_err(|_| invalid("invalid IANA timezone"))?;
    Ok(instant
        .with_timezone(&zone)
        .format("%Y-%m-%dT%H:%M:%S%.f")
        .to_string())
}

pub(super) fn duration(fields: &OccurrenceFields) -> Result<String> {
    fields.validate()?;
    if fields.all_day {
        return Ok(compute_duration(&fields.start_time, &fields.end_time));
    }
    let parse = |value: &str| {
        chrono::DateTime::parse_from_rfc3339(value).map_err(|_| invalid("invalid duration instant"))
    };
    let delta = parse(&fields.end_time)? - parse(&fields.start_time)?;
    let seconds = delta.num_seconds();
    let nanos = (delta - chrono::Duration::seconds(seconds))
        .num_nanoseconds()
        .ok_or_else(|| invalid("duration overflow"))?;
    if nanos == 0 {
        Ok(format!("PT{seconds}S"))
    } else {
        let fraction = format!("{nanos:09}");
        Ok(format!("PT{seconds}.{}S", fraction.trim_end_matches('0')))
    }
}

fn field_patch(
    before: &OccurrenceFields,
    desired: &OccurrenceFields,
    native: &Value,
) -> Result<Map<String, Value>> {
    desired.validate()?;
    let mut patch = Map::new();
    if before.title != desired.title {
        patch.insert("title".into(), json!(desired.title));
    }
    if before.description != desired.description {
        // Empty strings clear inherited override descriptions as well.
        patch.insert(
            "description".into(),
            json!(desired.description.as_deref().unwrap_or("")),
        );
    }
    if before.location != desired.location {
        let mut locations = native["locations"].as_object().cloned().unwrap_or_default();
        if let Some(name) = desired.location.as_deref().filter(|name| !name.is_empty()) {
            let key = locations
                .iter()
                .find(|(_, value)| value["name"].is_string())
                .map(|(key, _)| key.clone())
                .unwrap_or_else(|| "loc1".into());
            let location = locations
                .entry(key)
                .or_insert_with(|| json!({"@type": "Location"}));
            location["name"] = json!(name);
        } else {
            for location in locations.values_mut() {
                if let Some(object) = location.as_object_mut() {
                    object.remove("name");
                }
            }
        }
        patch.insert("locations".into(), Value::Object(locations));
    }
    if before.start_time != desired.start_time
        || before.timezone != desired.timezone
        || before.all_day != desired.all_day
    {
        patch.insert(
            "start".into(),
            json!(local_start(
                &desired.start_time,
                desired.all_day,
                desired.timezone.as_deref()
            )?),
        );
    }
    if before.start_time != desired.start_time
        || before.end_time != desired.end_time
        || before.all_day != desired.all_day
    {
        patch.insert("duration".into(), json!(duration(desired)?));
    }
    if before.timezone != desired.timezone || before.all_day != desired.all_day {
        patch.insert("timeZone".into(), json!(desired.timezone));
    }
    if before.all_day != desired.all_day {
        patch.insert("showWithoutTime".into(), json!(desired.all_day));
    }
    Ok(patch)
}

fn rules(event: &CalendarEvent) -> Result<Value> {
    match event
        .recurrence_rule
        .as_deref()
        .filter(|rule| !rule.is_empty())
    {
        None => Ok(Value::Null),
        Some(rule) => {
            let normalized = simple_recurrence::normalize_rule(rule, event)?;
            faithful_local_recurrence_rules(&normalized, event.timezone.as_deref())
                .ok_or_else(|| invalid("recurrence rule cannot be encoded faithfully"))
        }
    }
}

pub(super) fn creation_participants(
    organizer: Option<&str>,
    attendees: Option<&str>,
) -> Result<Value> {
    let mut participants = Map::new();
    if let Some(email) = organizer.filter(|email| !email.is_empty()) {
        participants.insert("organizer".into(), json!({"@type": "Participant",
            "calendarAddress": format!("mailto:{email}"), "roles": {"owner": true, "attendee": true},
            "participationStatus": "accepted", "expectReply": false}));
    }
    if let Some(attendees) = attendees {
        let attendees: Vec<Value> =
            serde_json::from_str(attendees).map_err(|_| invalid("invalid attendees JSON"))?;
        for (index, attendee) in attendees.iter().enumerate() {
            let email = attendee["email"]
                .as_str()
                .filter(|email| !email.is_empty())
                .ok_or_else(|| invalid("attendee address missing"))?;
            if organizer.is_some_and(|organizer| organizer.eq_ignore_ascii_case(email)) {
                continue;
            }
            let mut participant = json!({"@type": "Participant", "calendarAddress": format!("mailto:{email}"),
                "roles": {"attendee": true}, "participationStatus": attendee["status"].as_str().unwrap_or("needs-action"), "expectReply": true});
            if let Some(name) = attendee["name"].as_str() {
                participant["name"] = json!(name);
            }
            participants.insert(format!("att{index}"), participant);
        }
    }
    Ok(Value::Object(participants))
}

fn native_resource(
    account: &str,
    calendar: &str,
    state: &str,
    event: &Value,
) -> NativeCalendarResource {
    NativeCalendarResource {
        protocol: "jmap".into(),
        calendar_id: calendar.into(),
        event_id: event["id"].as_str().expect("validated native id").into(),
        revision: Some(state.into()),
        data: json!({"version": 1, "accountId": account, "event": event}).to_string(),
    }
}

fn unpack(resource: &NativeCalendarResource, account: &str) -> Result<Value> {
    let envelope: Value =
        serde_json::from_str(&resource.data).map_err(|_| invalid("invalid native envelope"))?;
    let event = &envelope["event"];
    if resource.protocol != "jmap"
        || envelope["version"] != 1
        || envelope["accountId"].as_str() != Some(account)
        || validate_event_object(event)? != resource.event_id
        || !event_calendar_ids(event)?.contains(&resource.calendar_id)
        || resource.revision.as_deref().is_none_or(str::is_empty)
    {
        return Err(invalid("native resource scope or revision mismatch"));
    }
    Ok(event.clone())
}

/// Apply JSCalendar PatchObject paths to a clone, preserving extension fields.
fn apply_patch_object(base: &Value, patch: &Map<String, Value>) -> Result<Value> {
    let mut result = base.clone();
    for key in patch.keys() {
        if patch
            .keys()
            .any(|other| other != key && other.starts_with(&format!("{key}/")))
        {
            return Err(invalid("overlapping override patch paths"));
        }
    }
    for (path, value) in patch {
        let segments = path
            .split('/')
            .map(|part| part.replace("~1", "/").replace("~0", "~"))
            .collect::<Vec<_>>();
        let mut target = &mut result;
        for segment in &segments[..segments.len() - 1] {
            target = target
                .as_object_mut()
                .and_then(|object| object.get_mut(segment))
                .ok_or_else(|| invalid("override patch parent is missing"))?;
        }
        let object = target
            .as_object_mut()
            .ok_or_else(|| invalid("override patch parent is not an object"))?;
        let key = segments.last().expect("split is nonempty");
        if value.is_null() {
            object.remove(key);
        } else {
            object.insert(key.clone(), value.clone());
        }
    }
    Ok(result)
}

fn effective_override(master: &Value, key: &str, patch: &Map<String, Value>) -> Result<Value> {
    let mut occurrence = master.clone();
    occurrence["start"] = json!(key);
    apply_patch_object(&occurrence, patch)
}

fn original_position(master: &CalendarEvent, local: &str, zone: Option<&str>) -> Result<String> {
    if master.all_day {
        let date = chrono::NaiveDateTime::parse_from_str(local, "%Y-%m-%dT%H:%M:%S%.f")
            .map_err(|_| invalid("invalid original all-day position"))?;
        return Ok(date.date().to_string());
    }
    effective_range(local, zone, "PT1S")
        .map(|(start, _)| start)
        .ok_or_else(|| invalid("original position has an ambiguous or invalid timezone"))
}

fn project(template: &CalendarEvent, native: &Value) -> Result<CalendarEvent> {
    let mut event = template.clone();
    let fields = occurrence_fields(native, None, None)
        .ok_or_else(|| invalid("incomplete canonical fields"))?;
    apply_event_fields(&mut event, &fields);
    event.uid = native["uid"].as_str().map(str::to_string);
    event.remote_id = native["id"].as_str().map(str::to_string);
    event.etag = None;
    event.ical_data = None;
    event.recurrence_kind = classify_recurrence(native);
    let native_rules = native["recurrenceRules"]
        .as_array()
        .filter(|rules| !rules.is_empty())
        .map(Vec::as_slice)
        .or_else(|| {
            native
                .get("recurrenceRule")
                .filter(|rule| rule.is_object())
                .map(std::slice::from_ref)
        });
    event.recurrence_rule = native_rules.and_then(|rules| {
        crate::calendar::recurrence::jscalendar_to_rrule(rules, fields.timezone.as_deref())
    });
    if fields.all_day {
        // A date-valued series has no UTC cutoff. The codec's timed UNTIL
        // projection must retain the native local calendar date instead.
        if let (Some(rule), Some(until)) = (
            &mut event.recurrence_rule,
            native_rules
                .and_then(|rules| rules.first())
                .and_then(|rule| rule["until"].as_str()),
        ) {
            let date = chrono::NaiveDateTime::parse_from_str(until, "%Y-%m-%dT%H:%M:%S%.f")
                .map_err(|_| invalid("invalid all-day recurrence cutoff"))?
                .date();
            *rule = rule
                .split(';')
                .map(|part| {
                    if part.starts_with("UNTIL=") {
                        format!("UNTIL={}", date.format("%Y%m%d"))
                    } else {
                        part.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(";");
        }
    }
    // Scheduling fields are canonical projections; native participant ids stay private.
    event.organizer_email = None;
    let mut attendees = Vec::new();
    if let Some(participants) = native["participants"].as_object() {
        for participant in participants.values() {
            let email = participant["calendarAddress"]
                .as_str()
                .or_else(|| participant["sendTo"]["imip"].as_str())
                .or_else(|| participant["email"].as_str());
            if let Some(email) = email {
                let email = email.trim_start_matches("mailto:");
                if participant["roles"]["owner"] == true {
                    event.organizer_email = Some(email.into());
                }
                attendees.push(json!({"email": email, "name": participant["name"],
                    "status": participant["participationStatus"].as_str().unwrap_or("needs-action")}));
            }
        }
    }
    event.attendees_json = (!attendees.is_empty()).then(|| json!(attendees).to_string());
    event.my_status = None;
    Ok(event)
}

fn decode_set(
    template: &CalendarEvent,
    account: &str,
    calendar: &str,
    state: &str,
    master: &Value,
    detached: &[Value],
) -> Result<CalendarEventSet> {
    validate_event_object(master)?;
    if master
        .get("excludedRecurrenceRules")
        .is_some_and(|rules| !rules.is_null() && !rules.as_array().is_some_and(Vec::is_empty))
    {
        return Err(invalid(
            "rule-based exclusions cannot be represented as finite overrides",
        ));
    }
    if !event_calendar_ids(master)?.iter().any(|id| id == calendar) {
        return Err(invalid("selected calendar membership is missing"));
    }
    let event = project(template, master)?;
    let native = native_resource(account, calendar, state, master);
    let mut overrides = Vec::new();
    if let Some(patches) = master
        .get("recurrenceOverrides")
        .filter(|value| !value.is_null())
    {
        for (key, patch) in patches
            .as_object()
            .ok_or_else(|| invalid("invalid overrides"))?
        {
            let patch = patch
                .as_object()
                .ok_or_else(|| invalid("invalid override"))?;
            if patch
                .get("recurrenceIdTimeZone")
                .is_some_and(|zone| zone != &master["timeZone"])
            {
                return Err(invalid(
                    "embedded override changes its recurrence identity timezone",
                ));
            }
            let effective = effective_override(master, key, patch)?;
            let original_start = original_position(&event, key, master["timeZone"].as_str())?;
            let value = if patch.get("excluded") == Some(&Value::Bool(true)) {
                None
            } else {
                let mut value = project(&event, &effective)?;
                value.recurrence_rule = None;
                value.recurrence_kind = RecurrenceKind::Occurrence;
                Some(value)
            };
            overrides.push(CalendarOverride {
                original_start,
                event: value,
                native: Some(native.clone()),
            });
        }
    }
    for object in detached {
        validate_event_object(object)?;
        if classify_recurrence(object) != RecurrenceKind::Occurrence
            || object["baseEventId"] != master["id"]
            || object["uid"] != master["uid"]
            || !event_calendar_ids(object)?.iter().any(|id| id == calendar)
        {
            return Err(invalid("detached instance identity contradicts master"));
        }
        let key = object["recurrenceId"]
            .as_str()
            .ok_or_else(|| invalid("missing detached recurrenceId"))?;
        let zone = object
            .get("recurrenceIdTimeZone")
            .unwrap_or(&master["timeZone"])
            .as_str();
        let original_start = original_position(&event, key, zone)?;
        let mut value = project(&event, object)?;
        value.recurrence_rule = None;
        value.recurrence_kind = RecurrenceKind::Occurrence;
        overrides.push(CalendarOverride {
            original_start,
            event: (object["excluded"] != true).then_some(value),
            native: Some(native_resource(account, calendar, state, object)),
        });
    }
    overrides.sort_by(|a, b| a.original_start.cmp(&b.original_start));
    let set = CalendarEventSet {
        event,
        overrides,
        native: Some(native),
        content: None,
    };
    set.validate()?;
    Ok(set)
}

fn check_account(body: &Map<String, Value>, account: &str) -> Result<()> {
    if body.get("accountId").and_then(Value::as_str) != Some(account) {
        return Err(invalid("response account mismatch"));
    }
    Ok(())
}

fn state(body: &Map<String, Value>, name: &str) -> Result<String> {
    body.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| invalid("missing CalendarEvent data state"))
}

impl JmapConnection {
    pub(crate) async fn delete_calendar_membership(
        &self,
        config: &JmapConfig,
        id: &str,
        calendar: &str,
    ) -> Result<()> {
        let (revision, mut objects) = self.native_get(config, &[id.into()]).await?;
        let memberships = objects[0]["calendarIds"]
            .as_object_mut()
            .ok_or_else(|| invalid("memberships missing"))?;
        if memberships.remove(calendar) != Some(json!(true)) {
            return Err(invalid("selected membership missing"));
        }
        if !memberships
            .values()
            .any(|value| value == &Value::Bool(true))
        {
            return self.destroy_native_event(config, id, &revision).await;
        }
        let expected = Value::Object(memberships.clone());
        let (new_state, _) = self
            .native_set(
                config,
                &revision,
                Map::new(),
                Map::from_iter([(id.into(), json!({"calendarIds": expected}))]),
                Vec::new(),
            )
            .await?;
        let (state, objects) = self.native_get(config, &[id.into()]).await?;
        if state != new_state || objects[0]["calendarIds"] != expected {
            return Err(invalid("canonical membership removal mismatch"));
        }
        Ok(())
    }

    pub(super) async fn destroy_native_event(
        &self,
        config: &JmapConfig,
        id: &str,
        revision: &str,
    ) -> Result<()> {
        let (new_state, _) = self
            .native_set(config, revision, Map::new(), Map::new(), vec![id.into()])
            .await?;
        let response = self
            .api_request(
                &calendar_events_get_request(&self.account_id, &[id.into()]),
                config,
            )
            .await?;
        let body = method_response(&response, "CalendarEvent/get", "g1")?;
        check_account(body, &self.account_id)?;
        if state(body, "state")? != new_state
            || body.get("notFound") != Some(&json!([id]))
            || body.get("list") != Some(&json!([]))
        {
            return Err(invalid("canonical deletion not confirmed"));
        }
        Ok(())
    }

    pub(crate) async fn update_native_event_set(
        &self,
        config: &JmapConfig,
        before: &CalendarEventSet,
        desired: &CalendarEventSet,
    ) -> Result<CalendarEventSet> {
        before.validate()?;
        desired.validate()?;
        let (resource, _) = set_resources(before, &self.account_id)?;
        let master = unpack(resource, &self.account_id)?;
        if before.event.remote_id.as_deref() != Some(resource.event_id.as_str())
            || before.event.uid != desired.event.uid
            || before.event.remote_id != desired.event.remote_id
            || before.event.account_id != desired.event.account_id
            || before.event.calendar_id != desired.event.calendar_id
        {
            return Err(invalid("update changed immutable identity"));
        }
        let expected = resource
            .revision
            .as_deref()
            .ok_or_else(|| invalid("revision missing"))?;
        let mut master_patch = field_patch(
            &event_fields(&before.event),
            &event_fields(&desired.event),
            &master,
        )?;
        if before.event.recurrence_rule != desired.event.recurrence_rule {
            master_patch.insert("recurrenceRules".into(), rules(&desired.event)?);
            if master.get("recurrenceRule").is_some() {
                master_patch.insert("recurrenceRule".into(), Value::Null);
            }
        }
        let effective_master = apply_patch_object(&master, &master_patch)?;
        let mapped = override_origins(before, desired)?;
        let mut updates = Map::new();
        let mut destroys = Vec::new();
        let original_patches = master["recurrenceOverrides"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let mut patches = Map::new();
        for old in &before.overrides {
            let old_native = old
                .native
                .as_ref()
                .ok_or_else(|| invalid("override native identity missing"))?;
            unpack(old_native, &self.account_id)?;
            if old_native.revision.as_deref() != Some(expected) {
                return Err(invalid("mixed override revisions"));
            }
            if old_native.event_id != resource.event_id
                && !mapped
                    .values()
                    .any(|kept| kept.original_start == old.original_start)
            {
                destroys.push(old_native.event_id.clone());
            }
        }
        for new in &desired.overrides {
            let old = mapped.get(&new.original_start).copied();
            let key = local_start(
                &new.original_start,
                desired.event.all_day,
                desired.event.timezone.as_deref(),
            )?;
            let detached = old
                .and_then(|old| old.native.as_ref())
                .filter(|native| native.event_id != resource.event_id);
            if let Some(native) = detached {
                let object = unpack(native, &self.account_id)?;
                if native.revision.as_deref() != Some(expected) {
                    return Err(invalid("mixed detached revisions"));
                }
                if old.is_some_and(|old| {
                    old.event == new.event && old.original_start == new.original_start
                }) && before.event.timezone == desired.event.timezone
                {
                    continue;
                }
                let mut patch = match &new.event {
                    None => Map::new(),
                    Some(event) => field_patch(
                        &occurrence_fields(&object, None, None)
                            .ok_or_else(|| invalid("invalid detached fields"))?,
                        &event_fields(event),
                        &object,
                    )?,
                };
                if object["recurrenceId"] != key || before.event.timezone != desired.event.timezone
                {
                    patch.insert("recurrenceId".into(), json!(key));
                    patch.insert("recurrenceIdTimeZone".into(), json!(desired.event.timezone));
                }
                patch.insert("excluded".into(), json!(new.event.is_none()));
                updates.insert(native.event_id.clone(), Value::Object(patch));
                continue;
            }
            let old_key = old
                .map(|old| {
                    local_start(
                        &old.original_start,
                        before.event.all_day,
                        before.event.timezone.as_deref(),
                    )
                })
                .transpose()?;
            let mut patch = old_key
                .as_ref()
                .and_then(|key| original_patches.get(key))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if old.is_none() {
                simple_recurrence::resolve(&desired.event, &new.original_start)?;
            }
            if let Some(event) = &new.event {
                let effective = effective_override(&effective_master, &key, &patch)?;
                let baseline = occurrence_fields(&effective, None, None)
                    .ok_or_else(|| invalid("invalid override fields"))?;
                let changed = field_patch(&baseline, &event_fields(event), &effective)?;
                for (name, value) in changed {
                    // A top-level replacement supersedes nested PatchObject paths.
                    patch.retain(|path, _| path != &name && !path.starts_with(&format!("{name}/")));
                    patch.insert(name, value);
                }
                patch.remove("excluded");
            } else {
                patch.insert("excluded".into(), json!(true));
            }
            patches.insert(key, Value::Object(patch));
        }
        if patches != original_patches {
            master_patch.insert("recurrenceOverrides".into(), Value::Object(patches));
        }
        if !master_patch.is_empty() {
            updates.insert(resource.event_id.clone(), Value::Object(master_patch));
        }
        if updates.is_empty() && destroys.is_empty() {
            return self
                .canonical_set(config, &before.event, &resource.calendar_id, expected)
                .await;
        }
        let (new_state, _) = self
            .native_set(config, expected, Map::new(), updates, destroys)
            .await?;
        let result = self
            .canonical_set(config, &before.event, &resource.calendar_id, &new_state)
            .await?;
        verify_copy(desired, &result)?;
        Ok(result)
    }

    pub(crate) async fn create_native_event_set(
        &self,
        config: &JmapConfig,
        calendar: &str,
        desired: &CalendarEventSet,
        operation_id: &str,
    ) -> Result<CalendarEventSet> {
        desired.validate()?;
        if !valid_identifier(calendar) || !valid_identifier(operation_id) {
            return Err(invalid("creation identity missing"));
        }
        // The journal owns operation_id. Deterministic UID survives a lost reply
        // and does not depend on any source provider id or source UID.
        use sha2::{Digest, Sha256};
        let uid = format!(
            "chithi-{:x}@operation",
            Sha256::digest(operation_id.as_bytes())
        );
        let (revision, objects) = self.native_snapshot(config).await?;
        let matching = objects
            .iter()
            .filter(|object| object["uid"] == uid)
            .collect::<Vec<_>>();
        if !matching.is_empty() {
            if matching.len() != 1
                || matching[0]["calendarIds"][calendar] != true
                || !matching[0]["baseEventId"].is_null()
            {
                return Err(invalid(
                    "operation UID is ambiguous or belongs to another calendar",
                ));
            }
            let mut template = desired.event.clone();
            template.remote_id = matching[0]["id"].as_str().map(str::to_string);
            template.uid = Some(uid);
            let result = self
                .canonical_set(config, &template, calendar, &revision)
                .await?;
            verify_copy(desired, &result)?;
            return Ok(result);
        }
        let object = creation_object(desired, calendar, &uid)?;
        let (new_state, created) = self
            .native_set(
                config,
                &revision,
                Map::from_iter([("new1".into(), object)]),
                Map::new(),
                Vec::new(),
            )
            .await?;
        let mut template = desired.event.clone();
        template.remote_id = created["new1"]["id"].as_str().map(str::to_string);
        template.uid = Some(uid);
        let result = self
            .canonical_set(config, &template, calendar, &new_state)
            .await?;
        verify_copy(desired, &result)?;
        Ok(result)
    }

    pub(crate) async fn move_native_event_set(
        &self,
        config: &JmapConfig,
        before: &CalendarEventSet,
        destination: &str,
    ) -> Result<CalendarEventSet> {
        if !valid_identifier(destination) {
            return Err(invalid("destination calendar missing"));
        }
        let (resource, objects) = set_resources(before, &self.account_id)?;
        let expected = resource
            .revision
            .as_deref()
            .ok_or_else(|| invalid("revision missing"))?;
        let mut updates = Map::new();
        for (id, mut object) in objects {
            let memberships = object["calendarIds"]
                .as_object_mut()
                .ok_or_else(|| invalid("memberships missing"))?;
            memberships.remove(&resource.calendar_id);
            memberships.insert(destination.into(), json!(true));
            updates.insert(id, json!({"calendarIds": memberships}));
        }
        let (new_state, _) = self
            .native_set(config, expected, Map::new(), updates.clone(), Vec::new())
            .await?;
        let result = self
            .canonical_set(config, &before.event, destination, &new_state)
            .await?;
        let (_, canonical) = set_resources(&result, &self.account_id)?;
        if canonical.len() != updates.len()
            || updates.iter().any(|(id, patch)| {
                canonical
                    .get(id)
                    .is_none_or(|object| object["calendarIds"] != patch["calendarIds"])
            })
        {
            return Err(invalid(
                "canonical move did not preserve exact resource memberships",
            ));
        }
        Ok(result)
    }

    pub(crate) async fn delete_native_event_set(
        &self,
        config: &JmapConfig,
        before: &CalendarEventSet,
    ) -> Result<()> {
        let (resource, objects) = set_resources(before, &self.account_id)?;
        let expected = resource
            .revision
            .as_deref()
            .ok_or_else(|| invalid("revision missing"))?;
        let mut updates = Map::new();
        let mut destroys = Vec::new();
        for (id, mut object) in objects {
            let memberships = object["calendarIds"]
                .as_object_mut()
                .ok_or_else(|| invalid("memberships missing"))?;
            memberships.remove(&resource.calendar_id);
            if memberships
                .values()
                .any(|value| value == &Value::Bool(true))
            {
                updates.insert(id, json!({"calendarIds": memberships}));
            } else {
                destroys.push(id);
            }
        }
        let (new_state, _) = self
            .native_set(
                config,
                expected,
                Map::new(),
                updates.clone(),
                destroys.clone(),
            )
            .await?;
        // Verify destruction and surviving memberships with an exact get. Query
        // absence is insufficient evidence and has a different state namespace.
        let ids = updates
            .keys()
            .cloned()
            .chain(destroys.iter().cloned())
            .collect::<Vec<_>>();
        for chunk in ids.chunks(
            self.max_objects_in_get
                .clamp(1, CALENDAR_EVENT_MAX_GET_CHUNK),
        ) {
            let response = self
                .api_request(
                    &calendar_events_get_request(&self.account_id, chunk),
                    config,
                )
                .await?;
            let body = method_response(&response, "CalendarEvent/get", "g1")?;
            check_account(body, &self.account_id)?;
            if state(body, "state")? != new_state {
                return Err(invalid("state changed before deletion verification"));
            }
            let missing = body
                .get("notFound")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("notFound missing"))?;
            let list = body
                .get("list")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("deletion canonical list missing"))?;
            let expected_missing = chunk
                .iter()
                .filter(|id| destroys.contains(id))
                .collect::<Vec<_>>();
            if missing.len() != expected_missing.len()
                || expected_missing.iter().any(|id| {
                    missing
                        .iter()
                        .filter(|value| value.as_str() == Some(id.as_str()))
                        .count()
                        != 1
                })
                || list.len() != chunk.len() - expected_missing.len()
            {
                return Err(invalid("partial deletion verification"));
            }
            let mut seen = HashSet::new();
            for object in list {
                let id = validate_event_object(object)?;
                if !seen.insert(id.clone())
                    || !updates.contains_key(&id)
                    || !chunk.contains(&id)
                    || object["calendarIds"] != updates[&id]["calendarIds"]
                {
                    return Err(invalid("canonical membership removal mismatch"));
                }
            }
        }
        Ok(())
    }

    pub(super) async fn native_get(
        &self,
        config: &JmapConfig,
        ids: &[String],
    ) -> Result<(String, Vec<Value>)> {
        let response = self
            .api_request(&calendar_events_get_request(&self.account_id, ids), config)
            .await?;
        let body = method_response(&response, "CalendarEvent/get", "g1")?;
        check_account(body, &self.account_id)?;
        let revision = state(body, "state")?;
        if !body
            .get("notFound")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            return Err(invalid("canonical objects missing or notFound omitted"));
        }
        let list = body
            .get("list")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("canonical list missing"))?;
        let mut seen = HashSet::new();
        for event in list {
            let id = validate_event_object(event)?;
            if !ids.contains(&id) || !seen.insert(id) {
                return Err(invalid("unexpected or repeated canonical object"));
            }
        }
        if seen.len() != ids.len() {
            return Err(invalid("partial canonical response"));
        }
        Ok((revision, list.clone()))
    }

    /// Query is only an enumeration validator. Revisions always come from get.
    pub(super) async fn native_snapshot(
        &self,
        config: &JmapConfig,
    ) -> Result<(String, Vec<Value>)> {
        let mut ids = Vec::new();
        let mut query_state = None;
        let mut total = None;
        for _ in 0..CALENDAR_EVENT_MAX_PAGES {
            let response = self
                .api_request(
                    &calendar_event_query_request(
                        &self.account_id,
                        ids.len(),
                        CALENDAR_EVENT_QUERY_PAGE_SIZE,
                        "q1",
                    ),
                    config,
                )
                .await?;
            let body = method_response(&response, "CalendarEvent/query", "q1")?;
            check_account(body, &self.account_id)?;
            let current = state(body, "queryState")?;
            let size = body
                .get("total")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("missing query total"))? as usize;
            if size > CALENDAR_EVENT_MAX_IDS
                || total.is_some_and(|old| old != size)
                || query_state.as_ref().is_some_and(|old| old != &current)
                || body.get("position").and_then(Value::as_u64) != Some(ids.len() as u64)
            {
                return Err(invalid("query changed or exceeded limits"));
            }
            query_state = Some(current);
            total = Some(size);
            let page = body
                .get("ids")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("query ids missing"))?;
            if page.len() > CALENDAR_EVENT_QUERY_PAGE_SIZE || (page.is_empty() && ids.len() != size)
            {
                return Err(invalid("nonadvancing query"));
            }
            for id in page {
                let id = id
                    .as_str()
                    .filter(|id| valid_identifier(id))
                    .ok_or_else(|| invalid("invalid query id"))?
                    .to_string();
                if ids.contains(&id) {
                    return Err(invalid("duplicate query id"));
                }
                ids.push(id);
            }
            if ids.len() == size {
                break;
            }
            if ids.len() > size {
                return Err(invalid("query exceeded total"));
            }
        }
        if total != Some(ids.len()) {
            return Err(invalid("incomplete query"));
        }
        let (revision, _) = self.native_get(config, &[]).await?;
        let mut objects = Vec::new();
        for chunk in ids.chunks(
            self.max_objects_in_get
                .clamp(1, CALENDAR_EVENT_MAX_GET_CHUNK),
        ) {
            let (current, values) = self.native_get(config, chunk).await?;
            if current != revision {
                return Err(invalid("data state changed while reading"));
            }
            objects.extend(values);
        }
        let response = self
            .api_request(
                &calendar_event_query_request(&self.account_id, 0, 0, "q2"),
                config,
            )
            .await?;
        let body = method_response(&response, "CalendarEvent/query", "q2")?;
        check_account(body, &self.account_id)?;
        if Some(state(body, "queryState")?) != query_state
            || body.get("total").and_then(Value::as_u64) != Some(ids.len() as u64)
            || body.get("position").and_then(Value::as_u64) != Some(0)
            || !body
                .get("ids")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
        {
            return Err(invalid("query changed before completion"));
        }
        Ok((revision, objects))
    }

    pub(crate) async fn fetch_native_event_set(
        &self,
        config: &JmapConfig,
        template: &CalendarEvent,
        calendar: &str,
    ) -> Result<CalendarEventSet> {
        let id = template
            .remote_id
            .as_deref()
            .filter(|id| valid_identifier(id))
            .ok_or_else(|| invalid("remote id missing"))?;
        let (revision, objects) = self.native_get(config, &[id.into()]).await?;
        let object = &objects[0];
        if !event_calendar_ids(object)?.iter().any(|id| id == calendar) {
            return Err(invalid("selected calendar membership missing"));
        }
        if template
            .uid
            .as_deref()
            .is_some_and(|uid| object["uid"].as_str() != Some(uid))
        {
            return Err(invalid("provider UID changed"));
        }
        if classify_recurrence(object) == RecurrenceKind::Standalone {
            return decode_set(template, &self.account_id, calendar, &revision, object, &[]);
        }
        let master_id = object["baseEventId"].as_str().unwrap_or(id);
        let (state, objects) = self.native_snapshot(config).await?;
        if state != revision {
            return Err(invalid("state changed while resolving series"));
        }
        let master = objects
            .iter()
            .find(|object| object["id"] == master_id)
            .ok_or_else(|| invalid("series master missing"))?;
        if master["uid"] != object["uid"] {
            return Err(invalid("detached UID mismatch"));
        }
        if objects.iter().any(|other| {
            other["id"] != master_id
                && other["uid"] == master["uid"]
                && other["baseEventId"].is_null()
                && other["calendarIds"][calendar] == true
        }) {
            return Err(invalid("ambiguous UID within selected calendar"));
        }
        let detached = objects
            .iter()
            .filter(|object| object["baseEventId"] == master_id)
            .cloned()
            .collect::<Vec<_>>();
        decode_set(
            template,
            &self.account_id,
            calendar,
            &state,
            master,
            &detached,
        )
    }

    /// Inspect every per-object result; a JMAP set can partially succeed.
    pub(super) async fn native_set(
        &self,
        config: &JmapConfig,
        expected: &str,
        create: Map<String, Value>,
        update: Map<String, Value>,
        destroy: Vec<String>,
    ) -> Result<(String, Map<String, Value>)> {
        if expected.is_empty()
            || create.len() + update.len() + destroy.len() > self.max_objects_in_set
        {
            return Err(invalid(
                "missing state or write exceeds server object limit",
            ));
        }
        let response = self
            .api_request(
                &json!({
                    "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
                    "methodCalls": [["CalendarEvent/set", {"accountId": self.account_id,
                        "ifInState": expected, "sendSchedulingMessages": false,
                        "create": create, "update": update, "destroy": destroy}, "s1"]]
                }),
                config,
            )
            .await?;
        let body = method_response(&response, "CalendarEvent/set", "s1")?;
        check_account(body, &self.account_id)?;
        if body.get("oldState").and_then(Value::as_str) != Some(expected) {
            return Err(invalid("write oldState mismatch"));
        }
        for name in ["notCreated", "notUpdated", "notDestroyed"] {
            if let Some(value) = body.get(name).filter(|value| !value.is_null()) {
                if !value.as_object().is_some_and(Map::is_empty) {
                    return Err(invalid(&format!(
                        "set rejected objects ({name}: {value}); other objects may have succeeded"
                    )));
                }
            }
        }
        for name in ["created", "updated"] {
            if body
                .get(name)
                .is_some_and(|value| !value.is_null() && !value.is_object())
            {
                return Err(invalid("malformed per-object set result map"));
            }
        }
        if body
            .get("destroyed")
            .is_some_and(|value| !value.is_null() && !value.is_array())
        {
            return Err(invalid("malformed destroyed result list"));
        }
        let created = body
            .get("created")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let updated = body
            .get("updated")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let destroyed = body
            .get("destroyed")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if created.len() != create.len()
            || updated.len() != update.len()
            || destroyed.len() != destroy.len()
            || create.keys().any(|id| {
                !created
                    .get(id)
                    .is_some_and(|object| object["id"].as_str().is_some_and(valid_identifier))
            })
            || update.keys().any(|id| {
                !updated
                    .get(id)
                    .is_some_and(|value| value.is_null() || value.is_object())
            })
            || destroy.iter().any(|id| {
                destroyed
                    .iter()
                    .filter(|value| value.as_str() == Some(id))
                    .count()
                    != 1
            })
        {
            return Err(invalid(
                "set omitted or contradicted object results; partial success possible",
            ));
        }
        Ok((state(body, "newState")?, created))
    }

    async fn canonical_set(
        &self,
        config: &JmapConfig,
        template: &CalendarEvent,
        calendar: &str,
        expected: &str,
    ) -> Result<CalendarEventSet> {
        let result = self
            .fetch_native_event_set(config, template, calendar)
            .await?;
        if result
            .native
            .as_ref()
            .and_then(|native| native.revision.as_deref())
            != Some(expected)
        {
            return Err(invalid("canonical read state changed after write"));
        }
        Ok(result)
    }
}

fn set_resources<'a>(
    set: &'a CalendarEventSet,
    account: &str,
) -> Result<(&'a NativeCalendarResource, BTreeMap<String, Value>)> {
    set.validate()?;
    let resource = set
        .native
        .as_ref()
        .ok_or_else(|| invalid("native master missing"))?;
    let mut objects = BTreeMap::from([(resource.event_id.clone(), unpack(resource, account)?)]);
    for exception in &set.overrides {
        let native = exception
            .native
            .as_ref()
            .ok_or_else(|| invalid("override resource missing"))?;
        if native.revision != resource.revision || native.calendar_id != resource.calendar_id {
            return Err(invalid("mixed resource scope or revision"));
        }
        let object = unpack(native, account)?;
        if native.event_id != resource.event_id && object["baseEventId"] != resource.event_id {
            return Err(invalid("detached resource has wrong master"));
        }
        if let Some(previous) = objects.insert(native.event_id.clone(), object.clone()) {
            if previous != object {
                return Err(invalid("contradictory native resources"));
            }
        }
    }
    Ok((resource, objects))
}

/// The coordinator remaps exceptions by recurrence phase for a whole-series
/// reschedule. Recover each original native patch before changing its map key.
fn override_origins<'a>(
    before: &'a CalendarEventSet,
    desired: &CalendarEventSet,
) -> Result<BTreeMap<String, &'a CalendarOverride>> {
    let schedule_changed = before.event.start_time != desired.event.start_time
        || before.event.timezone != desired.event.timezone
        || before.event.all_day != desired.event.all_day
        || before.event.recurrence_rule != desired.event.recurrence_rule;
    let mut result = BTreeMap::new();
    if desired.overrides.is_empty() {
        return Ok(result);
    }
    for old in &before.overrides {
        let position = if schedule_changed {
            let phase = simple_recurrence::occurrence_index(&before.event, &old.original_start)?;
            simple_recurrence::position_at(&desired.event, phase)?
        } else {
            old.original_start.clone()
        };
        if desired
            .overrides
            .iter()
            .any(|new| new.original_start == position)
        {
            result.insert(position, old);
        }
    }
    Ok(result)
}

fn creation_object(desired: &CalendarEventSet, calendar: &str, uid: &str) -> Result<Value> {
    let mut source_set = None;
    let mut object = if let Some(native) = desired
        .native
        .as_ref()
        .filter(|native| native.protocol == "jmap")
    {
        let envelope: Value = serde_json::from_str(&native.data)
            .map_err(|_| invalid("invalid source native JSON"))?;
        if envelope["version"] != 1 {
            return Err(invalid("unknown source native envelope version"));
        }
        validate_event_object(&envelope["event"])?;
        let mut object = envelope["event"].clone();
        source_set = Some(decode_set(
            &desired.event,
            envelope["accountId"]
                .as_str()
                .ok_or_else(|| invalid("source account missing"))?,
            &native.calendar_id,
            native
                .revision
                .as_deref()
                .ok_or_else(|| invalid("source revision missing"))?,
            &object,
            &[],
        )?);
        let source = project(&desired.event, &object)?;
        let patch = field_patch(
            &event_fields(&source),
            &event_fields(&desired.event),
            &object,
        )?;
        object = apply_patch_object(&object, &patch)?;
        if source.recurrence_rule != desired.event.recurrence_rule {
            object["recurrenceRules"] = rules(&desired.event)?;
            object
                .as_object_mut()
                .expect("event object")
                .remove("recurrenceRule");
        }
        object
    } else {
        let fields = event_fields(&desired.event);
        fields.validate()?;
        let mut object = json!({"@type": "Event", "title": fields.title,
            "description": fields.description, "start": local_start(&fields.start_time, fields.all_day, fields.timezone.as_deref())?,
            "duration": duration(&fields)?, "showWithoutTime": fields.all_day,
            "timeZone": fields.timezone, "recurrenceRules": rules(&desired.event)?,
            "participants": creation_participants(desired.event.organizer_email.as_deref(), desired.event.attendees_json.as_deref())?});
        if let Some(location) = fields.location {
            object["locations"] = json!({"loc1": {"@type": "Location", "name": location}});
        }
        object
    };
    let origins = source_set
        .as_ref()
        .map(|source| override_origins(source, desired))
        .transpose()?
        .unwrap_or_default();
    let mut patches = Map::new();
    for exception in &desired.overrides {
        let key = local_start(
            &exception.original_start,
            desired.event.all_day,
            desired.event.timezone.as_deref(),
        )?;
        let source_key = origins
            .get(&exception.original_start)
            .map(|old| {
                let source = source_set.as_ref().expect("origins require source");
                local_start(
                    &old.original_start,
                    source.event.all_day,
                    source.event.timezone.as_deref(),
                )
            })
            .transpose()?;
        let mut patch = source_key
            .as_ref()
            .and_then(|key| object["recurrenceOverrides"].get(key))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(native) = exception.native.as_ref().filter(|native| {
            native.protocol == "jmap"
                && desired
                    .native
                    .as_ref()
                    .is_some_and(|master| native.event_id != master.event_id)
        }) {
            let envelope: Value = serde_json::from_str(&native.data)
                .map_err(|_| invalid("invalid detached native JSON"))?;
            if envelope["version"] != 1 {
                return Err(invalid("unknown detached envelope version"));
            }
            let mut detached = envelope["event"].clone();
            validate_event_object(&detached)?;
            strip_resource_identity(&mut detached)?;
            patch = detached
                .as_object()
                .cloned()
                .ok_or_else(|| invalid("invalid detached source"))?;
            for key in [
                "uid",
                "calendarIds",
                "recurrenceId",
                "recurrenceIdTimeZone",
                "excluded",
            ] {
                patch.remove(key);
            }
        }
        if let Some(event) = &exception.event {
            let effective = effective_override(&object, &key, &patch)?;
            let fields = occurrence_fields(&effective, None, None)
                .ok_or_else(|| invalid("invalid copy override"))?;
            for (name, value) in field_patch(&fields, &event_fields(event), &effective)? {
                patch.retain(|path, _| path != &name && !path.starts_with(&format!("{name}/")));
                patch.insert(name, value);
            }
            patch.remove("excluded");
        } else {
            patch.insert("excluded".into(), json!(true));
        }
        patches.insert(key, Value::Object(patch));
    }
    if !patches.is_empty() || object.get("recurrenceOverrides").is_some() {
        object["recurrenceOverrides"] = Value::Object(patches);
    }
    strip_resource_identity(&mut object)?;
    object["calendarIds"] = json!({calendar: true});
    object["uid"] = json!(uid);
    Ok(object)
}

fn verify_copy(desired: &CalendarEventSet, canonical: &CalendarEventSet) -> Result<()> {
    if desired.overrides.len() != canonical.overrides.len()
        || desired.overrides.iter().any(|expected| {
            !canonical.overrides.iter().any(|actual| {
                actual.original_start == expected.original_start
                    && actual.event.is_some() == expected.event.is_some()
            })
        })
    {
        return Err(invalid(
            "canonical copy omitted or changed override/exclusion identities",
        ));
    }
    Ok(())
}

/// Only known server resource identity is removed. Unknown JSCalendar properties
/// remain intact; a server rejection is preferable to a silently lossy copy.
fn strip_resource_identity(object: &mut Value) -> Result<()> {
    let object = object
        .as_object_mut()
        .ok_or_else(|| invalid("source is not an Event"))?;
    for key in ["id", "blobId", "baseEventId"] {
        object.remove(key);
    }
    Ok(())
}
