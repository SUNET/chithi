//! Provider operations only. Journaling and local cache commits belong to callers.

use sha2::{Digest, Sha256};

use super::{connect, resource, CalDavCalendarBackend};
use crate::backend::calendar::{CalendarBackend, CalendarBackendCtx, CalendarCapability};
use crate::calendar::event_set::{CalendarEventSet, NativeCalendarResource};
use crate::calendar::CalendarEvent;
use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::caldav::CalDavClient;

fn failure(message: &str) -> Error {
    Error::Sync(format!("CalDAV event set: {message}"))
}

fn native<'a>(
    account: &AccountFull,
    set: &'a CalendarEventSet,
) -> Result<&'a NativeCalendarResource> {
    let native = set
        .native
        .as_ref()
        .ok_or_else(|| failure("missing native snapshot"))?;
    if set.event.account_id != account.id
        || native.protocol != "caldav"
        || set.event.remote_id.as_deref() != Some(&native.event_id)
        || native.calendar_id.is_empty()
        || native.event_id.is_empty()
    {
        return Err(failure("source ownership or resource identity mismatch"));
    }
    if resource::snapshot(native.clone(), &set.event)?.event.uid != set.event.uid {
        return Err(failure("source UID contradicts native resource"));
    }
    Ok(native)
}

async fn canonical(
    client: &CalDavClient,
    calendar: &str,
    href: &str,
    template: &CalendarEvent,
) -> Result<CalendarEventSet> {
    client.validate_resource_href(calendar, href)?;
    let received = client.get_event_at_href(href).await?;
    let set = resource::snapshot(
        NativeCalendarResource {
            protocol: "caldav".into(),
            calendar_id: calendar.into(),
            event_id: href.into(),
            revision: received.etag,
            data: received.ical_data,
        },
        template,
    )?;
    if template.uid.is_none() || template.uid != set.event.uid {
        return Err(failure("canonical resource UID does not match the source"));
    }
    Ok(set)
}

/// A weak read validator can be upgraded only if a fresh strong representation
/// is byte-identical to the original snapshot. Never silently rebase user intent.
async fn strong_revision(client: &CalDavClient, native: &NativeCalendarResource) -> Result<String> {
    client.validate_resource_href(&native.calendar_id, &native.event_id)?;
    client
        .strong_revision_for(
            &native.event_id,
            native.revision.as_deref(),
            Some(&native.data),
        )
        .await
}

pub(super) async fn fetch(
    ctx: &CalendarBackendCtx<'_>,
    account: &AccountFull,
    event: &CalendarEvent,
    calendar: &str,
) -> Result<CalendarEventSet> {
    if event.account_id != account.id {
        return Err(failure("source account mismatch"));
    }
    let href = event
        .remote_id
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| failure("missing source href"))?;
    let client = connect(ctx, account).await?;
    client.validate_resource_href(calendar, href)?;
    let received = client.get_event_at_href(href).await?;
    let mut resources = vec![NativeCalendarResource {
        protocol: "caldav".into(),
        calendar_id: calendar.into(),
        event_id: href.into(),
        revision: received.etag,
        data: received.ical_data,
    }];
    if event.recurrence_kind != crate::calendar::RecurrenceKind::Standalone {
        // No time-range filter: detached exceptions outside the view are part of
        // the authoritative set too, even on servers with nonstandard storage.
        for item in client.fetch_events(calendar).await? {
            if Some(&item.uid) != event.uid.as_ref()
                || client.same_resource_href(&item.href, href)?
            {
                continue;
            }
            client.validate_resource_href(calendar, &item.href)?;
            let received = client.get_event_at_href(&item.href).await?;
            resources.push(NativeCalendarResource {
                protocol: "caldav".into(),
                calendar_id: calendar.into(),
                event_id: item.href,
                revision: received.etag,
                data: received.ical_data,
            });
        }
    }
    resource::combine(&resources, event)
}

