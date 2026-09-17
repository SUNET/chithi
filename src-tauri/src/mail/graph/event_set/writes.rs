//! Write plans are validated before the first mutation. Each call returns a new
//! authoritative snapshot; the coordinator owns durable operation state.

use super::*;
use crate::calendar::event_set::{apply_event_fields, event_fields};
use crate::calendar::recurrence_identity::UpdateOccurrenceInput;
use sha2::{Digest, Sha256};

const INTENT_PROPERTY: &str =
    "String {77f84316-463d-49ec-ae0c-247e05f13645} Name ChithiCalendarIntent";

fn checked_native<'a>(
    set: &'a CalendarEventSet,
    account: &str,
) -> Result<&'a NativeCalendarResource> {
    set.validate()?;
    let resource = set
        .native
        .as_ref()
        .ok_or_else(|| invalid("missing native resource"))?;
    if resource.protocol != "graph"
        || resource.revision.as_deref() == Some("*")
        || set.event.account_id != account
        || set.event.remote_id.as_deref() != Some(resource.event_id.as_str())
    {
        return Err(invalid("foreign account or native event identity"));
    }
    let value: Value = serde_json::from_str(&resource.data).map_err(invalid)?;
    if required_string(&value, "id")? != resource.event_id
        || Some(required_string(&value, "@odata.etag")?) != resource.revision.as_deref()
        || set.event.etag != resource.revision
    {
        return Err(invalid(
            "native identity/revision disagrees with before snapshot",
        ));
    }
    event_path(&resource.calendar_id, &resource.event_id)?;
    let mut ids = HashSet::new();
    for entry in &set.overrides {
        if let Some(event) = &entry.event {
            let instance = entry
                .native
                .as_ref()
                .ok_or_else(|| invalid("before override has no native resource"))?;
            let value: Value = serde_json::from_str(&instance.data).map_err(invalid)?;
            if instance.protocol != "graph"
                || instance.revision.as_deref() == Some("*")
                || instance.calendar_id != resource.calendar_id
                || event.account_id != account
                || event.remote_id.as_deref() != Some(instance.event_id.as_str())
                || event.etag != instance.revision
                || Some(required_string(&value, "@odata.etag")?) != instance.revision.as_deref()
                || required_string(&value, "id")? != instance.event_id
                || value["seriesMasterId"] != resource.event_id
                || original(&set.event, &value)? != entry.original_start
                || !ids.insert(&instance.event_id)
            {
                return Err(invalid(
                    "before override has foreign or inconsistent native identity",
                ));
            }
        }
    }
    Ok(resource)
}

fn content_native(resource: Option<&NativeCalendarResource>) -> Result<Option<Value>> {
    resource
        .filter(|r| r.protocol == "graph")
        .map(|r| serde_json::from_str(&r.data).map_err(invalid))
        .transpose()
}

