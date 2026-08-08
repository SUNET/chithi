use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::ops::flags::{subtract_flag_mutation, FlagMutation, FlagTarget};
use crate::ops::queue::MailOp;

/// Replay order matching Thunderbird's `nsImapOfflineSync`:
/// flags (0) -> moves (1) -> copies (2) -> deletes (3).
/// `send` is independent of folder state, so it goes last (4).
fn replay_order(action_type: &str) -> i32 {
    match action_type {
        "set_flags" => 0,
        "move" => 1,
        "copy" => 2,
        "delete" => 3,
        "send" => 4,
        _ => 5,
    }
}

/// An entry from the outbox table.
#[derive(Debug)]
pub struct OutboxEntry {
    pub id: i64,
    pub account_id: String,
    pub action_type: String,
    pub payload_json: String,
    pub status: String,
    pub retry_count: i32,
    pub error_message: Option<String>,
}

/// Write a failed operation to the outbox for later replay.
///
/// Replay order (flags -> moves -> copies -> deletes) is computed at read
/// time in `get_pending_ops` via `replay_order()`, so no extra column or
/// workaround is needed here.
pub fn queue_offline_op(
    conn: &Connection,
    account_id: &str,
    action_type: &str,
    payload: &serde_json::Value,
) -> Result<i64> {
    queue_offline_op_with_status(conn, account_id, action_type, payload, "pending")
}

/// Like `queue_offline_op` but inserts with an explicit initial status.
///
/// `compose::send_message` uses this with `'sending'` to claim the row
/// for the in-process spawn task, keeping the worker's replay loop from
/// picking it up while the first attempt is still in flight.
pub fn queue_offline_op_with_status(
    conn: &Connection,
    account_id: &str,
    action_type: &str,
    payload: &serde_json::Value,
    initial_status: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO outbox (account_id, action_type, payload_json, status, retry_count, error_message)
         VALUES (?1, ?2, ?3, ?4, 0, NULL)",
        rusqlite::params![account_id, action_type, payload.to_string(), initial_status],
    )
    .map_err(Error::Database)?;
    let id = conn.last_insert_rowid();
    Ok(id)
}

/// Flip a 'sending' row back to 'pending' so the worker will retry it
/// on the next sync.
pub fn mark_pending(conn: &Connection, outbox_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET status = 'pending' WHERE id = ?1",
        rusqlite::params![outbox_id],
    )
    .map_err(Error::Database)?;
    Ok(())
}

/// On app startup, revive any rows left as 'sending' from a previous run.
/// The owning task is gone, so the message would otherwise be invisible
/// to both the user and the worker. Returns how many rows were revived.
pub fn revive_stuck_sending(conn: &Connection) -> Result<usize> {
    let count = conn
        .execute(
            "UPDATE outbox SET status = 'pending' WHERE status = 'sending'",
            [],
        )
        .map_err(Error::Database)?;
    if count > 0 {
        log::info!("Revived {} outbox row(s) stuck in 'sending'", count);
    }
    Ok(count)
}

/// Get all pending operations for an account, ordered by replay priority.
pub fn get_pending_ops(conn: &Connection, account_id: &str) -> Result<Vec<OutboxEntry>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, account_id, action_type, payload_json, status, retry_count, error_message
             FROM outbox
             WHERE account_id = ?1 AND status = 'pending'
             ORDER BY id ASC",
        )
        .map_err(Error::Database)?;

    let mut entries: Vec<OutboxEntry> = stmt
        .query_map(rusqlite::params![account_id], |row| {
            Ok(OutboxEntry {
                id: row.get(0)?,
                account_id: row.get(1)?,
                action_type: row.get(2)?,
                payload_json: row.get(3)?,
                status: row.get(4)?,
                retry_count: row.get(5)?,
                error_message: row.get(6)?,
            })
        })
        .map_err(Error::Database)?
        .filter_map(|r| r.ok())
        .collect();

    // Sort by replay order: flags -> moves -> copies -> deletes.
    // Use stable ordering: break ties by id to preserve insertion order.
    entries.sort_by(|a, b| {
        replay_order(&a.action_type)
            .cmp(&replay_order(&b.action_type))
            .then(a.id.cmp(&b.id))
    });
    Ok(entries)
}

