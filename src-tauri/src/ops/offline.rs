use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::message::BackendMessageRef;
use crate::ops::flags::{remove_deleted_refs, subtract_flag_mutation, FlagMutation, FlagTarget};
use crate::ops::queue::MailOp;

pub const INDETERMINATE_DELIVERY_ERROR_MESSAGE: &str =
    "Delivery outcome is unknown; automatic retry disabled to avoid duplicate delivery. Verify delivery before retrying manually.";

const STUCK_SENDING_ERROR_MESSAGE: &str =
    "Delivery outcome is unknown after app restart; automatic retry disabled to avoid duplicate delivery. Verify delivery before retrying manually.";

const MAX_SEND_ERROR_MESSAGE_BYTES: usize = 256;
const MAX_RETRIES: i32 = 5;

pub fn is_indeterminate_delivery_error_message(message: &str) -> bool {
    message == INDETERMINATE_DELIVERY_ERROR_MESSAGE || message == STUCK_SENDING_ERROR_MESSAGE
}

/// Replay order matching operation dependencies:
/// flags (0) -> copies (1) -> moves (2) -> deletes (3).
/// Copies precede moves because moving an IMAP message invalidates its source UID.
/// `send` is independent of folder state, so it goes last (4).
fn replay_order(action_type: &str) -> i32 {
    match action_type {
        "set_flags" => 0,
        "copy" => 1,
        "move" => 2,
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
    pub retry_count: i32,
}

/// Write a failed operation to the outbox for later replay.
///
/// Replay order (flags -> copies -> moves -> deletes) is computed at read
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

/// Preserve an operation for visibility without scheduling automatic replay.
pub fn queue_dead_op(
    conn: &Connection,
    account_id: &str,
    action_type: &str,
    payload: &serde_json::Value,
    error: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO outbox (account_id, action_type, payload_json, status, retry_count, error_message)
         VALUES (?1, ?2, ?3, 'dead', 0, ?4)",
        rusqlite::params![account_id, action_type, payload.to_string(), error],
    )
    .map_err(Error::Database)?;
    Ok(conn.last_insert_rowid())
}

/// Atomically claim a pending send before replaying it. A false result means
/// the snapshot is stale or another actor already claimed or removed the row.
pub fn claim_pending_send(
    conn: &Connection,
    outbox_id: i64,
    expected_retry_count: i32,
) -> Result<bool> {
    let changed = conn
        .execute(
            "UPDATE outbox SET status = 'sending'
             WHERE id = ?1 AND action_type = 'send' AND status = 'pending'
               AND retry_count = ?2",
            rusqlite::params![outbox_id, expected_retry_count],
        )
        .map_err(Error::Database)?;
    Ok(changed == 1)
}

/// Delete a successfully delivered send only while its claim is still held.
pub fn complete_sending_send(conn: &Connection, outbox_id: i64) -> Result<bool> {
    let changed = conn
        .execute(
            "DELETE FROM outbox
             WHERE id = ?1 AND action_type = 'send' AND status = 'sending'",
            rusqlite::params![outbox_id],
        )
        .map_err(Error::Database)?;
    Ok(changed == 1)
}

/// Complete a delivered send before running any best-effort follow-up work.
pub async fn complete_send_before<T, F, Fut>(
    db: &crate::db::pool::DbPool,
    outbox_id: i64,
    after_completion: F,
) -> Result<bool>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let completed = {
        let conn = db.writer().await;
        complete_sending_send(&conn, outbox_id)?
    };
    if completed {
        let _ = after_completion().await;
    }
    Ok(completed)
}

/// Durable disposition after recording a definite send failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendRetryDisposition {
    Pending,
    Dead,
    MissingClaim,
}

/// Record a definite send failure and atomically release or exhaust its claim.
pub fn retry_sending_send(
    conn: &Connection,
    outbox_id: i64,
    error: &str,
) -> Result<SendRetryDisposition> {
    let error = bounded_send_error(error);
    let status = match conn.query_row(
        "UPDATE outbox
             SET retry_count = retry_count + 1,
                 status = CASE
                     WHEN retry_count + 1 >= ?1 THEN 'dead'
                     ELSE 'pending'
                 END,
                 error_message = ?2
             WHERE id = ?3 AND action_type = 'send' AND status = 'sending'
             RETURNING status",
        rusqlite::params![MAX_RETRIES, error, outbox_id],
        |row| row.get::<_, String>(0),
    ) {
        Ok(status) => status,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(SendRetryDisposition::MissingClaim),
        Err(error) => return Err(Error::Database(error)),
    };
    match status.as_str() {
        "pending" => Ok(SendRetryDisposition::Pending),
        "dead" => Ok(SendRetryDisposition::Dead),
        _ => Err(Error::Other(format!(
            "Unexpected send retry disposition '{status}'"
        ))),
    }
}

/// Quarantine a claimed send after an ambiguous outcome. The explanation is
/// byte-bounded before persistence and the row can only move from sending.
pub fn quarantine_sending_send(conn: &Connection, outbox_id: i64, error: &str) -> Result<bool> {
    quarantine_send_from_status(conn, outbox_id, "sending", error)
}

/// Quarantine an unclaimed send without disturbing a concurrently claimed
/// row. Used for retry exhaustion and invalid persisted payloads.
pub fn quarantine_pending_send(conn: &Connection, outbox_id: i64, error: &str) -> Result<bool> {
    quarantine_send_from_status(conn, outbox_id, "pending", error)
}

/// Mark a legacy pending send at the retry limit dead without replacing its
/// last transport error. The fallback is used only if no error was persisted.
pub fn exhaust_pending_send(
    conn: &Connection,
    outbox_id: i64,
    fallback_error: &str,
) -> Result<bool> {
    let fallback_error = bounded_send_error(fallback_error);
    let changed = conn
        .execute(
            "UPDATE outbox
             SET status = 'dead', error_message = COALESCE(error_message, ?1)
             WHERE id = ?2 AND action_type = 'send' AND status = 'pending'
               AND retry_count >= ?3",
            rusqlite::params![fallback_error, outbox_id, MAX_RETRIES],
        )
        .map_err(Error::Database)?;
    Ok(changed == 1)
}