fn resources(before: &CalendarEventSet) -> Result<Vec<NativeCalendarResource>> {
    let main = before
        .native
        .as_ref()
        .ok_or_else(|| failure("missing native source"))?;
    let mut sources = vec![main.clone()];
    for item in &before.overrides {
        if let Some(native) = &item.native {
            if native.protocol != "caldav" || native.calendar_id != main.calendar_id {
                return Err(failure("detached source scope mismatch"));
            }
            if !sources.iter().any(|n| n.event_id == native.event_id) {
                sources.push(native.clone());
            }
        }
    }
    resource::combine(&sources, &before.event)?;
    Ok(sources)
}

pub(super) async fn update(
    ctx: &CalendarBackendCtx<'_>,
    account: &AccountFull,
    before: &CalendarEventSet,
    desired: &CalendarEventSet,
) -> Result<CalendarEventSet> {
    let source = native(account, before)?;
    if desired.event.uid != before.event.uid || desired.event.account_id != before.event.account_id
    {
        return Err(failure("update cannot change source UID or account"));
    }
    let sources = resources(before)?;
    let mut main_before = before.clone();
    let mut main_desired = desired.clone();
    let detached_positions = before
        .overrides
        .iter()
        .filter(|o| {
            o.native
                .as_ref()
                .is_some_and(|n| n.event_id != source.event_id)
        })
        .map(|o| o.original_start.as_str())
        .collect::<std::collections::HashSet<_>>();
    main_before
        .overrides
        .retain(|o| !detached_positions.contains(o.original_start.as_str()));
    main_desired
        .overrides
        .retain(|o| !detached_positions.contains(o.original_start.as_str()));
    let rewrite_before = main_before.clone();
    let rewrite_desired = main_desired.clone();
    let data =
        tokio::task::spawn_blocking(move || resource::rewrite(&rewrite_before, &rewrite_desired))
            .await
            .map_err(|e| failure(&format!("resource preparation failed: {e}")))??;
    let client = connect(ctx, account).await?;
    let mut writes = Vec::new();
    if !resource::same_set(&main_before, &main_desired) {
        writes.push((source.clone(), data));
    }
    for detached in sources.iter().skip(1) {
        if let Some(data) = resource::detached_write(detached, before, desired)? {
            writes.push((detached.clone(), data));
        }
    }
    let mut revisions = Vec::new();
    for (native, _) in &writes {
        revisions.push(strong_revision(&client, native).await?);
    }
    for ((native, data), revision) in writes.iter().zip(revisions) {
        client.put_event_at_href(&native.event_id, data, Some(&revision)).await
            .map_err(|e| failure(&format!("conditional set update failed; some resources may have been accepted; reconciliation required: {e}")))?;
    }
    if sources.len() == 1 {
        return canonical(
            &client,
            &source.calendar_id,
            &source.event_id,
            &before.event,
        )
        .await
        .map_err(|e| {
            failure(&format!(
                "PUT accepted but canonical read failed; reconciliation required: {e}"
            ))
        });
    }
    let mut canonical_resources = Vec::new();
    for source in sources {
        let received = client
            .get_event_at_href(&source.event_id)
            .await
            .map_err(|e| {
                failure(&format!(
                    "PUT accepted; canonical detached read failed: {e}"
                ))
            })?;
        canonical_resources.push(NativeCalendarResource {
            data: received.ical_data,
            revision: received.etag,
            ..source
        });
    }
    resource::combine(&canonical_resources, &before.event)
}