/// Mark an outbox entry as completed (will be deleted).
pub fn mark_completed(conn: &Connection, outbox_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM outbox WHERE id = ?1",
        rusqlite::params![outbox_id],
    )
    .map_err(Error::Database)?;
    Ok(())
}

/// Mark an outbox entry as failed, incrementing retry count.
pub fn mark_failed(conn: &Connection, outbox_id: i64, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET retry_count = retry_count + 1, error_message = ?1
         WHERE id = ?2",
        rusqlite::params![error, outbox_id],
    )
    .map_err(Error::Database)?;
    Ok(())
}

/// Mark an outbox entry as dead (too many retries).
pub fn mark_dead(conn: &Connection, outbox_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET status = 'dead' WHERE id = ?1",
        rusqlite::params![outbox_id],
    )
    .map_err(Error::Database)?;
    Ok(())
}

/// Get dead operations (retry_count >= max_retries) for surfacing to user.
pub fn get_dead_ops(conn: &Connection, account_id: &str) -> Result<Vec<OutboxEntry>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, account_id, action_type, payload_json, status, retry_count, error_message
             FROM outbox
             WHERE account_id = ?1 AND status = 'dead'
             ORDER BY id ASC",
        )
        .map_err(Error::Database)?;

    let entries = stmt
        .query_map(rusqlite::params![account_id], |row| {
            Ok(OutboxEntry {
                id: row.get(0)?,
                account_id: row.get(1)?,
                action_type: row.get(2)?,
                payload_json: row.get(3)?,
                status: row.get(4)?,
                retry_count: row.get(5)?,
                error_message: row.get(6)?,
            })
        })
        .map_err(Error::Database)?
        .filter_map(|r| r.ok())
        .collect();

    Ok(entries)
}

/// Convert a MailOp to an action_type string and JSON payload for outbox storage.
pub fn mail_op_to_outbox(op: &MailOp) -> Option<(&'static str, serde_json::Value)> {
    match op {
        MailOp::MoveMessages {
            by_folder,
            target_folder,
        } => Some((
            "move",
            serde_json::json!({
                "by_folder": by_folder,
                "target_folder": target_folder,
            }),
        )),
        MailOp::DeleteMessages { by_folder } => {
            Some(("delete", serde_json::json!({ "by_folder": by_folder })))
        }
        MailOp::SetFlags { mutations } => {
            let payload = if let [mutation] = mutations.as_slice() {
                flag_mutation_to_json(mutation)
            } else {
                serde_json::json!({
                    "mutations": mutations
                        .iter()
                        .map(flag_mutation_to_json)
                        .collect::<Vec<_>>(),
                })
            };
            Some(("set_flags", payload))
        }
        MailOp::CopyMessages {
            by_folder,
            target_folder,
        } => Some((
            "copy",
            serde_json::json!({
                "by_folder": by_folder,
                "target_folder": target_folder,
            }),
        )),
        MailOp::SendRaw {
            raw_message,
            from,
            to,
            cc,
            bcc,
            subject,
        } => {
            use base64::Engine;
            let raw_b64 = base64::engine::general_purpose::STANDARD.encode(raw_message);
            Some((
                "send",
                serde_json::json!({
                    "raw_message_b64": raw_b64,
                    "from": from,
                    "to": to,
                    "cc": cc,
                    "bcc": bcc,
                    "subject": subject,
                }),
            ))
        }
        // Sync ops are not queued offline
        _ => None,
    }
}