fn quarantine_send_from_status(
    conn: &Connection,
    outbox_id: i64,
    expected_status: &str,
    error: &str,
) -> Result<bool> {
    let error = bounded_send_error(error);
    let changed = conn
        .execute(
            "UPDATE outbox SET status = 'dead', error_message = ?1
             WHERE id = ?2 AND action_type = 'send' AND status = ?3",
            rusqlite::params![error, outbox_id, expected_status],
        )
        .map_err(Error::Database)?;
    Ok(changed == 1)
}

fn bounded_send_error(error: &str) -> &str {
    let mut end = error.len().min(MAX_SEND_ERROR_MESSAGE_BYTES);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    &error[..end]
}

/// On app startup, quarantine rows left as 'sending' by a previous run.
/// The owning task is gone and delivery may already have occurred, so an
/// automatic retry could create a duplicate. Returns the quarantined count.
pub fn quarantine_stuck_sending(conn: &Connection) -> Result<usize> {
    let count = conn
        .execute(
            "UPDATE outbox SET status = 'dead', error_message = ?1
             WHERE action_type = 'send' AND status = 'sending'",
            rusqlite::params![STUCK_SENDING_ERROR_MESSAGE],
        )
        .map_err(Error::Database)?;
    if count > 0 {
        log::warn!("Quarantined {} outbox row(s) stuck in 'sending'", count);
    }
    Ok(count)
}

/// Get all pending operations for an account, ordered by replay priority.
pub fn get_pending_ops(conn: &Connection, account_id: &str) -> Result<Vec<OutboxEntry>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, account_id, action_type, payload_json, retry_count
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
                retry_count: row.get(4)?,
            })
        })
        .map_err(Error::Database)?
        .filter_map(|r| r.ok())
        .collect();

    // Sort by replay order: flags -> copies -> moves -> deletes.
    // Use stable ordering: break ties by id to preserve insertion order.
    entries.sort_by(|a, b| {
        replay_order(&a.action_type)
            .cmp(&replay_order(&b.action_type))
            .then(a.id.cmp(&b.id))
    });
    Ok(entries)
}

/// Mark a non-send outbox entry as completed (will be deleted).
pub fn mark_completed(conn: &Connection, outbox_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM outbox WHERE id = ?1 AND action_type != 'send'",
        rusqlite::params![outbox_id],
    )
    .map_err(Error::Database)?;
    Ok(())
}

/// Mark a non-send outbox entry as failed, incrementing retry count.
pub fn mark_failed(conn: &Connection, outbox_id: i64, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET retry_count = retry_count + 1, error_message = ?1
         WHERE id = ?2 AND action_type != 'send'",
        rusqlite::params![error, outbox_id],
    )
    .map_err(Error::Database)?;
    Ok(())
}

/// Mark a non-send outbox entry as dead (too many retries).
pub fn mark_dead(conn: &Connection, outbox_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET status = 'dead'
         WHERE id = ?1 AND action_type != 'send'",
        rusqlite::params![outbox_id],
    )
    .map_err(Error::Database)?;
    Ok(())
}

/// Mark a non-send operation dead with a user-visible explanation.
pub fn mark_dead_with_error(conn: &Connection, outbox_id: i64, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET status = 'dead', error_message = ?1
         WHERE id = ?2 AND action_type != 'send'",
        rusqlite::params![error, outbox_id],
    )
    .map_err(Error::Database)?;
    Ok(())
}