pub(super) async fn create(
    ctx: &CalendarBackendCtx<'_>,
    account: &AccountFull,
    calendar: &str,
    desired: &CalendarEventSet,
    operation: &str,
) -> Result<CalendarEventSet> {
    if operation.is_empty() {
        return Err(failure("missing persisted operation identity"));
    }
    let marker = format!("{:x}", Sha256::digest(operation.as_bytes()));
    let uid = format!("chithi-{marker}");
    let href = format!("{}/{}.ics", calendar.trim_end_matches('/'), uid);
    let payload_set = desired.clone();
    let payload_uid = uid.clone();
    let payload_marker = marker.clone();
    let data = tokio::task::spawn_blocking(move || {
        resource::create(&payload_set, &payload_uid, &payload_marker)
    })
    .await
    .map_err(|e| failure(&format!("resource preparation failed: {e}")))??;
    let client = connect(ctx, account).await?;
    client.validate_resource_href(calendar, &href)?;
    let put = client.put_event_at_href(&href, &data, None).await;
    // A 412 or lost PUT response is reconciled by reading the deterministic target.
    // Possession of the UID alone never authorizes overwriting an existing object.
    let received = client.get_event_at_href(&href).await.map_err(|e| {
        failure(&format!(
            "create outcome requires reconciliation (PUT: {}; GET: {e})",
            put.as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_else(|| "accepted".into())
        ))
    })?;
    resource::verify_operation(&received.ical_data, &uid, &marker)?;
    let mut template = desired.event.clone();
    template.account_id = account.id.clone();
    template.uid = Some(uid);
    resource::snapshot(
        NativeCalendarResource {
            protocol: "caldav".into(),
            calendar_id: calendar.into(),
            event_id: href,
            revision: received.etag,
            data: received.ical_data,
        },
        &template,
    )
}

pub(super) async fn delete(
    ctx: &CalendarBackendCtx<'_>,
    account: &AccountFull,
    before: &CalendarEventSet,
) -> Result<()> {
    native(account, before)?;
    let sources = resources(before)?;
    let client = connect(ctx, account).await?;
    let mut revisions = Vec::new();
    for source in &sources {
        revisions.push(strong_revision(&client, source).await?);
    }
    for (source, revision) in sources.iter().zip(revisions) {
        client.delete_event_if_match(&source.event_id, &revision).await
            .map_err(|e| failure(&format!("conditional source removal failed; reconcile partial/unknown deletion before retry: {e}")))?;
    }
    Ok(())
}

pub(super) async fn move_native(
    ctx: &CalendarBackendCtx<'_>,
    account: &AccountFull,
    before: &CalendarEventSet,
    calendar: &str,
) -> Result<CalendarCapability<CalendarEventSet>> {
    let source = native(account, before)?;
    if resources(before)?.len() > 1 {
        return Ok(CalendarCapability::Unsupported);
    }
    let client = connect(ctx, account).await?;
    let filename = source
        .event_id
        .rsplit('/')
        .next()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| failure("missing source filename"))?;
    let destination = format!("{}/{}", calendar.trim_end_matches('/'), filename);
    client.validate_resource_href(calendar, &destination)?;
    let revision = strong_revision(&client, source).await?;
    if !client
        .move_event_if_match(&source.event_id, &destination, &revision)
        .await?
    {
        return Ok(CalendarCapability::Unsupported);
    }
    canonical(&client, calendar, &destination, &before.event)
        .await
        .map(CalendarCapability::Supported)
        .map_err(|e| {
            failure(&format!(
                "MOVE accepted but canonical read failed; reconciliation required: {e}"
            ))
        })
}

pub(super) async fn ordinary_update(
    ctx: &CalendarBackendCtx<'_>,
    account: &AccountFull,
    href: &str,
    event: &CalendarEvent,
) -> Result<()> {
    let calendar: String = {
        let conn = ctx.db.reader();
        conn.query_row(
            "SELECT remote_id FROM calendars WHERE id = ?1 AND account_id = ?2",
            rusqlite::params![event.calendar_id, account.id],
            |row| row.get(0),
        )?
    };
    let mut template = event.clone();
    template.remote_id = Some(href.to_owned());
    let before = fetch(ctx, account, &template, &calendar).await?;
    if event.etag.is_some() && event.etag != before.event.etag {
        return Err(failure("ordinary update is stale; refresh before retry"));
    }
    let mut desired = before.clone();
    desired.event = template;
    CalDavCalendarBackend
        .update_event_set(ctx, account, &before, &desired)
        .await?;
    Ok(())
}