/// Remove portions of older pending flag operations superseded by newer user
/// intent while retaining all replacements in their original outbox row.
pub fn supersede_pending_flag_ops(
    conn: &Connection,
    account_id: &str,
    new_mutations: &[FlagMutation],
) -> Result<()> {
    let pending = get_pending_ops(conn, account_id)?;
    let transaction = conn.unchecked_transaction().map_err(Error::Database)?;

    for entry in pending
        .into_iter()
        .filter(|entry| entry.action_type == "set_flags")
    {
        let Some(MailOp::SetFlags { mutations }) = outbox_to_mail_op(&entry) else {
            continue;
        };
        let remaining = new_mutations.iter().fold(mutations, |current, newer| {
            current
                .into_iter()
                .flat_map(|older| subtract_flag_mutation(older, newer))
                .collect()
        });
        if remaining.is_empty() {
            transaction
                .execute(
                    "DELETE FROM outbox WHERE id = ?1",
                    rusqlite::params![entry.id],
                )
                .map_err(Error::Database)?;
            continue;
        }

        let payload = mail_op_to_outbox(&MailOp::SetFlags {
            mutations: remaining,
        })
        .expect("SetFlags is serializable")
        .1
        .to_string();
        transaction
            .execute(
                "UPDATE outbox SET payload_json = ?1 WHERE id = ?2",
                rusqlite::params![payload, entry.id],
            )
            .map_err(Error::Database)?;
    }

    transaction.commit().map_err(Error::Database)
}

/// Validate that a folder path doesn't contain characters that could be
/// injected into IMAP commands (null bytes, bare newlines).
fn validate_folder_paths(by_folder: &std::collections::HashMap<String, Vec<u32>>) -> bool {
    by_folder
        .keys()
        .all(|path| !path.contains('\0') && !path.contains('\n') && !path.contains('\r'))
}

fn flag_mutation_to_json(mutation: &FlagMutation) -> serde_json::Value {
    match &mutation.target {
        FlagTarget::Messages(message_refs)
            if message_refs.iter().all(|message_ref| {
                matches!(
                    message_ref,
                    crate::mail::compat::BackendMessageRef::Imap { .. }
                )
            }) =>
        {
            let mut by_folder = std::collections::HashMap::<String, Vec<u32>>::new();
            for message_ref in message_refs {
                if let crate::mail::compat::BackendMessageRef::Imap { folder_path, uid } =
                    message_ref
                {
                    by_folder.entry(folder_path.clone()).or_default().push(*uid);
                }
            }
            serde_json::json!({
                "by_folder": by_folder,
                "flags": mutation.flags,
                "add": mutation.add,
            })
        }
        FlagTarget::Messages(message_refs) => serde_json::json!({
            "message_refs": message_refs
                .iter()
                .map(message_ref_to_json)
                .collect::<Vec<_>>(),
            "flags": mutation.flags,
            "add": mutation.add,
        }),
        FlagTarget::AllMessagesInFolders {
            folder_paths,
            excluded_refs,
        } => serde_json::json!({
            "target": {
                "kind": "all_messages_in_folders",
                "folder_paths": folder_paths,
                "excluded_refs": excluded_refs
                    .iter()
                    .map(message_ref_to_json)
                    .collect::<Vec<_>>(),
            },
            "flags": mutation.flags,
            "add": mutation.add,
        }),
    }
}