fn payload(event: &CalendarEvent, resource: Option<&NativeCalendarResource>) -> Result<Value> {
    let mut source = event.clone();
    source.ical_data = None;
    if source.recurrence_kind == RecurrenceKind::Occurrence {
        source.recurrence_kind = RecurrenceKind::Standalone;
    }
    let mut body = super::super::event_to_graph_json(&source)?;
    body["body"] =
        json!({"contentType": "text", "content": event.description.as_deref().unwrap_or("")});
    body["location"] = json!({"displayName": event.location.as_deref().unwrap_or("")});
    body["attendees"] = Value::Array(Vec::new());
    if let Some(encoded) = &event.attendees_json {
        let attendees: Vec<Value> = serde_json::from_str(encoded).map_err(invalid)?;
        let mut mapped = Vec::new();
        for attendee in attendees {
            let address = required_string(&attendee, "email")?;
            mapped.push(json!({"emailAddress": {"address": address, "name": attendee["name"].as_str().unwrap_or("")}, "type": "required"}));
        }
        body["attendees"] = json!(mapped);
    }
    body["recurrence"] = body.get("recurrence").cloned().unwrap_or(Value::Null);
    if let Some(native) = content_native(resource)? {
        // Native content is a source for retained fields, never a write target.
        if native["body"]["content"].as_str().filter(|s| !s.is_empty())
            == event.description.as_deref().filter(|s| !s.is_empty())
        {
            body["body"] = native["body"].clone();
        }
        if native["location"]["displayName"]
            .as_str()
            .filter(|s| !s.is_empty())
            == event.location.as_deref().filter(|s| !s.is_empty())
        {
            body["location"] = native["location"].clone();
            if native["locations"].is_array() {
                body["locations"] = native["locations"].clone();
            }
        }
        // Match the neutral projection before retaining required/resource/optional
        // roles and names that are absent from the shared attendee DTO.
        let projected = super::super::parse_graph_event(
            &native,
            resource.map(|r| r.calendar_id.as_str()).unwrap_or(""),
        )?
        .into_live()?;
        if projected.attendees_json == event.attendees_json {
            let mut attendees = native["attendees"]
                .as_array()
                .ok_or_else(|| invalid("native attendees missing"))?
                .clone();
            for attendee in &mut attendees {
                attendee
                    .as_object_mut()
                    .ok_or_else(|| invalid("invalid native attendee"))?
                    .remove("status");
            }
            body["attendees"] = json!(attendees);
        }
        for key in [
            "categories",
            "importance",
            "sensitivity",
            "showAs",
            "isReminderOn",
            "reminderMinutesBeforeStart",
            "responseRequested",
            "hideAttendees",
        ] {
            if let Some(value) = native.get(key) {
                body[key] = value.clone();
            }
        }
    }
    Ok(body)
}

fn diff(
    before: &CalendarEvent,
    desired: &CalendarEvent,
    resource: Option<&NativeCalendarResource>,
) -> Result<Value> {
    let patch = UpdateOccurrenceInput {
        title: (before.title != desired.title).then(|| desired.title.clone()),
        description: (before.description != desired.description)
            .then(|| desired.description.clone().unwrap_or_default()),
        location: (before.location != desired.location)
            .then(|| desired.location.clone().unwrap_or_default()),
        start_time: (before.start_time != desired.start_time).then(|| desired.start_time.clone()),
        end_time: (before.end_time != desired.end_time).then(|| desired.end_time.clone()),
        all_day: (before.all_day != desired.all_day).then_some(desired.all_day),
        timezone: (before.timezone != desired.timezone)
            .then(|| desired.timezone.clone().unwrap_or_default()),
    };
    let mut result = super::super::occurrence_patch_to_graph_json(&patch, &event_fields(desired))?;
    if before.recurrence_rule != desired.recurrence_rule
        || before.recurrence_kind != desired.recurrence_kind
        || (desired.recurrence_kind == RecurrenceKind::Series
            && (before.start_time != desired.start_time
                || before.timezone != desired.timezone
                || before.all_day != desired.all_day))
    {
        result["recurrence"] = payload(desired, resource)?["recurrence"].clone();
    }
    if before.attendees_json != desired.attendees_json {
        result["attendees"] = payload(desired, resource)?["attendees"].clone();
    }
    if before.description != desired.description {
        let native = content_native(resource)?;
        if native
            .as_ref()
            .is_some_and(|n| n["isOnlineMeeting"].as_bool() == Some(true))
        {
            // Graph requires retaining its meeting blob. The plain-text editor
            // cannot identify an arbitrary provider HTML fragment safely.
            return Err(invalid(
                "editing an online-meeting body requires preserving its native meeting blob",
            ));
        }
    }
    Ok(result)
}

fn local_date(event: &CalendarEvent, value: &str) -> Result<NaiveDate> {
    if value.len() == 10 {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(invalid)
    } else {
        Ok(instant(value)?
            .with_timezone(&zone(event.timezone.as_deref().unwrap_or("UTC"))?)
            .date_naive())
    }
}