/// Convert a MailOp to an action_type string and JSON payload for outbox storage.
pub fn mail_op_to_outbox(op: &MailOp) -> Option<(&'static str, serde_json::Value)> {
    match op {
        MailOp::MoveMessages {
            message_refs,
            target_folder,
        } if message_refs
            .iter()
            .all(|message_ref| matches!(message_ref, BackendMessageRef::Imap { .. })) =>
        {
            let mut by_folder = std::collections::HashMap::<String, Vec<u32>>::new();
            for message_ref in message_refs {
                if let BackendMessageRef::Imap { folder_path, uid } = message_ref {
                    by_folder.entry(folder_path.clone()).or_default().push(*uid);
                }
            }
            Some((
                "move",
                serde_json::json!({
                    "by_folder": by_folder,
                    "target_folder": target_folder,
                }),
            ))
        }
        MailOp::MoveMessages {
            message_refs,
            target_folder,
        } => Some((
            "move",
            serde_json::json!({
                "message_refs": message_refs
                    .iter()
                    .map(message_ref_to_json)
                    .collect::<Vec<_>>(),
                "target_folder": target_folder,
            }),
        )),
        MailOp::DeleteMessages { message_refs }
            if message_refs
                .iter()
                .all(|message_ref| matches!(message_ref, BackendMessageRef::Imap { .. })) =>
        {
            let mut by_folder = std::collections::HashMap::<String, Vec<u32>>::new();
            for message_ref in message_refs {
                if let BackendMessageRef::Imap { folder_path, uid } = message_ref {
                    by_folder.entry(folder_path.clone()).or_default().push(*uid);
                }
            }
            Some(("delete", serde_json::json!({ "by_folder": by_folder })))
        }
        MailOp::DeleteMessages { message_refs } => Some((
            "delete",
            serde_json::json!({
                "message_refs": message_refs
                    .iter()
                    .map(message_ref_to_json)
                    .collect::<Vec<_>>()
            }),
        )),
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
            message_refs,
            target_folder,
        } if message_refs
            .iter()
            .all(|message_ref| matches!(message_ref, BackendMessageRef::Imap { .. })) =>
        {
            let mut by_folder = std::collections::HashMap::<String, Vec<u32>>::new();
            for message_ref in message_refs {
                if let BackendMessageRef::Imap { folder_path, uid } = message_ref {
                    by_folder.entry(folder_path.clone()).or_default().push(*uid);
                }
            }
            Some((
                "copy",
                serde_json::json!({
                    "by_folder": by_folder,
                    "target_folder": target_folder,
                }),
            ))
        }
        MailOp::CopyMessages {
            message_refs,
            target_folder,
        } => Some((
            "copy",
            serde_json::json!({
                "message_refs": message_refs
                    .iter()
                    .map(message_ref_to_json)
                    .collect::<Vec<_>>(),
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

/// Remove explicit pending flag mutations made obsolete by a newer delete.
pub fn supersede_pending_flags_for_delete(
    conn: &Connection,
    account_id: &str,
    deleted_refs: &[BackendMessageRef],
) -> Result<()> {
    let transaction = conn.unchecked_transaction().map_err(Error::Database)?;
    rewrite_pending_flags_for_delete(&transaction, account_id, deleted_refs)?;
    transaction.commit().map_err(Error::Database)
}

/// Atomically preserve an unqueued delete and prune stale flag intent.
pub fn queue_failed_delete(
    conn: &Connection,
    account_id: &str,
    message_refs: &[BackendMessageRef],
) -> Result<i64> {
    let (_, payload) = mail_op_to_outbox(&MailOp::DeleteMessages {
        message_refs: message_refs.to_vec(),
    })
    .expect("DeleteMessages is serializable");
    let transaction = conn.unchecked_transaction().map_err(Error::Database)?;
    let outbox_id = queue_offline_op(&transaction, account_id, "delete", &payload)?;
    rewrite_pending_flags_for_delete(&transaction, account_id, message_refs)?;
    transaction.commit().map_err(Error::Database)?;
    Ok(outbox_id)
}

/// Preserve a move that could not be sent to the account worker.
pub fn queue_failed_move(
    conn: &Connection,
    account_id: &str,
    message_refs: &[BackendMessageRef],
    target_folder: &str,
) -> Result<i64> {
    let (_, payload) = mail_op_to_outbox(&MailOp::MoveMessages {
        message_refs: message_refs.to_vec(),
        target_folder: target_folder.to_string(),
    })
    .expect("MoveMessages is serializable");
    queue_offline_op(conn, account_id, "move", &payload)
}

/// Preserve a copy that could not be sent to the account worker.
pub fn queue_failed_copy(
    conn: &Connection,
    account_id: &str,
    message_refs: &[BackendMessageRef],
    target_folder: &str,
) -> Result<i64> {
    let (_, payload) = mail_op_to_outbox(&MailOp::CopyMessages {
        message_refs: message_refs.to_vec(),
        target_folder: target_folder.to_string(),
    })
    .expect("CopyMessages is serializable");
    queue_offline_op(conn, account_id, "copy", &payload)
}

fn rewrite_pending_flags_for_delete(
    conn: &Connection,
    account_id: &str,
    deleted_refs: &[BackendMessageRef],
) -> Result<()> {
    let pending = get_pending_ops(conn, account_id)?;
    for entry in pending
        .into_iter()
        .filter(|entry| entry.action_type == "set_flags")
    {
        let Some(MailOp::SetFlags { mutations }) = outbox_to_mail_op(&entry) else {
            continue;
        };
        let remaining: Vec<_> = mutations
            .into_iter()
            .filter_map(|mutation| remove_deleted_refs(mutation, deleted_refs))
            .collect();
        if remaining.is_empty() {
            conn.execute(
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
        conn.execute(
            "UPDATE outbox SET payload_json = ?1 WHERE id = ?2",
            rusqlite::params![payload, entry.id],
        )
        .map_err(Error::Database)?;
    }
    Ok(())
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
                matches!(message_ref, crate::message::BackendMessageRef::Imap { .. })
            }) =>
        {
            let mut by_folder = std::collections::HashMap::<String, Vec<u32>>::new();
            for message_ref in message_refs {
                if let crate::message::BackendMessageRef::Imap { folder_path, uid } = message_ref {
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
                crate::message::BackendMessageRef::Imap { folder_path, .. }
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
                        crate::message::BackendMessageRef::imap(folder_path.clone(), uid)
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

fn message_ref_to_json(message_ref: &crate::message::BackendMessageRef) -> serde_json::Value {
    use crate::message::BackendMessageRef;

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

fn message_ref_from_json(value: &serde_json::Value) -> Option<crate::message::BackendMessageRef> {
    use crate::message::BackendMessageRef;

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

fn valid_message_ref(message_ref: &BackendMessageRef) -> bool {
    match message_ref {
        BackendMessageRef::Imap { folder_path, uid } => valid_folder_path(folder_path) && *uid > 0,
        BackendMessageRef::Jmap {
            mailbox_id,
            email_id,
        } => !mailbox_id.is_empty() && !email_id.is_empty(),
        BackendMessageRef::Graph { item_id } => !item_id.is_empty(),
    }
}

fn message_refs_have_one_provider(message_refs: &[BackendMessageRef]) -> bool {
    let Some(first) = message_refs.first() else {
        return false;
    };
    let provider = std::mem::discriminant(first);
    message_refs
        .iter()
        .all(|message_ref| std::mem::discriminant(message_ref) == provider)
}

fn legacy_delete_refs(
    entry: &OutboxEntry,
    payload: &serde_json::Value,
) -> Option<Vec<BackendMessageRef>> {
    let protocol = payload.get("protocol")?.as_str()?;
    let message_ids = payload.get("message_ids")?.as_array()?;
    match protocol {
        "graph" => message_ids
            .iter()
            .map(|id| {
                Some(BackendMessageRef::graph_from_db_id(
                    &entry.account_id,
                    id.as_str()?,
                ))
            })
            .collect(),
        "jmap" => {
            log::error!("Rejecting legacy JMAP delete payload with ambiguous message identity");
            None
        }
        _ => None,
    }
}

fn legacy_move_refs(
    entry: &OutboxEntry,
    payload: &serde_json::Value,
) -> Option<Vec<BackendMessageRef>> {
    let protocol = payload.get("protocol")?.as_str()?;
    let message_ids = payload.get("message_ids")?.as_array()?;
    match protocol {
        "graph" => message_ids
            .iter()
            .map(|id| {
                Some(BackendMessageRef::graph_from_db_id(
                    &entry.account_id,
                    id.as_str()?,
                ))
            })
            .collect(),
        "jmap" => {
            log::error!("Rejecting legacy JMAP move payload with ambiguous message identity");
            None
        }
        _ => None,
    }
}

/// Convert an outbox entry back to a MailOp for replay.
pub fn outbox_to_mail_op(entry: &OutboxEntry) -> Option<MailOp> {
    let payload: serde_json::Value = serde_json::from_str(&entry.payload_json).ok()?;
    match entry.action_type.as_str() {
        "move" => {
            let message_refs = if let Some(by_folder) = payload.get("by_folder") {
                let by_folder: std::collections::HashMap<String, Vec<u32>> =
                    serde_json::from_value(by_folder.clone()).ok()?;
                if !validate_folder_paths(&by_folder) {
                    log::warn!("outbox_to_mail_op: rejected move op with invalid folder path");
                    return None;
                }
                by_folder
                    .into_iter()
                    .flat_map(|(folder_path, uids)| {
                        uids.into_iter()
                            .map(move |uid| BackendMessageRef::imap(folder_path.clone(), uid))
                    })
                    .collect()
            } else if let Some(message_refs) = payload.get("message_refs") {
                message_refs
                    .as_array()?
                    .iter()
                    .map(message_ref_from_json)
                    .collect::<Option<Vec<_>>>()?
            } else {
                legacy_move_refs(entry, &payload)?
            };
            if message_refs.is_empty() || !message_refs.iter().all(valid_message_ref) {
                log::warn!("outbox_to_mail_op: rejected move op with invalid message reference");
                return None;
            }
            let target_folder = payload.get("target_folder")?.as_str()?.to_string();
            if target_folder.is_empty() || !valid_folder_path(&target_folder) {
                log::warn!("outbox_to_mail_op: rejected move op with invalid target folder");
                return None;
            }
            Some(MailOp::MoveMessages {
                message_refs,
                target_folder,
            })
        }
        "delete" => {
            let message_refs = if let Some(by_folder) = payload.get("by_folder") {
                let by_folder: std::collections::HashMap<String, Vec<u32>> =
                    serde_json::from_value(by_folder.clone()).ok()?;
                by_folder
                    .into_iter()
                    .flat_map(|(folder_path, uids)| {
                        uids.into_iter()
                            .map(move |uid| BackendMessageRef::imap(folder_path.clone(), uid))
                    })
                    .collect()
            } else if let Some(message_refs) = payload.get("message_refs") {
                message_refs
                    .as_array()?
                    .iter()
                    .map(message_ref_from_json)
                    .collect::<Option<Vec<_>>>()?
            } else {
                legacy_delete_refs(entry, &payload)?
            };
            if message_refs.is_empty() || !message_refs.iter().all(valid_message_ref) {
                log::warn!("outbox_to_mail_op: rejected delete op with invalid message reference");
                return None;
            }
            Some(MailOp::DeleteMessages { message_refs })
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
            let message_refs = if let Some(by_folder) = payload.get("by_folder") {
                let by_folder: std::collections::HashMap<String, Vec<u32>> =
                    serde_json::from_value(by_folder.clone()).ok()?;
                if !validate_folder_paths(&by_folder) {
                    log::warn!("outbox_to_mail_op: rejected copy op with invalid folder path");
                    return None;
                }
                by_folder
                    .into_iter()
                    .flat_map(|(folder_path, uids)| {
                        uids.into_iter()
                            .map(move |uid| BackendMessageRef::imap(folder_path.clone(), uid))
                    })
                    .collect()
            } else {
                payload
                    .get("message_refs")?
                    .as_array()?
                    .iter()
                    .map(message_ref_from_json)
                    .collect::<Option<Vec<_>>>()?
            };
            if !message_refs_have_one_provider(&message_refs)
                || !message_refs.iter().all(valid_message_ref)
            {
                log::warn!("outbox_to_mail_op: rejected copy op with invalid message reference");
                return None;
            }
            let target_folder = payload.get("target_folder")?.as_str()?.to_string();
            if target_folder.is_empty() || !valid_folder_path(&target_folder) {
                log::warn!("outbox_to_mail_op: rejected copy op with invalid target folder");
                return None;
            }
            Some(MailOp::CopyMessages {
                message_refs,
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

/// Check if an entry has exceeded the retry limit.
pub fn is_dead(entry: &OutboxEntry) -> bool {
    entry.retry_count >= MAX_RETRIES
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::BackendMessageRef;
    use std::sync::atomic::{AtomicBool, Ordering};
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

    fn row_state(conn: &Connection, id: i64) -> Option<(String, i32, Option<String>)> {
        conn.query_row(
            "SELECT status, retry_count, error_message FROM outbox WHERE id = ?1",
            rusqlite::params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok()
    }

    #[test]
    fn provider_message_reference_json_is_stable() {
        let cases = [
            (
                BackendMessageRef::imap("INBOX", 7),
                serde_json::json!({
                    "kind": "imap",
                    "folder_path": "INBOX",
                    "uid": 7,
                }),
            ),
            (
                BackendMessageRef::jmap("mailbox", "email"),
                serde_json::json!({
                    "kind": "jmap",
                    "mailbox_id": "mailbox",
                    "email_id": "email",
                }),
            ),
            (
                BackendMessageRef::graph("item"),
                serde_json::json!({
                    "kind": "graph",
                    "item_id": "item",
                }),
            ),
        ];

        for (message_ref, expected) in cases {
            assert_eq!(message_ref_to_json(&message_ref), expected);
            assert_eq!(message_ref_from_json(&expected), Some(message_ref));
        }
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
    fn copy_replays_before_move() {
        let conn = setup_db();
        queue_offline_op(&conn, "acc1", "move", &serde_json::json!({})).unwrap();
        queue_offline_op(&conn, "acc1", "copy", &serde_json::json!({})).unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert_eq!(pending[0].action_type, "copy");
        assert_eq!(pending[1].action_type, "move");
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
    fn invalid_payload_is_marked_dead_with_an_error() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "delete", &serde_json::json!({})).unwrap();
        mark_dead_with_error(&conn, id, "invalid delete payload").unwrap();

        assert_eq!(
            row_state(&conn, id),
            Some(("dead".into(), 0, Some("invalid delete payload".into())))
        );
    }

    #[test]
    fn queue_dead_op_is_visible_but_not_pending() {
        let conn = setup_db();
        let id = queue_dead_op(
            &conn,
            "acc1",
            "copy",
            &serde_json::json!({ "message_refs": [] }),
            "ambiguous copy outcome",
        )
        .unwrap();

        assert!(get_pending_ops(&conn, "acc1").unwrap().is_empty());
        assert_eq!(
            row_state(&conn, id),
            Some(("dead".into(), 0, Some("ambiguous copy outcome".into())))
        );
    }

    #[test]
    fn send_claim_rejects_stale_and_non_send_rows() {
        let conn = setup_db();
        let send_id = queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();
        let dead_id =
            queue_offline_op_with_status(&conn, "acc1", "send", &serde_json::json!({}), "dead")
                .unwrap();
        let non_send_id = queue_offline_op(&conn, "acc1", "move", &serde_json::json!({})).unwrap();
        let discarded_id = queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();
        let changed_retry_id =
            queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();
        mark_failed(&conn, changed_retry_id, "legacy helper cannot change send").unwrap();
        conn.execute(
            "UPDATE outbox SET retry_count = 1 WHERE id = ?1",
            rusqlite::params![changed_retry_id],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM outbox WHERE id = ?1 AND status = 'pending'",
            rusqlite::params![discarded_id],
        )
        .unwrap();

        assert!(claim_pending_send(&conn, send_id, 0).unwrap());
        assert_eq!(row_state(&conn, send_id).unwrap().0, "sending");
        assert!(!claim_pending_send(&conn, send_id, 0).unwrap());
        assert!(!claim_pending_send(&conn, dead_id, 0).unwrap());
        assert!(!claim_pending_send(&conn, non_send_id, 0).unwrap());
        assert!(!claim_pending_send(&conn, discarded_id, 0).unwrap());
        assert!(!claim_pending_send(&conn, changed_retry_id, 0).unwrap());
        assert!(claim_pending_send(&conn, changed_retry_id, 1).unwrap());
    }

    #[test]
    fn legacy_non_send_transitions_cannot_mutate_send_rows() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();

        mark_completed(&conn, id).unwrap();
        mark_failed(&conn, id, "failure").unwrap();
        mark_dead(&conn, id).unwrap();
        mark_dead_with_error(&conn, id, "dead").unwrap();

        assert_eq!(row_state(&conn, id), Some(("pending".into(), 0, None)));
    }

    #[test]
    fn pending_send_quarantine_does_not_override_a_claim() {
        let conn = setup_db();
        let pending = queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();
        let claimed = queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();
        assert!(claim_pending_send(&conn, claimed, 0).unwrap());

        assert!(quarantine_pending_send(&conn, pending, "invalid payload").unwrap());
        assert!(!quarantine_pending_send(&conn, claimed, "stale snapshot").unwrap());
        assert_eq!(row_state(&conn, pending).unwrap().0, "dead");
        assert_eq!(row_state(&conn, claimed).unwrap().0, "sending");
    }

    #[test]
    fn send_completion_deletes_only_a_claimed_send() {
        let conn = setup_db();
        let send_id = queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();
        let non_send_id =
            queue_offline_op_with_status(&conn, "acc1", "move", &serde_json::json!({}), "sending")
                .unwrap();

        assert!(!complete_sending_send(&conn, send_id).unwrap());
        assert!(claim_pending_send(&conn, send_id, 0).unwrap());
        assert!(complete_sending_send(&conn, send_id).unwrap());
        assert!(row_state(&conn, send_id).is_none());
        assert!(!complete_sending_send(&conn, send_id).unwrap());
        assert!(!complete_sending_send(&conn, non_send_id).unwrap());
        assert_eq!(row_state(&conn, non_send_id).unwrap().0, "sending");
    }

    #[tokio::test]
    async fn completion_precedes_and_survives_failed_postprocess() {
        let temp = tempfile::tempdir().unwrap();
        let pool = crate::db::pool::DbPool::new(&temp.path().join("outbox.db"), 1).unwrap();
        {
            let conn = pool.writer().await;
            conn.execute_batch(
                "CREATE TABLE outbox (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    account_id TEXT NOT NULL,
                    action_type TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    status TEXT NOT NULL,
                    retry_count INTEGER NOT NULL,
                    error_message TEXT
                );",
            )
            .unwrap();
        }
        let id = {
            let conn = pool.writer().await;
            queue_offline_op_with_status(&conn, "acc1", "send", &serde_json::json!({}), "sending")
                .unwrap()
        };
        let postprocess_ran = AtomicBool::new(false);

        let completed = complete_send_before(&pool, id, || async {
            let conn = pool.reader();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM outbox WHERE id = ?1",
                    rusqlite::params![id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "delivery must complete before postprocess");
            postprocess_ran.store(true, Ordering::SeqCst);
            Err::<(), &str>("injected Sent append failure")
        })
        .await
        .unwrap();

        assert!(completed);
        assert!(postprocess_ran.load(Ordering::SeqCst));
        let conn = pool.reader();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM outbox WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn definite_send_failure_atomically_releases_claim_and_increments_retry() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();

        assert_eq!(
            retry_sending_send(&conn, id, "definite rejection").unwrap(),
            SendRetryDisposition::MissingClaim
        );
        assert!(claim_pending_send(&conn, id, 0).unwrap());
        assert_eq!(
            retry_sending_send(&conn, id, "definite rejection").unwrap(),
            SendRetryDisposition::Pending
        );
        assert_eq!(
            row_state(&conn, id),
            Some(("pending".into(), 1, Some("definite rejection".into())))
        );
        assert_eq!(
            retry_sending_send(&conn, id, "second failure").unwrap(),
            SendRetryDisposition::MissingClaim
        );
        assert_eq!(row_state(&conn, id).unwrap().1, 1);
    }

    #[test]
    fn definite_send_failure_at_limit_is_dead_with_last_bounded_error() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();
        conn.execute(
            "UPDATE outbox SET retry_count = ?1 WHERE id = ?2",
            rusqlite::params![MAX_RETRIES - 1, id],
        )
        .unwrap();
        assert!(claim_pending_send(&conn, id, MAX_RETRIES - 1).unwrap());

        let error = format!("last transport error: {}", "é".repeat(512));
        assert_eq!(
            retry_sending_send(&conn, id, &error).unwrap(),
            SendRetryDisposition::Dead
        );
        let state = row_state(&conn, id).unwrap();
        assert_eq!(state.0, "dead");
        assert_eq!(state.1, MAX_RETRIES);
        let stored = state.2.unwrap();
        assert!(stored.starts_with("last transport error:"));
        assert!(stored.len() <= MAX_SEND_ERROR_MESSAGE_BYTES);
    }

    #[test]
    fn legacy_pending_send_at_limit_preserves_existing_error() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();
        conn.execute(
            "UPDATE outbox SET retry_count = ?1, error_message = 'last SMTP rejection'
             WHERE id = ?2",
            rusqlite::params![MAX_RETRIES, id],
        )
        .unwrap();

        assert!(exhaust_pending_send(&conn, id, "retry limit reached").unwrap());
        assert_eq!(
            row_state(&conn, id),
            Some((
                "dead".into(),
                MAX_RETRIES,
                Some("last SMTP rejection".into())
            ))
        );
    }

    #[test]
    fn indeterminate_send_atomically_quarantines_with_bounded_error() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();
        assert!(claim_pending_send(&conn, id, 0).unwrap());

        let long_error = format!("Delivery outcome is unknown: {}", "é".repeat(512));
        assert!(quarantine_sending_send(&conn, id, &long_error).unwrap());
        let state = row_state(&conn, id).unwrap();
        assert_eq!(state.0, "dead");
        let stored = state.2.unwrap();
        assert!(stored.starts_with("Delivery outcome is unknown"));
        assert!(stored.len() <= MAX_SEND_ERROR_MESSAGE_BYTES);
        assert!(!quarantine_sending_send(&conn, id, "again").unwrap());
    }

    #[test]
    fn failed_send_transition_leaves_the_claim_held() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();
        assert!(claim_pending_send(&conn, id, 0).unwrap());
        conn.execute_batch(
            "CREATE TRIGGER reject_send_transition
             BEFORE UPDATE OF status ON outbox
             WHEN OLD.status = 'sending'
             BEGIN
                 SELECT RAISE(ABORT, 'injected transition failure');
             END;",
        )
        .unwrap();

        assert!(retry_sending_send(&conn, id, "definite rejection").is_err());
        assert!(quarantine_sending_send(&conn, id, "unknown").is_err());
        assert_eq!(row_state(&conn, id), Some(("sending".into(), 0, None)));

        conn.execute_batch(
            "DROP TRIGGER reject_send_transition;
             CREATE TRIGGER reject_send_completion
             BEFORE DELETE ON outbox
             WHEN OLD.status = 'sending'
             BEGIN
                 SELECT RAISE(ABORT, 'injected completion failure');
             END;",
        )
        .unwrap();
        assert!(complete_sending_send(&conn, id).is_err());
        assert_eq!(row_state(&conn, id), Some(("sending".into(), 0, None)));
    }

    #[test]
    fn stuck_sending_rows_are_quarantined_as_dead() {
        let conn = setup_db();
        let sending_id =
            queue_offline_op_with_status(&conn, "acc1", "send", &serde_json::json!({}), "sending")
                .unwrap();
        let pending_id = queue_offline_op(&conn, "acc1", "send", &serde_json::json!({})).unwrap();

        assert_eq!(quarantine_stuck_sending(&conn).unwrap(), 1);
        assert_eq!(quarantine_stuck_sending(&conn).unwrap(), 0);

        assert_eq!(
            row_state(&conn, sending_id),
            Some(("dead".into(), 0, Some(STUCK_SENDING_ERROR_MESSAGE.into())))
        );
        assert!(STUCK_SENDING_ERROR_MESSAGE.len() <= 256);
        assert!(INDETERMINATE_DELIVERY_ERROR_MESSAGE.len() <= 256);

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, pending_id);
    }

    #[test]
    fn test_mark_failed_increments_retry() {
        let conn = setup_db();
        let id = queue_offline_op(&conn, "acc1", "move", &serde_json::json!({})).unwrap();
        mark_failed(&conn, id, "network error").unwrap();
        mark_failed(&conn, id, "network error").unwrap();

        assert_eq!(
            row_state(&conn, id),
            Some(("pending".into(), 2, Some("network error".into())))
        );
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
        assert_eq!(
            row_state(&conn, id),
            Some(("dead".into(), 5, Some("timeout".into())))
        );
        // Should no longer be in pending
        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn test_roundtrip_mail_op() {
        let op = MailOp::MoveMessages {
            message_refs: vec![
                BackendMessageRef::imap("INBOX", 1),
                BackendMessageRef::imap("INBOX", 2),
                BackendMessageRef::imap("INBOX", 3),
            ],
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
                message_refs,
                target_folder,
            } => {
                assert_eq!(target_folder, "Trash");
                assert_eq!(message_refs.len(), 3);
                assert!(message_refs.contains(&BackendMessageRef::imap("INBOX", 1)));
                assert!(message_refs.contains(&BackendMessageRef::imap("INBOX", 2)));
                assert!(message_refs.contains(&BackendMessageRef::imap("INBOX", 3)));
            }
            _ => panic!("Expected MoveMessages"),
        }
        mark_completed(&conn, id).unwrap();
    }

    #[test]
    fn provider_move_references_round_trip_without_delimiter_parsing() {
        for message_ref in [
            BackendMessageRef::jmap("box_with_under", "email_with_under"),
            BackendMessageRef::graph("AAMk_with_under"),
        ] {
            let op = MailOp::MoveMessages {
                message_refs: vec![message_ref],
                target_folder: "target_with_under".into(),
            };
            let (action_type, payload) = mail_op_to_outbox(&op).unwrap();
            assert!(payload.get("message_refs").is_some());
            assert_eq!(
                outbox_to_mail_op(&outbox_entry(action_type, payload)),
                Some(op)
            );
        }
    }

    #[test]
    fn legacy_graph_move_is_recovered_but_ambiguous_jmap_is_rejected() {
        let graph = outbox_entry(
            "move",
            serde_json::json!({
                "protocol": "graph",
                "message_ids": ["acc1_AAMk_with_under"],
                "target_folder": "archive"
            }),
        );
        assert_eq!(
            outbox_to_mail_op(&graph),
            Some(MailOp::MoveMessages {
                message_refs: vec![BackendMessageRef::graph("AAMk_with_under")],
                target_folder: "archive".into()
            })
        );

        let jmap = outbox_entry(
            "move",
            serde_json::json!({
                "protocol": "jmap",
                "message_ids": ["acc1_mailbox_email_with_under"],
                "target_folder": "archive"
            }),
        );
        assert!(outbox_to_mail_op(&jmap).is_none());
    }

    #[test]
    fn move_rejects_invalid_references_and_targets() {
        for payload in [
            serde_json::json!({
                "message_refs": [{ "kind": "graph", "item_id": "" }],
                "target_folder": "archive"
            }),
            serde_json::json!({
                "message_refs": [{ "kind": "graph", "item_id": "id" }],
                "target_folder": ""
            }),
            serde_json::json!({
                "by_folder": { "INBOX": [1] },
                "target_folder": "bad\rpath"
            }),
        ] {
            assert!(outbox_to_mail_op(&outbox_entry("move", payload)).is_none());
        }
    }

    #[test]
    fn imap_copy_preserves_legacy_payload() {
        let op = MailOp::CopyMessages {
            message_refs: vec![
                BackendMessageRef::imap("INBOX", 1),
                BackendMessageRef::imap("INBOX", 2),
            ],
            target_folder: "Archive".into(),
        };
        let (action_type, payload) = mail_op_to_outbox(&op).unwrap();
        assert_eq!(action_type, "copy");
        assert_eq!(
            payload,
            serde_json::json!({
                "by_folder": { "INBOX": [1, 2] },
                "target_folder": "Archive"
            })
        );
        assert_eq!(
            outbox_to_mail_op(&outbox_entry(action_type, payload)),
            Some(op)
        );
    }

    #[test]
    fn provider_copy_references_round_trip_without_delimiter_parsing() {
        for message_ref in [
            BackendMessageRef::jmap("box_with_under", "email_with_under"),
            BackendMessageRef::graph("AAMk_with_under"),
        ] {
            let op = MailOp::CopyMessages {
                message_refs: vec![message_ref],
                target_folder: "target_with_under".into(),
            };
            let (action_type, payload) = mail_op_to_outbox(&op).unwrap();
            assert!(payload.get("message_refs").is_some());
            assert_eq!(
                outbox_to_mail_op(&outbox_entry(action_type, payload)),
                Some(op)
            );
        }
    }

    #[test]
    fn copy_rejects_invalid_references_and_targets() {
        for payload in [
            serde_json::json!({
                "message_refs": [],
                "target_folder": "archive"
            }),
            serde_json::json!({
                "by_folder": { "INBOX": [0] },
                "target_folder": "archive"
            }),
            serde_json::json!({
                "message_refs": [{ "kind": "graph", "item_id": "" }],
                "target_folder": "archive"
            }),
            serde_json::json!({
                "message_refs": [{ "kind": "jmap", "mailbox_id": "box", "email_id": "" }],
                "target_folder": "archive"
            }),
            serde_json::json!({
                "message_refs": [
                    { "kind": "imap", "folder_path": "INBOX", "uid": 1 },
                    { "kind": "graph", "item_id": "item" }
                ],
                "target_folder": "archive"
            }),
            serde_json::json!({
                "by_folder": { "INBOX": [1] },
                "target_folder": "bad\npath"
            }),
        ] {
            assert!(outbox_to_mail_op(&outbox_entry("copy", payload)).is_none());
        }
    }

    #[test]
    fn queue_failed_copy_persists_provider_references() {
        let conn = setup_db();
        let refs = vec![BackendMessageRef::graph("item_with_under")];
        queue_failed_copy(&conn, "acc1", &refs, "archive").unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert_eq!(
            outbox_to_mail_op(&pending[0]),
            Some(MailOp::CopyMessages {
                message_refs: refs,
                target_folder: "archive".into(),
            })
        );
    }

    fn outbox_entry(action_type: &str, payload: serde_json::Value) -> OutboxEntry {
        OutboxEntry {
            id: 1,
            account_id: "acc1".into(),
            action_type: action_type.into(),
            payload_json: payload.to_string(),
            retry_count: 0,
        }
    }

    #[test]
    fn imap_delete_preserves_legacy_payload() {
        let op = MailOp::DeleteMessages {
            message_refs: vec![
                BackendMessageRef::imap("INBOX", 1),
                BackendMessageRef::imap("INBOX", 2),
            ],
        };
        let (action_type, payload) = mail_op_to_outbox(&op).unwrap();
        assert_eq!(action_type, "delete");
        assert_eq!(
            payload,
            serde_json::json!({ "by_folder": { "INBOX": [1, 2] } })
        );
        assert_eq!(
            outbox_to_mail_op(&outbox_entry(action_type, payload)),
            Some(op)
        );
    }

    #[test]
    fn provider_delete_references_round_trip_without_delimiter_parsing() {
        for message_ref in [
            BackendMessageRef::jmap("box_with_under", "email_with_under"),
            BackendMessageRef::graph("AAMk_with_under"),
        ] {
            let op = MailOp::DeleteMessages {
                message_refs: vec![message_ref],
            };
            let (action_type, payload) = mail_op_to_outbox(&op).unwrap();
            assert!(payload.get("message_refs").is_some());
            assert_eq!(
                outbox_to_mail_op(&outbox_entry(action_type, payload)),
                Some(op)
            );
        }
    }

    #[test]
    fn legacy_graph_delete_is_recovered_but_ambiguous_jmap_is_rejected() {
        let graph = outbox_entry(
            "delete",
            serde_json::json!({
                "protocol": "graph",
                "message_ids": ["acc1_AAMk_with_under"]
            }),
        );
        assert_eq!(
            outbox_to_mail_op(&graph),
            Some(MailOp::DeleteMessages {
                message_refs: vec![BackendMessageRef::graph("AAMk_with_under")]
            })
        );

        let jmap = outbox_entry(
            "delete",
            serde_json::json!({
                "protocol": "jmap",
                "message_ids": ["acc1_mailbox_email_with_under"]
            }),
        );
        assert!(outbox_to_mail_op(&jmap).is_none());

        let mut underscored_account = outbox_entry(
            "delete",
            serde_json::json!({
                "protocol": "jmap",
                "message_ids": ["acc_1_mailbox_email"]
            }),
        );
        underscored_account.account_id = "acc_1".into();
        assert!(outbox_to_mail_op(&underscored_account).is_none());
    }

    #[test]
    fn delete_rejects_invalid_provider_references() {
        for payload in [
            serde_json::json!({ "by_folder": { "INBOX\nBAD": [1] } }),
            serde_json::json!({
                "message_refs": [{ "kind": "graph", "item_id": "" }]
            }),
            serde_json::json!({
                "message_refs": [{
                    "kind": "jmap",
                    "mailbox_id": "box",
                    "email_id": ""
                }]
            }),
        ] {
            assert!(outbox_to_mail_op(&outbox_entry("delete", payload)).is_none());
        }
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
            retry_count: 0,
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
                retry_count: 0,
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
    fn delete_prunes_matching_pending_explicit_flags() {
        let conn = setup_db();
        let flags = MailOp::SetFlags {
            mutations: vec![FlagMutation {
                target: FlagTarget::messages(vec![
                    BackendMessageRef::jmap("inbox", "email_1"),
                    BackendMessageRef::jmap("inbox", "email_2"),
                ]),
                flags: vec!["seen".into()],
                add: true,
            }],
        };
        let (_, payload) = mail_op_to_outbox(&flags).unwrap();
        queue_offline_op(&conn, "acc1", "set_flags", &payload).unwrap();

        supersede_pending_flags_for_delete(
            &conn,
            "acc1",
            &[BackendMessageRef::jmap("archive", "email_1")],
        )
        .unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        match outbox_to_mail_op(&pending[0]).unwrap() {
            MailOp::SetFlags { mutations } => assert_eq!(
                mutations[0].target.message_refs().unwrap(),
                &[BackendMessageRef::jmap("inbox", "email_2")]
            ),
            _ => panic!("Expected SetFlags"),
        }

        supersede_pending_flags_for_delete(
            &conn,
            "acc1",
            &[BackendMessageRef::jmap("other", "email_2")],
        )
        .unwrap();
        assert!(get_pending_ops(&conn, "acc1").unwrap().is_empty());
    }

    #[test]
    fn failed_enqueue_persists_delete_and_prunes_flags_atomically() {
        let conn = setup_db();
        let flags = MailOp::SetFlags {
            mutations: vec![FlagMutation {
                target: FlagTarget::messages(vec![
                    BackendMessageRef::graph("deleted"),
                    BackendMessageRef::graph("retained"),
                ]),
                flags: vec!["seen".into()],
                add: true,
            }],
        };
        let (_, payload) = mail_op_to_outbox(&flags).unwrap();
        queue_offline_op(&conn, "acc1", "set_flags", &payload).unwrap();

        let deleted = BackendMessageRef::graph("deleted");
        queue_failed_delete(&conn, "acc1", std::slice::from_ref(&deleted)).unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert_eq!(pending.len(), 2);
        match outbox_to_mail_op(&pending[0]).unwrap() {
            MailOp::SetFlags { mutations } => assert_eq!(
                mutations[0].target.message_refs().unwrap(),
                &[BackendMessageRef::graph("retained")]
            ),
            _ => panic!("Expected SetFlags"),
        }
        assert_eq!(
            outbox_to_mail_op(&pending[1]),
            Some(MailOp::DeleteMessages {
                message_refs: vec![deleted]
            })
        );
    }

    #[test]
    fn failed_enqueue_persists_typed_move() {
        let conn = setup_db();
        let message_refs = vec![BackendMessageRef::jmap(
            "mailbox_with_under",
            "email_with_under",
        )];
        queue_failed_move(&conn, "acc1", &message_refs, "archive").unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            outbox_to_mail_op(&pending[0]),
            Some(MailOp::MoveMessages {
                message_refs,
                target_folder: "archive".into(),
            })
        );
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
            retry_count: 0,
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
    fn newest_read_removes_an_older_bulk_exclusion() {
        let conn = setup_db();
        let bulk = MailOp::SetFlags {
            mutations: vec![FlagMutation {
                target: FlagTarget::AllMessagesInFolders {
                    folder_paths: vec!["INBOX".into()],
                    excluded_refs: Vec::new(),
                },
                flags: vec!["seen".into()],
                add: true,
            }],
        };
        let (_, payload) = mail_op_to_outbox(&bulk).unwrap();
        queue_offline_op(&conn, "acc1", "set_flags", &payload).unwrap();

        let message_ref = BackendMessageRef::imap("INBOX", 9);
        let unread = FlagMutation {
            target: FlagTarget::messages(vec![message_ref.clone()]),
            flags: vec!["seen".into()],
            add: false,
        };
        supersede_pending_flag_ops(&conn, "acc1", std::slice::from_ref(&unread)).unwrap();
        let (_, payload) = mail_op_to_outbox(&MailOp::SetFlags {
            mutations: vec![unread],
        })
        .unwrap();
        queue_offline_op(&conn, "acc1", "set_flags", &payload).unwrap();

        supersede_pending_flag_ops(
            &conn,
            "acc1",
            &[FlagMutation {
                target: FlagTarget::messages(vec![message_ref]),
                flags: vec!["seen".into()],
                add: true,
            }],
        )
        .unwrap();

        let pending = get_pending_ops(&conn, "acc1").unwrap();
        assert_eq!(pending.len(), 1);
        match outbox_to_mail_op(&pending[0]).unwrap() {
            MailOp::SetFlags { mutations } => assert_eq!(
                mutations[0].target,
                FlagTarget::AllMessagesInFolders {
                    folder_paths: vec!["INBOX".into()],
                    excluded_refs: Vec::new(),
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
            retry_count: 0,
        };

        assert!(outbox_to_mail_op(&entry).is_none());
    }
}