fn flag_mutation_from_json(value: &serde_json::Value) -> Option<FlagMutation> {
    let flags: Vec<String> = serde_json::from_value(value.get("flags")?.clone()).ok()?;
    let add = value.get("add")?.as_bool()?;
    let target = if let Some(target) = value.get("target") {
        if target.get("kind")?.as_str()? != "all_messages_in_folders"
            || !add
            || flags.len() != 1
            || !flags[0].eq_ignore_ascii_case("seen")
        {
            log::warn!("outbox_to_mail_op: rejected unsupported bulk flag target");
            return None;
        }
        let folder_paths: Vec<String> =
            serde_json::from_value(target.get("folder_paths")?.clone()).ok()?;
        if !folder_paths.iter().all(|path| valid_folder_path(path)) {
            log::warn!("outbox_to_mail_op: rejected bulk target with invalid folder path");
            return None;
        }
        let excluded_refs = target
            .get("excluded_refs")?
            .as_array()?
            .iter()
            .map(message_ref_from_json)
            .collect::<Option<Vec<_>>>()?;
        if !excluded_refs.iter().all(|message_ref| {
            matches!(
                message_ref,
                crate::mail::compat::BackendMessageRef::Imap { folder_path, .. }
                    if folder_paths.contains(folder_path)
            )
        }) {
            log::warn!("outbox_to_mail_op: rejected invalid bulk target exclusion");
            return None;
        }
        FlagTarget::AllMessagesInFolders {
            folder_paths,
            excluded_refs,
        }
    } else if let Some(by_folder) = value.get("by_folder") {
        let by_folder: std::collections::HashMap<String, Vec<u32>> =
            serde_json::from_value(by_folder.clone()).ok()?;
        if !validate_folder_paths(&by_folder) {
            log::warn!("outbox_to_mail_op: rejected set_flags op with invalid folder path");
            return None;
        }
        FlagTarget::Messages(
            by_folder
                .into_iter()
                .flat_map(|(folder_path, uids)| {
                    uids.into_iter().map(move |uid| {
                        crate::mail::compat::BackendMessageRef::imap(folder_path.clone(), uid)
                    })
                })
                .collect(),
        )
    } else {
        FlagTarget::Messages(
            value
                .get("message_refs")?
                .as_array()?
                .iter()
                .map(message_ref_from_json)
                .collect::<Option<Vec<_>>>()?,
        )
    };
    Some(FlagMutation { target, flags, add })
}

fn valid_folder_path(path: &str) -> bool {
    !path.contains('\0') && !path.contains('\n') && !path.contains('\r')
}

fn message_ref_to_json(message_ref: &crate::mail::compat::BackendMessageRef) -> serde_json::Value {
    use crate::mail::compat::BackendMessageRef;

    match message_ref {
        BackendMessageRef::Imap { folder_path, uid } => serde_json::json!({
            "kind": "imap",
            "folder_path": folder_path,
            "uid": uid,
        }),
        BackendMessageRef::Jmap {
            mailbox_id,
            email_id,
        } => serde_json::json!({
            "kind": "jmap",
            "mailbox_id": mailbox_id,
            "email_id": email_id,
        }),
        BackendMessageRef::Graph { item_id } => serde_json::json!({
            "kind": "graph",
            "item_id": item_id,
        }),
    }
}

fn message_ref_from_json(
    value: &serde_json::Value,
) -> Option<crate::mail::compat::BackendMessageRef> {
    use crate::mail::compat::BackendMessageRef;

    match value.get("kind")?.as_str()? {
        "imap" => Some(BackendMessageRef::imap(
            value.get("folder_path")?.as_str()?,
            u32::try_from(value.get("uid")?.as_u64()?).ok()?,
        )),
        "jmap" => Some(BackendMessageRef::jmap(
            value.get("mailbox_id")?.as_str()?,
            value.get("email_id")?.as_str()?,
        )),
        "graph" => Some(BackendMessageRef::graph(value.get("item_id")?.as_str()?)),
        _ => None,
    }
}