fn effective_fields(
    set: &CalendarEventSet,
    original: &str,
) -> Result<Option<crate::calendar::recurrence_identity::OccurrenceFields>> {
    if let Some(entry) = set.overrides.iter().find(|o| o.original_start == original) {
        return Ok(entry.event.as_ref().map(event_fields));
    }
    Ok(Some(simple_recurrence::resolve(&set.event, original)?))
}

fn field_instant(
    fields: &crate::calendar::recurrence_identity::OccurrenceFields,
    start: bool,
) -> Result<DateTime<Utc>> {
    let value = if start {
        &fields.start_time
    } else {
        &fields.end_time
    };
    if !fields.all_day {
        return instant(value);
    }
    let midnight = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(invalid)?
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| invalid("invalid midnight"))?;
    zone(fields.timezone.as_deref().unwrap_or("UTC"))?
        .from_local_datetime(&midnight)
        .single()
        .map(|time| time.with_timezone(&Utc))
        .ok_or_else(|| invalid("all-day boundary is ambiguous or nonexistent"))
}

fn preflight(desired: &CalendarEventSet) -> Result<()> {
    desired.validate()?;
    payload(&desired.event, desired.native.as_ref())?;
    if desired.event.recurrence_kind == RecurrenceKind::Series {
        simple_recurrence::normalize_rule(
            desired
                .event
                .recurrence_rule
                .as_deref()
                .ok_or_else(|| invalid("missing rule"))?,
            &desired.event,
        )?;
    }
    for exception in &desired.overrides {
        simple_recurrence::resolve(&desired.event, &exception.original_start)?;
        if let Some(event) = &exception.event {
            payload(event, exception.native.as_ref())?;
            let index =
                simple_recurrence::occurrence_index(&desired.event, &exception.original_start)?;
            let effective = local_date(&desired.event, &event.start_time)?;
            let effective_fields_current = event_fields(event);
            if index > 0 {
                let previous = simple_recurrence::position_at(&desired.event, index - 1)?;
                if effective <= local_date(&desired.event, &previous)? {
                    return Err(invalid("ErrorOccurrenceCrossingBoundary: occurrence moves to/before the previous occurrence day"));
                }
                if let Some(previous) = effective_fields(desired, &previous)? {
                    if field_instant(&effective_fields_current, true)?
                        < field_instant(&previous, false)?
                    {
                        return Err(invalid("ErrorOccurrenceCrossingBoundary: overlapping previous effective occurrence"));
                    }
                }
            }
            // Remove only the end bound to locate the next pattern position.
            // Then check membership against the original bound explicitly.
            let mut unbounded = desired.event.clone();
            unbounded.recurrence_rule = Some(
                simple_recurrence::normalize_rule(
                    desired
                        .event
                        .recurrence_rule
                        .as_deref()
                        .ok_or_else(|| invalid("missing rule"))?,
                    &desired.event,
                )?
                .split(';')
                .filter(|p| !p.starts_with("COUNT=") && !p.starts_with("UNTIL="))
                .collect::<Vec<_>>()
                .join(";"),
            );
            let next = simple_recurrence::position_at(
                &unbounded,
                index
                    .checked_add(1)
                    .ok_or_else(|| invalid("occurrence index overflow"))?,
            )?;
            let next_fields = simple_recurrence::expand(
                &desired.event,
                &next,
                &utc(instant_or_date(&next)?
                    .checked_add_signed(Duration::days(2))
                    .ok_or_else(|| invalid("date overflow"))?),
                4,
            )?;
            if next_fields
                .occurrences
                .iter()
                .any(|o| o.original_start == next)
            {
                if effective >= local_date(&desired.event, &next)? {
                    return Err(invalid("ErrorOccurrenceCrossingBoundary: occurrence moves to/after the next occurrence day"));
                }
                if let Some(next) = effective_fields(desired, &next)? {
                    if field_instant(&effective_fields_current, false)?
                        > field_instant(&next, true)?
                    {
                        return Err(invalid("ErrorOccurrenceCrossingBoundary: overlapping next effective occurrence"));
                    }
                }
            }
        }
    }
    Ok(())
}