/// Convert an outbox entry back to a MailOp for replay.
pub fn outbox_to_mail_op(entry: &OutboxEntry) -> Option<MailOp> {
    let payload: serde_json::Value = serde_json::from_str(&entry.payload_json).ok()?;
    match entry.action_type.as_str() {
        "move" => {
            let by_folder: std::collections::HashMap<String, Vec<u32>> =
                serde_json::from_value(payload.get("by_folder")?.clone()).ok()?;
            if !validate_folder_paths(&by_folder) {
                log::warn!("outbox_to_mail_op: rejected move op with invalid folder path");
                return None;
            }
            let target_folder = payload.get("target_folder")?.as_str()?.to_string();
            if target_folder.contains('\0') || target_folder.contains('\n') {
                log::warn!("outbox_to_mail_op: rejected move op with invalid target folder");
                return None;
            }
            Some(MailOp::MoveMessages {
                by_folder,
                target_folder,
            })
        }
        "delete" => {
            let by_folder: std::collections::HashMap<String, Vec<u32>> =
                serde_json::from_value(payload.get("by_folder")?.clone()).ok()?;
            if !validate_folder_paths(&by_folder) {
                log::warn!("outbox_to_mail_op: rejected delete op with invalid folder path");
                return None;
            }
            Some(MailOp::DeleteMessages { by_folder })
        }
        "set_flags" => {
            let mutations = if let Some(mutations) = payload.get("mutations") {
                mutations
                    .as_array()?
                    .iter()
                    .map(flag_mutation_from_json)
                    .collect::<Option<Vec<_>>>()?
            } else {
                vec![flag_mutation_from_json(&payload)?]
            };
            Some(MailOp::SetFlags { mutations })
        }
        "copy" => {
            let by_folder = serde_json::from_value(payload.get("by_folder")?.clone()).ok()?;
            let target_folder = payload.get("target_folder")?.as_str()?.to_string();
            Some(MailOp::CopyMessages {
                by_folder,
                target_folder,
            })
        }
        "send" => {
            use base64::Engine;
            let raw_b64 = payload.get("raw_message_b64")?.as_str()?;
            let raw_message = base64::engine::general_purpose::STANDARD
                .decode(raw_b64)
                .ok()?;
            let from = payload.get("from")?.as_str()?.to_string();
            let to: Vec<String> =
                serde_json::from_value(payload.get("to").cloned().unwrap_or_default())
                    .unwrap_or_default();
            let cc: Vec<String> =
                serde_json::from_value(payload.get("cc").cloned().unwrap_or_default())
                    .unwrap_or_default();
            let bcc: Vec<String> =
                serde_json::from_value(payload.get("bcc").cloned().unwrap_or_default())
                    .unwrap_or_default();
            let subject = payload
                .get("subject")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if to.is_empty() && cc.is_empty() && bcc.is_empty() {
                log::warn!("outbox_to_mail_op: rejected send op with no recipients");
                return None;
            }
            Some(MailOp::SendRaw {
                raw_message,
                from,
                to,
                cc,
                bcc,
                subject,
            })
        }
        _ => None,
    }
}

const MAX_RETRIES: i32 = 5;