fn instant_or_date(value: &str) -> Result<DateTime<Utc>> {
    if value.len() == 10 {
        Ok(NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map_err(invalid)?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| invalid("invalid midnight"))?
            .and_utc())
    } else {
        instant(value)
    }
}

impl GraphClient {
    async fn instance_at(
        &self,
        calendar: &str,
        set: &CalendarEventSet,
        position: &str,
    ) -> Result<(CalendarEvent, NativeCalendarResource)> {
        if let Some(existing) = set.overrides.iter().find(|o| o.original_start == position) {
            match (&existing.event, &existing.native) {
                (Some(event), Some(resource)) => return Ok((event.clone(), resource.clone())),
                (None, _) => return Err(invalid("Graph cannot address a previously cancelled occurrence; recreating the series requires a separate transfer operation")),
                _ => return Err(invalid("live override has no native identity")),
            }
        }
        simple_recurrence::resolve(&set.event, position)?;
        let native = set
            .native
            .as_ref()
            .ok_or_else(|| invalid("missing master identity"))?;
        let anchor = instant_or_date(position)?;
        let start = utc(anchor
            .checked_sub_signed(Duration::days(2))
            .ok_or_else(|| invalid("instance window underflow"))?);
        let end = utc(anchor
            .checked_add_signed(Duration::days(2))
            .ok_or_else(|| invalid("instance window overflow"))?);
        let path = format!("{}/instances", event_path(calendar, &native.event_id)?);
        let select = format!("{CALENDAR_EVENT_SELECT},{EXTRA_SELECT}");
        let page = self
            .calendar_set_request(
                Method::GET,
                &path,
                &[
                    ("startDateTime", &start),
                    ("endDateTime", &end),
                    ("$select", &select),
                ],
                None,
                None,
            )
            .await?;
        let mut found = None;
        for value in self.set_pages(page, &path).await? {
            if value["seriesMasterId"] != native.event_id
                || !matches!(value["type"].as_str(), Some("occurrence" | "exception"))
            {
                return Err(invalid("instance belongs to another series"));
            }
            if original(&set.event, &value)? == position {
                if found.is_some() {
                    return Err(invalid("multiple instances at one originalStart"));
                }
                found = Some((
                    canonical(&value, &set.event, calendar)?,
                    super::native(&value, calendar)?,
                ));
            }
        }
        found.ok_or_else(|| invalid("native instance API returned no exact originalStart"))
    }

    async fn apply_overrides(
        &self,
        mut current: CalendarEventSet,
        desired: &CalendarEventSet,
        replay: bool,
    ) -> Result<CalendarEventSet> {
        let resource = current
            .native
            .clone()
            .ok_or_else(|| invalid("missing master identity"))?;
        let calendar = &resource.calendar_id;
        // Missing desired live overrides are restored to generated content. A
        // cancelled slot cannot be restored with Graph's instance API.
        let mut targets = desired.overrides.clone();
        for old in &current.overrides {
            if !targets
                .iter()
                .any(|o| o.original_start == old.original_start)
            {
                if old.event.is_none() {
                    return Err(invalid(
                        "Graph cannot restore a cancelled occurrence in place",
                    ));
                }
                let fields = simple_recurrence::resolve(&desired.event, &old.original_start)?;
                let mut event = desired.event.clone();
                apply_event_fields(&mut event, &fields);
                event.recurrence_rule = None;
                event.recurrence_kind = RecurrenceKind::Occurrence;
                targets.push(CalendarOverride {
                    original_start: old.original_start.clone(),
                    event: Some(event),
                    native: None,
                });
            }
        }
        for target in targets {
            if target.event.is_none()
                && current
                    .overrides
                    .iter()
                    .any(|o| o.original_start == target.original_start && o.event.is_none())
            {
                continue;
            }
            let (before, native) = self
                .instance_at(calendar, &current, &target.original_start)
                .await?;
            let path = event_path(calendar, &native.event_id)?;
            log::debug!(
                "Graph calendar action resolved occurrence: calendar_id={} series_id={} occurrence_id={}",
                calendar,
                resource.event_id,
                native.event_id
            );
            let etag = native
                .revision
                .as_deref()
                .ok_or_else(|| invalid("missing occurrence etag"))?;
            if let Some(event) = &target.event {
                let patch = if replay {
                    let mut patch = payload(event, target.native.as_ref())?;
                    patch
                        .as_object_mut()
                        .ok_or_else(|| invalid("invalid override payload"))?
                        .remove("recurrence");
                    patch
                } else {
                    diff(&before, event, target.native.as_ref().or(Some(&native)))?
                };
                if patch.as_object().is_some_and(|o| !o.is_empty()) {
                    self.calendar_set_request(Method::PATCH, &path, &[], Some(&patch), Some(etag))
                        .await?;
                } else {
                    continue;
                }
            } else {
                self.calendar_set_request(Method::DELETE, &path, &[], None, Some(etag))
                    .await?;
            }
            log::debug!(
                "Graph calendar action occurrence mutation accepted: calendar_id={} series_id={} occurrence_id={}",
                calendar,
                resource.event_id,
                native.event_id
            );
            // The next instance revision must come from the post-mutation master.
            current = match self
                .fetch_calendar_event_set(calendar, &resource.event_id, &current.event)
                .await
            {
                Ok(current) => {
                    log::debug!(
                        "Graph calendar action canonical re-fetch completed: calendar_id={} series_id={}",
                        calendar,
                        resource.event_id
                    );
                    current
                }
                Err(error) => {
                    log::warn!(
                        "Graph calendar action canonical re-fetch failed after occurrence mutation: calendar_id={} series_id={} occurrence_id={} error={error}",
                        calendar,
                        resource.event_id,
                        native.event_id
                    );
                    return Err(error);
                }
            };
        }
        Ok(current)
    }

    async fn verify_creation_master(
        &self,
        current: &CalendarEventSet,
        desired: &CalendarEventSet,
    ) -> Result<()> {
        let actual = payload(&current.event, current.native.as_ref())?;
        let expected = payload(&desired.event, desired.native.as_ref())?;
        for field in ["subject", "isAllDay", "attendees"] {
            if actual[field] != expected[field] {
                return Err(invalid(format!(
                    "created/reconciled master differs in {field}"
                )));
            }
        }
        for field in ["start", "end"] {
            if native_time(&actual[field])? != native_time(&expected[field])?
                || zone(required_string(&actual[field], "timeZone")?)?
                    != zone(required_string(&expected[field], "timeZone")?)?
            {
                return Err(invalid(format!(
                    "created/reconciled master differs in {field}"
                )));
            }
        }
        let normalize_recurrence = |mut recurrence: Value| -> Result<Value> {
            if !recurrence.is_null() {
                let tz = zone(required_string(&recurrence["range"], "recurrenceTimeZone")?)?;
                recurrence["range"]["recurrenceTimeZone"] = json!(tz.name());
            }
            Ok(recurrence)
        };
        if normalize_recurrence(actual["recurrence"].clone())?
            != normalize_recurrence(expected["recurrence"].clone())?
        {
            return Err(invalid("created/reconciled master recurrence differs"));
        }
        if actual["location"]["displayName"] != expected["location"]["displayName"] {
            return Err(invalid("created/reconciled master location differs"));
        }
        if actual["body"]["content"] != expected["body"]["content"] {
            if expected["body"]["contentType"] != "text" {
                return Err(invalid("created/reconciled master HTML body differs"));
            }
            let native = current
                .native
                .as_ref()
                .ok_or_else(|| invalid("missing created identity"))?;
            let url = self.calendar_set_url(&event_path(&native.calendar_id, &native.event_id)?)?;
            let response = self.send_with_retry(|| self.http.get(url.clone())
                .bearer_auth(&self.access_token)
                .header("Prefer", "outlook.timezone=\"UTC\", IdType=\"ImmutableId\", outlook.body-content-type=\"text\"")
                .query(&[("$select", "id,body")]), "verify created body", true).await?;
            if !response.status().is_success() {
                return Err(invalid(format!(
                    "body verification returned {}",
                    response.status()
                )));
            }
            let value: Value = response.json().await.map_err(invalid)?;
            if value["id"] != native.event_id
                || value["@odata.etag"].as_str() != native.revision.as_deref()
                || value["body"]["contentType"] != "text"
                || value["body"]["content"] != expected["body"]["content"]
            {
                return Err(invalid("created/reconciled master text body differs"));
            }
        }
        Ok(())
    }