/// Check if an entry has exceeded the retry limit.
pub fn is_dead(entry: &OutboxEntry) -> bool {
    entry.retry_count >= MAX_RETRIES
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::compat::BackendMessageRef;
    use std::collections::HashMap;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id TEXT NOT NULL,
                action_type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                status TEXT DEFAULT 'pending',
                retry_count INTEGER DEFAULT 0,
                error_message TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_queue_and_get_pending() {
        let conn = setup_db();
        let payload = serde_json::json!({"by_folder": {"INBOX": [1, 2]}});
        queue_offline_op(&conn, "acc1", "delete", &payload).unwrap();
        queue_offline_op(&conn, "acc1", "set_flags", &serde_json::json!({})).unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert_eq!(pending.len(), 2);
        // flags should come before deletes (replay order)
        assert_eq!(pending[0].action_type, "set_flags");
        assert_eq!(pending[1].action_type, "delete");
    }

    #[test]
    fn test_mark_completed_removes_entry() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "delete", &serde_json::json!({})).unwrap();
        mark_completed(&conn, id).unwrap();
        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn test_mark_failed_increments_retry() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "move", &serde_json::json!({})).unwrap();
        mark_failed(&conn, id, "network error").unwrap();
        mark_failed(&conn, id, "network error").unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert_eq!(pending[0].retry_count, 2);
    }

    #[test]
    fn test_dead_after_max_retries() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "delete", &serde_json::json!({})).unwrap();
        for _ in 0..5 {
            mark_failed(&conn, id, "timeout").unwrap();
        }
        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert!(is_dead(&pending[0]));

        mark_dead(&conn, id).unwrap();
        let dead = get_dead_ops(&conn, "acc1").unwrap();
        assert_eq!(dead.len(), 1);
        // Should no longer be in pending
        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn test_roundtrip_mail_op() {
        let op = MailOp::MoveMessages {
            by_folder: HashMap::from([("INBOX".to_string(), vec![1, 2, 3])]),
            target_folder: "Trash".to_string(),
        };
        let (action_type, payload) = mail_op_to_outbox(&op).unwrap();
        assert_eq!(action_type, "move");

        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", action_type, &payload).unwrap();
        let pending = get_pending_ops(&conn, "acc1").unwrap();
        let restored = outbox_to_mail_op(&pending[0]).unwrap();

        match restored {
            MailOp::MoveMessages {
                by_folder,
                target_folder,
            } => {
                assert_eq!(target_folder, "Trash");
                assert_eq!(by_folder["INBOX"], vec![1, 2, 3]);
            }
            _ => panic!("Expected MoveMessages"),
        }
        mark_completed(&conn, id).unwrap();
    }

    #[test]
    fn set_flags_preserves_legacy_imap_payload() {
        let op = MailOp::SetFlags {
            mutations: vec![FlagMutation {
                target: FlagTarget::messages(vec![
                    BackendMessageRef::imap("INBOX", 1),
                    BackendMessageRef::imap("INBOX", 2),
                ]),
                flags: vec!["seen".into()],
                add: true,
            }],
        };
        let (action_type, payload) = mail_op_to_outbox(&op).unwrap();
        assert_eq!(action_type, "set_flags");
        assert_eq!(
            payload,
            serde_json::json!({
                "by_folder": { "INBOX": [1, 2] },
                "flags": ["seen"],
                "add": true,
            })
        );

        let entry = OutboxEntry {
            id: 1,
            account_id: "acc1".into(),
            action_type: action_type.into(),
            payload_json: payload.to_string(),
            status: "pending".into(),
            retry_count: 0,
            error_message: None,
        };
        match outbox_to_mail_op(&entry).unwrap() {
            MailOp::SetFlags { mutations } => assert_eq!(
                mutations[0].target.message_refs().unwrap(),
                vec![
                    BackendMessageRef::imap("INBOX", 1),
                    BackendMessageRef::imap("INBOX", 2),
                ]
            ),
            _ => panic!("Expected SetFlags"),
        }
    }

    #[test]
    fn provider_flag_references_round_trip_without_delimiter_parsing() {
        for message_ref in [
            BackendMessageRef::jmap("box_with_under", "email_with_under"),
            BackendMessageRef::graph("AAMk_with_under"),
        ] {
            let op = MailOp::SetFlags {
                mutations: vec![FlagMutation {
                    target: FlagTarget::messages(vec![message_ref.clone()]),
                    flags: vec!["flagged".into()],
                    add: false,
                }],
            };
            let (action_type, payload) = mail_op_to_outbox(&op).unwrap();
            assert!(payload.get("message_refs").is_some());
            assert!(payload.get("by_folder").is_none());
            let entry = OutboxEntry {
                id: 1,
                account_id: "acc1".into(),
                action_type: action_type.into(),
                payload_json: payload.to_string(),
                status: "pending".into(),
                retry_count: 0,
                error_message: None,
            };
            match outbox_to_mail_op(&entry).unwrap() {
                MailOp::SetFlags { mutations } => {
                    assert_eq!(mutations[0].target.message_refs().unwrap(), &[message_ref]);
                }
                _ => panic!("Expected SetFlags"),
            }
        }
    }

    #[test]
    fn newer_flags_split_and_supersede_overlapping_pending_intent() {
        let conn = setup_db();
        let old = MailOp::SetFlags {
            mutations: vec![FlagMutation {
                target: FlagTarget::messages(vec![
                    BackendMessageRef::imap("INBOX", 1),
                    BackendMessageRef::imap("INBOX", 2),
                ]),
                flags: vec!["seen".into(), "flagged".into()],
                add: true,
            }],
        };
        let (_, payload) = mail_op_to_outbox(&old).unwrap();
        queue_offline_op(&conn, "acc1", "set_flags", &payload).unwrap();

        let later = MailOp::SetFlags {
            mutations: vec![FlagMutation {
                target: FlagTarget::messages(vec![BackendMessageRef::imap("INBOX", 2)]),
                flags: vec!["flagged".into()],
                add: false,
            }],
        };
        let (_, payload) = mail_op_to_outbox(&later).unwrap();
        queue_offline_op(&conn, "acc1", "set_flags", &payload).unwrap();

        supersede_pending_flag_ops(
            &conn,
            "acc1",
            &[FlagMutation {
                target: FlagTarget::messages(vec![BackendMessageRef::imap("INBOX", 2)]),
                flags: vec!["\\Seen".into()],
                add: false,
            }],
        )
        .unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert_eq!(pending.len(), 2);
        match outbox_to_mail_op(&pending[0]).unwrap() {
            MailOp::SetFlags { mutations } => {
                assert_eq!(mutations.len(), 2);
                assert_eq!(
                    mutations[0].target.message_refs().unwrap(),
                    &[BackendMessageRef::imap("INBOX", 1)]
                );
                assert_eq!(mutations[0].flags, vec!["seen", "flagged"]);
                assert_eq!(
                    mutations[1].target.message_refs().unwrap(),
                    &[BackendMessageRef::imap("INBOX", 2)]
                );
                assert_eq!(mutations[1].flags, vec!["flagged"]);
            }
            _ => panic!("Expected SetFlags"),
        }
        match outbox_to_mail_op(&pending[1]).unwrap() {
            MailOp::SetFlags { mutations } => {
                assert_eq!(
                    mutations[0].target.message_refs().unwrap(),
                    &[BackendMessageRef::imap("INBOX", 2)]
                );
                assert_eq!(mutations[0].flags, vec!["flagged"]);
                assert!(!mutations[0].add);
            }
            _ => panic!("Expected SetFlags"),
        }

        supersede_pending_flag_ops(
            &conn,
            "acc1",
            &[FlagMutation {
                target: FlagTarget::messages(vec![
                    BackendMessageRef::imap("INBOX", 1),
                    BackendMessageRef::imap("INBOX", 2),
                ]),
                flags: vec!["seen".into(), "flagged".into()],
                add: false,
            }],
        )
        .unwrap();
        assert!(get_pending_ops(&conn, "acc1").unwrap().is_empty());
    }

    #[test]
    fn bulk_flag_target_round_trips_with_exclusions() {
        let op = MailOp::SetFlags {
            mutations: vec![FlagMutation {
                target: FlagTarget::AllMessagesInFolders {
                    folder_paths: vec!["INBOX".into(), "Archive".into()],
                    excluded_refs: vec![BackendMessageRef::imap("INBOX", 7)],
                },
                flags: vec!["seen".into()],
                add: true,
            }],
        };
        let (action_type, payload) = mail_op_to_outbox(&op).unwrap();
        let entry = OutboxEntry {
            id: 1,
            account_id: "acc1".into(),
            action_type: action_type.into(),
            payload_json: payload.to_string(),
            status: "pending".into(),
            retry_count: 0,
            error_message: None,
        };

        assert_eq!(outbox_to_mail_op(&entry).unwrap(), op);
    }

    #[test]
    fn newer_unread_adds_an_exclusion_to_pending_bulk_read() {
        let conn = setup_db();
        let bulk = MailOp::SetFlags {
            mutations: vec![FlagMutation {
                target: FlagTarget::AllMessagesInFolders {
                    folder_paths: vec!["INBOX".into(), "Archive".into()],
                    excluded_refs: Vec::new(),
                },
                flags: vec!["seen".into()],
                add: true,
            }],
        };
        let (_, payload) = mail_op_to_outbox(&bulk).unwrap();
        queue_offline_op(&conn, "acc1", "set_flags", &payload).unwrap();

        supersede_pending_flag_ops(
            &conn,
            "acc1",
            &[FlagMutation {
                target: FlagTarget::messages(vec![BackendMessageRef::imap("INBOX", 9)]),
                flags: vec!["seen".into()],
                add: false,
            }],
        )
        .unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        match outbox_to_mail_op(&pending[0]).unwrap() {
            MailOp::SetFlags { mutations } => assert_eq!(
                mutations[0].target,
                FlagTarget::AllMessagesInFolders {
                    folder_paths: vec!["INBOX".into(), "Archive".into()],
                    excluded_refs: vec![BackendMessageRef::imap("INBOX", 9)],
                }
            ),
            _ => panic!("Expected SetFlags"),
        }
    }

    #[test]
    fn newer_bulk_read_supersedes_older_message_intent_in_covered_folders() {
        let conn = setup_db();
        let old = MailOp::SetFlags {
            mutations: vec![FlagMutation {
                target: FlagTarget::messages(vec![
                    BackendMessageRef::imap("INBOX", 1),
                    BackendMessageRef::imap("Other", 2),
                ]),
                flags: vec!["seen".into()],
                add: false,
            }],
        };
        let (_, payload) = mail_op_to_outbox(&old).unwrap();
        queue_offline_op(&conn, "acc1", "set_flags", &payload).unwrap();

        supersede_pending_flag_ops(
            &conn,
            "acc1",
            &[FlagMutation {
                target: FlagTarget::AllMessagesInFolders {
                    folder_paths: vec!["INBOX".into()],
                    excluded_refs: Vec::new(),
                },
                flags: vec!["seen".into()],
                add: true,
            }],
        )
        .unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        match outbox_to_mail_op(&pending[0]).unwrap() {
            MailOp::SetFlags { mutations } => assert_eq!(
                mutations[0].target.message_refs().unwrap(),
                &[BackendMessageRef::imap("Other", 2)]
            ),
            _ => panic!("Expected SetFlags"),
        }
    }

    #[test]
    fn overlapping_newer_bulk_read_keeps_only_uncovered_folders() {
        let conn = setup_db();
        let old = MailOp::SetFlags {
            mutations: vec![FlagMutation {
                target: FlagTarget::AllMessagesInFolders {
                    folder_paths: vec!["INBOX".into(), "Archive".into()],
                    excluded_refs: Vec::new(),
                },
                flags: vec!["seen".into()],
                add: true,
            }],
        };
        let (_, payload) = mail_op_to_outbox(&old).unwrap();
        queue_offline_op(&conn, "acc1", "set_flags", &payload).unwrap();

        supersede_pending_flag_ops(
            &conn,
            "acc1",
            &[FlagMutation {
                target: FlagTarget::AllMessagesInFolders {
                    folder_paths: vec!["INBOX".into()],
                    excluded_refs: Vec::new(),
                },
                flags: vec!["seen".into()],
                add: true,
            }],
        )
        .unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        match outbox_to_mail_op(&pending[0]).unwrap() {
            MailOp::SetFlags { mutations } => assert_eq!(
                mutations[0].target,
                FlagTarget::AllMessagesInFolders {
                    folder_paths: vec!["Archive".into()],
                    excluded_refs: Vec::new(),
                }
            ),
            _ => panic!("Expected SetFlags"),
        }
    }

    #[test]
    fn bulk_target_rejects_invalid_folder_paths() {
        let entry = OutboxEntry {
            id: 1,
            account_id: "acc1".into(),
            action_type: "set_flags".into(),
            payload_json: serde_json::json!({
                "target": {
                    "kind": "all_messages_in_folders",
                    "folder_paths": ["INBOX\nBAD"],
                    "excluded_refs": [],
                },
                "flags": ["seen"],
                "add": true,
            })
            .to_string(),
            status: "pending".into(),
            retry_count: 0,
            error_message: None,
        };

        assert!(outbox_to_mail_op(&entry).is_none());
    }
}