    async fn verify_created_overrides(
        &self,
        current: &CalendarEventSet,
        desired: &CalendarEventSet,
    ) -> Result<()> {
        if current.overrides.len() != desired.overrides.len() {
            return Err(invalid("created set has unexpected or missing overrides"));
        }
        if !desired.overrides.is_empty() {
            self.verify_creation_master(current, desired).await?;
        }
        for expected in &desired.overrides {
            let actual = current
                .overrides
                .iter()
                .find(|o| o.original_start == expected.original_start)
                .ok_or_else(|| invalid("created set is missing an original position"))?;
            match (&actual.event, &expected.event) {
                (None, None) => {}
                (Some(_), Some(_)) => {
                    self.verify_creation_master(
                        &current.standalone_at(&actual.original_start)?,
                        &desired.standalone_at(&expected.original_start)?,
                    )
                    .await?;
                }
                _ => return Err(invalid("created override/exclusion state differs")),
            }
        }
        Ok(())
    }

    pub(crate) async fn update_calendar_event_set(
        &self,
        account: &str,
        before: &CalendarEventSet,
        desired: &CalendarEventSet,
    ) -> Result<CalendarEventSet> {
        let resource = checked_native(before, account)?;
        if desired.event.account_id != account
            || desired.native != before.native
            || desired.event.remote_id != before.event.remote_id
        {
            return Err(invalid(
                "desired snapshot changed account or native master identity",
            ));
        }
        preflight(desired)?;
        for old in &before.overrides {
            if old.event.is_none()
                && !desired
                    .overrides
                    .iter()
                    .any(|o| o.original_start == old.original_start && o.event.is_none())
                && desired.event.recurrence_rule == before.event.recurrence_rule
            {
                return Err(invalid(
                    "Graph cannot restore a cancelled occurrence in place",
                ));
            }
        }
        let patch = diff(&before.event, &desired.event, before.native.as_ref())?;
        let replay = patch.get("recurrence").is_some();
        // Validate all occurrence diffs, including body restrictions, before a
        // master patch can send notifications or discard exception state.
        for target in &desired.overrides {
            if let Some(event) = &target.event {
                if let Some(old) = before
                    .overrides
                    .iter()
                    .find(|o| o.original_start == target.original_start)
                {
                    if let Some(previous) = &old.event {
                        diff(previous, event, old.native.as_ref())?;
                    }
                }
            }
        }
        let mut current = before.clone();
        if patch.as_object().is_some_and(|o| !o.is_empty()) {
            self.calendar_set_request(
                Method::PATCH,
                &event_path(&resource.calendar_id, &resource.event_id)?,
                &[],
                Some(&patch),
                resource.revision.as_deref(),
            )
            .await?;
            current = self
                .fetch_calendar_event_set(&resource.calendar_id, &resource.event_id, &before.event)
                .await?;
        }
        self.apply_overrides(current, desired, replay).await
    }

    pub(crate) async fn delete_calendar_event_set(
        &self,
        account: &str,
        before: &CalendarEventSet,
    ) -> Result<()> {
        let native = checked_native(before, account)?;
        self.calendar_set_request(
            Method::DELETE,
            &event_path(&native.calendar_id, &native.event_id)?,
            &[],
            None,
            native.revision.as_deref(),
        )
        .await?;
        Ok(())
    }

    async fn reconcile_creation(
        &self,
        calendar: &str,
        operation: &str,
        intent: &str,
        template: &CalendarEvent,
    ) -> Result<Option<CalendarEventSet>> {
        let path = collection_path(calendar)?;
        let filter = format!("singleValueExtendedProperties/Any(p: p/id eq '{OP_PROPERTY}' and p/value eq '{operation}')");
        let expand = format!("singleValueExtendedProperties($filter=id eq '{OP_PROPERTY}' or id eq '{INTENT_PROPERTY}')");
        let page = self
            .calendar_set_request(
                Method::GET,
                &path,
                &[
                    ("$filter", &filter),
                    ("$expand", &expand),
                    ("$select", "id,transactionId"),
                ],
                None,
                None,
            )
            .await?;
        let items = self.set_pages(page, &path).await?;
        if items.len() > 1 {
            return Err(invalid(
                "multiple events have the same creation operation tag",
            ));
        }
        if let Some(value) = items.first() {
            let properties = value["singleValueExtendedProperties"]
                .as_array()
                .ok_or_else(|| invalid("creation reconciliation omitted tags"))?;
            for (id, expected) in [(OP_PROPERTY, operation), (INTENT_PROPERTY, intent)] {
                let matching: Vec<_> = properties
                    .iter()
                    .filter(|p| p["id"].as_str() == Some(id))
                    .collect();
                if matching.len() != 1 || matching[0]["value"].as_str() != Some(expected) {
                    return Err(invalid("creation operation tag or intent does not match"));
                }
            }
            if value["transactionId"].as_str() != Some(operation) {
                return Err(invalid("creation transactionId differs"));
            }
            return Ok(Some(
                self.fetch_calendar_event_set(calendar, required_string(value, "id")?, template)
                    .await?,
            ));
        }
        Ok(None)
    }

    pub(crate) async fn create_calendar_event_set(
        &self,
        account: &str,
        calendar: &str,
        desired: &CalendarEventSet,
        operation_id: &str,
    ) -> Result<CalendarEventSet> {
        preflight(desired)?;
        if operation_id.trim().is_empty() {
            return Err(invalid("creation requires a persisted operation ID"));
        }
        let operation = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(account, calendar, operation_id)).map_err(invalid)?
            )
        );
        let mut body = payload(&desired.event, desired.native.as_ref())?;
        let overrides = desired.overrides.iter().map(|o| Ok(json!({"original": o.original_start, "event": o.event.as_ref().map(|e| payload(e, o.native.as_ref())).transpose()?}))).collect::<Result<Vec<_>>>()?;
        let intent = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&json!({"master": body, "overrides": overrides}))
                    .map_err(invalid)?
            )
        );
        let mut template = desired.event.clone();
        template.account_id = account.into();
        template.remote_id = None;
        template.etag = None;
        if let Some(existing) = self
            .reconcile_creation(calendar, &operation, &intent, &template)
            .await?
        {
            self.verify_creation_master(&existing, desired).await?;
            let created = self.apply_overrides(existing, desired, false).await?;
            self.verify_created_overrides(&created, desired).await?;
            return Ok(created);
        }
        body["transactionId"] = json!(operation);
        body["singleValueExtendedProperties"] = json!([{"id": OP_PROPERTY, "value": operation}, {"id": INTENT_PROPERTY, "value": intent}]);
        let created = self
            .calendar_set_request(
                Method::POST,
                &collection_path(calendar)?,
                &[],
                Some(&body),
                None,
            )
            .await;
        let current = match created {
            Ok(value) => match required_string(&value, "id") {
                Ok(id) => {
                    self.fetch_calendar_event_set(calendar, id, &template)
                        .await?
                }
                Err(error) => self
                    .reconcile_creation(calendar, &operation, &intent, &template)
                    .await?
                    .ok_or(error)?,
            },
            Err(error) => self
                .reconcile_creation(calendar, &operation, &intent, &template)
                .await?
                .ok_or(error)?,
        };
        self.verify_creation_master(&current, desired).await?;
        let created = self.apply_overrides(current, desired, false).await?;
        self.verify_created_overrides(&created, desired).await?;
        Ok(created)
    }
}
