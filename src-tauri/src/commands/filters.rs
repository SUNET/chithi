use std::collections::HashMap;
use tauri::State;

use crate::db;
use crate::error::{Error, Result};
use crate::filters::engine::{self, AddressEntry, MessageData};
use crate::filters::rules::{FilterAction, FilterRule};
use crate::mail::imap::{ImapConfig, ImapConnection};
use crate::state::AppState;

/// Strip the `{account_id}_` prefix from a composite DB id to recover the Graph message id.
fn graph_id_from_db_id<'a>(account_id: &str, db_id: &'a str) -> &'a str {
    db_id
        .strip_prefix(&format!("{}_", account_id))
        .unwrap_or(db_id)
}

/// Detect Graph "item not found" errors — i.e. the local id is stale because
/// the message was moved or deleted server-side. Matches both the HTTP 404
/// status and the Graph-specific `ErrorItemNotFound` error code so we don't
/// false-positive on other "not found" responses (e.g. folder not found).
fn is_graph_item_not_found(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("404") && s.contains("ErrorItemNotFound")
}

/// Extract the JMAP email id from a composite DB id of form
/// `{account_id}_{mailbox_id}_{email_id}` by stripping the known
/// `{account_id}_{mailbox_id}_` prefix. Splitting on `_` is unsafe because
/// JMAP mailbox ids and email ids are server-opaque and may legally contain
/// underscores.
fn jmap_id_from_db_id(account_id: &str, mailbox_id: &str, db_id: &str) -> Option<String> {
    let prefix = format!("{}_{}_", account_id, mailbox_id);
    db_id.strip_prefix(&prefix).map(|s| s.to_string())
}

/// List all filter rules for an account (plus global rules).
#[tauri::command]
pub async fn list_filters(
    state: State<'_, AppState>,
    account_id: Option<String>,
) -> Result<Vec<FilterRule>> {
    log::info!("List filters command: account_id={:?}", account_id);
    let conn = state.db.reader();
    let rules = db::filters::list_filters(&conn, account_id.as_deref())?;
    log::info!("Found {} filter rules", rules.len());
    Ok(rules)
}

/// Save (upsert) a filter rule. Inserts if the id is new, updates if it exists.
#[tauri::command]
pub async fn save_filter(state: State<'_, AppState>, rule: FilterRule) -> Result<()> {
    log::info!("Save filter command: id={} name='{}'", rule.id, rule.name);
    let conn = state.db.writer().await;

    // Check if the rule already exists
    match db::filters::get_filter(&conn, &rule.id) {
        Ok(_) => {
            log::info!("Filter '{}' exists, updating", rule.id);
            db::filters::update_filter(&conn, &rule)?;
        }
        Err(_) => {
            log::info!("Filter '{}' is new, inserting", rule.id);
            db::filters::insert_filter(&conn, &rule)?;
        }
    }

    Ok(())
}

/// Delete a filter rule by id.
#[tauri::command]
pub async fn delete_filter(state: State<'_, AppState>, filter_id: String) -> Result<()> {
    log::info!("Delete filter command: id={}", filter_id);
    let conn = state.db.writer().await;
    db::filters::delete_filter(&conn, &filter_id)?;
    Ok(())
}

/// Apply all enabled filters for an account to all messages in a given folder.
/// Returns the number of messages that had at least one action applied.
#[tauri::command]
pub async fn apply_filters_to_folder(
    state: State<'_, AppState>,
    account_id: String,
    folder_path: String,
) -> Result<u32> {
    log::info!(
        "Apply filters to folder command: account={} folder='{}'",
        account_id,
        folder_path
    );

    // 1. Load filters from DB
    let (rules, messages, account) = {
        let conn = state.db.reader();

        let rules = db::filters::list_filters(&conn, Some(&account_id))?;
        let enabled_rules: Vec<FilterRule> = rules.into_iter().filter(|r| r.enabled).collect();

        if enabled_rules.is_empty() {
            log::info!("No enabled filters for account {}", account_id);
            return Ok(0);
        }

        // 2. Load all messages in the folder
        let messages = load_folder_messages(&conn, &account_id, &folder_path)?;

        if messages.is_empty() {
            log::info!("No messages in folder '{}'", folder_path);
            return Ok(0);
        }

        let account = db::accounts::get_account_full(&conn, &account_id)?;
        (enabled_rules, messages, account)
    };

    log::info!(
        "Running {} filters against {} messages in '{}' (protocol={})",
        rules.len(),
        messages.len(),
        folder_path,
        account.mail_protocol_str()
    );

    // 3. For each message, run filter engine to get actions
    let mut action_plan: Vec<(MessageData, Vec<FilterAction>)> = Vec::new();
    for msg in &messages {
        let actions = engine::apply_filters(&rules, msg);
        if !actions.is_empty() {
            action_plan.push((msg.clone(), actions));
        }
    }

    let affected_count = action_plan.len() as u32;

    if action_plan.is_empty() {
        log::info!("No messages matched any filter rules");
        return Ok(0);
    }

    log::info!(
        "{} messages matched filter rules, executing actions",
        affected_count
    );

    // 4. Execute server-side actions per protocol. Each execute function
    //    returns the set of DB ids actually moved or deleted (so the caller
    //    only cleans up rows whose server-side state really changed). The
    //    Graph path tolerates per-message failures so one stale id can't
    //    block the rest of the batch.
    let result = match account.mail_protocol_str() {
        "graph" => execute_graph_filter_actions(&account_id, &action_plan).await?,
        "jmap" => execute_jmap_filter_actions(&account, &folder_path, &action_plan).await?,
        "imap" => {
            execute_imap_filter_actions(&account_id, &account, &folder_path, &action_plan).await?
        }
        "" => {
            // Mail binding disabled (DAV-only account, etc.). There should be
            // no messages in `messages` for this account, but guard the
            // dispatch explicitly so we never fall through to IMAP with
            // bogus host/port.
            log::info!(
                "Account {} has no mail protocol configured; skipping filter actions",
                account_id
            );
            return Ok(0);
        }
        other => {
            return Err(Error::Other(format!(
                "Unknown mail protocol '{}' for account {}",
                other, account_id
            )));
        }
    };

    // Capture counts before consuming the result for DB cleanup so we can
    // report them in the partial-failure message below.
    let succeeded = result.moved_db_ids.len() + result.deleted_db_ids.len();
    let errors = result.errors;

    // 5. Update local DB: remove server-confirmed moved/deleted messages
    {
        let conn = state.db.writer().await;
        let mut to_remove = result.moved_db_ids;
        to_remove.extend(result.deleted_db_ids);
        to_remove.sort();
        to_remove.dedup();
        if !to_remove.is_empty() {
            log::info!(
                "Removing {} moved/deleted messages from local DB",
                to_remove.len()
            );
            db::messages::delete_messages_by_ids(&conn, &to_remove)?;
        }
    }

    if !errors.is_empty() {
        log::warn!(
            "Apply filters to folder finished with {} failure(s); first: {}",
            errors.len(),
            errors[0]
        );
        // Surface a partial-failure error message so the UI shows it.
        // `errors` mixes per-message (Graph move/delete) and per-batch
        // (Graph mark-read, JMAP set_flags) entries, and `affected_count`
        // counts matched messages, not attempted actions — so we report
        // the failure count plus the first error verbatim rather than
        // claiming a precise denominator.
        return Err(Error::Other(format!(
            "{} failure(s) across {} matched message(s) ({} server-confirmed change(s)); first: {}",
            errors.len(),
            affected_count,
            succeeded,
            errors[0]
        )));
    }

    log::info!(
        "Apply filters to folder complete: {} messages affected",
        affected_count
    );

    Ok(affected_count)
}

/// Result of executing a filter action plan against the server.
/// Contains the DB ids whose server-side state was actually changed (so the
/// caller can prune those rows locally) plus any per-message errors that
/// occurred during execution.
#[derive(Default)]
struct ExecutionResult {
    moved_db_ids: Vec<String>,
    deleted_db_ids: Vec<String>,
    errors: Vec<String>,
}

/// Execute filter actions over IMAP using a blocking connection.
/// IMAP move/delete are batched per folder, so partial success is not
/// represented here — either the whole batch op succeeds or the function
/// returns Err.
async fn execute_imap_filter_actions(
    account_id: &str,
    account: &db::accounts::AccountFull,
    folder_path: &str,
    action_plan: &[(MessageData, Vec<FilterAction>)],
) -> Result<ExecutionResult> {
    // Build IMAP config — O365 needs XOAUTH2 token refresh
    let (imap_password, imap_xoauth2) = if account.auth_method == "oauth-microsoft" {
        let tokens = crate::oauth::load_tokens(account_id)?
            .ok_or_else(|| Error::Other("No O365 tokens".into()))?;
        let refresh = tokens
            .refresh_token
            .ok_or_else(|| Error::Other("No O365 refresh token".into()))?;
        let new = crate::oauth::refresh_with_scopes(
            &crate::oauth::MICROSOFT,
            &refresh,
            crate::oauth::MICROSOFT_IMAP_SCOPES,
        )
        .await?;
        crate::oauth::store_tokens(account_id, &new)?;
        (new.access_token, true)
    } else {
        (account.password.clone(), false)
    };
    let imap_config = ImapConfig {
        host: account.imap_host.clone(),
        port: account.imap_port,
        username: account.username.clone(),
        password: imap_password,
        use_tls: account.use_tls,
        use_xoauth2: imap_xoauth2,
    };

    let mut move_targets: HashMap<String, Vec<u32>> = HashMap::new();
    let mut copy_targets: HashMap<String, Vec<u32>> = HashMap::new();
    let mut delete_uids: Vec<u32> = Vec::new();
    let mut flag_add: HashMap<String, Vec<u32>> = HashMap::new();
    let mut flag_remove: HashMap<String, Vec<u32>> = HashMap::new();
    let mut mark_read_uids: Vec<u32> = Vec::new();
    let mut mark_unread_uids: Vec<u32> = Vec::new();

    for (msg, actions) in action_plan {
        for action in actions {
            match action {
                FilterAction::Move { target } => {
                    move_targets
                        .entry(target.clone())
                        .or_default()
                        .push(msg.uid);
                }
                FilterAction::Copy { target } => {
                    copy_targets
                        .entry(target.clone())
                        .or_default()
                        .push(msg.uid);
                }
                FilterAction::Delete => {
                    delete_uids.push(msg.uid);
                }
                FilterAction::Flag { value } => {
                    flag_add
                        .entry(format!("\\{}", capitalize_flag(value)))
                        .or_default()
                        .push(msg.uid);
                }
                FilterAction::Unflag { value } => {
                    flag_remove
                        .entry(format!("\\{}", capitalize_flag(value)))
                        .or_default()
                        .push(msg.uid);
                }
                FilterAction::MarkRead => mark_read_uids.push(msg.uid),
                FilterAction::MarkUnread => mark_unread_uids.push(msg.uid),
                FilterAction::Stop => {}
            }
        }
    }

    let folder = folder_path.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut conn = ImapConnection::connect(&imap_config)?;
        conn.select_folder(&folder)?;

        if !mark_read_uids.is_empty() {
            log::info!("Marking {} messages as read", mark_read_uids.len());
            conn.set_flags(&mark_read_uids, &["\\Seen"], true)?;
        }
        if !mark_unread_uids.is_empty() {
            log::info!("Marking {} messages as unread", mark_unread_uids.len());
            conn.set_flags(&mark_unread_uids, &["\\Seen"], false)?;
        }
        for (flag, uids) in &flag_add {
            log::info!("Adding flag '{}' to {} messages", flag, uids.len());
            conn.set_flags(uids, &[flag.as_str()], true)?;
        }
        for (flag, uids) in &flag_remove {
            log::info!("Removing flag '{}' from {} messages", flag, uids.len());
            conn.set_flags(uids, &[flag.as_str()], false)?;
        }
        // Copy before move/delete (which may expunge)
        for (target, uids) in &copy_targets {
            log::info!("Copying {} messages to '{}'", uids.len(), target);
            conn.copy_messages(uids, target)?;
        }
        for (target, uids) in &move_targets {
            log::info!("Moving {} messages to '{}'", uids.len(), target);
            conn.move_messages(uids, target)?;
        }
        let delete_remaining: Vec<u32> = delete_uids
            .iter()
            .filter(|uid| {
                !move_targets
                    .values()
                    .any(|moved_uids| moved_uids.contains(uid))
            })
            .copied()
            .collect();
        if !delete_remaining.is_empty() {
            log::info!("Deleting {} messages", delete_remaining.len());
            conn.delete_messages(&delete_remaining)?;
        }
        conn.logout();
        Ok(())
    })
    .await
    .map_err(|e| Error::Other(format!("Filter action task panicked: {}", e)))??;

    // IMAP ops above are batched and atomic; on success every planned move
    // and delete is taken to have applied.
    let mut result = ExecutionResult::default();
    for (msg, actions) in action_plan {
        for action in actions {
            match action {
                FilterAction::Move { .. } => result.moved_db_ids.push(msg.id.clone()),
                FilterAction::Delete => result.deleted_db_ids.push(msg.id.clone()),
                _ => {}
            }
        }
    }
    Ok(result)
}

/// Execute filter actions against Microsoft Graph for an O365 account using
/// the Graph mail protocol. Per-message Move and Delete failures (e.g. a
/// stale id whose message was already moved server-side, returning 404
/// `ErrorItemNotFound`) are logged and skipped so one bad id does not block
/// the rest of the batch. Copy and non-Seen flag changes are unsupported.
async fn execute_graph_filter_actions(
    account_id: &str,
    action_plan: &[(MessageData, Vec<FilterAction>)],
) -> Result<ExecutionResult> {
    let token = crate::mail::graph::get_graph_token(account_id).await?;
    let client = crate::mail::graph::GraphClient::new(&token);

    // Pair every operation with its DB id so we can report which messages
    // really changed server-side.
    let mut moves: Vec<(String, String, String)> = Vec::new(); // (db_id, graph_id, target)
    let mut deletes: Vec<(String, String)> = Vec::new(); // (db_id, graph_id)
    let mut mark_read_ids: Vec<String> = Vec::new();
    let mut mark_unread_ids: Vec<String> = Vec::new();

    for (msg, actions) in action_plan {
        let graph_id = graph_id_from_db_id(account_id, &msg.id).to_string();
        for action in actions {
            match action {
                FilterAction::Move { target } => {
                    moves.push((msg.id.clone(), graph_id.clone(), target.clone()));
                }
                FilterAction::Delete => {
                    deletes.push((msg.id.clone(), graph_id.clone()));
                }
                FilterAction::MarkRead => mark_read_ids.push(graph_id.clone()),
                FilterAction::MarkUnread => mark_unread_ids.push(graph_id.clone()),
                FilterAction::Flag { value } if value.eq_ignore_ascii_case("seen") => {
                    mark_read_ids.push(graph_id.clone());
                }
                FilterAction::Unflag { value } if value.eq_ignore_ascii_case("seen") => {
                    mark_unread_ids.push(graph_id.clone());
                }
                FilterAction::Flag { value } | FilterAction::Unflag { value } => {
                    log::warn!(
                        "Filter flag action '{}' not supported on Graph protocol; skipping",
                        value
                    );
                }
                FilterAction::Copy { target } => {
                    log::warn!(
                        "Filter copy action to '{}' not supported on Graph protocol; skipping",
                        target
                    );
                }
                FilterAction::Stop => {}
            }
        }
    }

    let mut result = ExecutionResult::default();

    if !mark_read_ids.is_empty() {
        log::info!("Graph: marking {} messages as read", mark_read_ids.len());
        if let Err(e) = client.set_read_status(&mark_read_ids, true).await {
            log::warn!("Graph mark-read batch failed: {}", e);
            result.errors.push(format!("mark-read: {}", e));
        }
    }
    if !mark_unread_ids.is_empty() {
        log::info!(
            "Graph: marking {} messages as unread",
            mark_unread_ids.len()
        );
        if let Err(e) = client.set_read_status(&mark_unread_ids, false).await {
            log::warn!("Graph mark-unread batch failed: {}", e);
            result.errors.push(format!("mark-unread: {}", e));
        }
    }

    log::info!("Graph: moving {} messages", moves.len());
    let mut moved_db_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stale_count = 0u32;
    for (db_id, graph_id, target) in &moves {
        match client.move_message(graph_id, target).await {
            Ok(_) => {
                moved_db_set.insert(db_id.clone());
                result.moved_db_ids.push(db_id.clone());
            }
            Err(e) if is_graph_item_not_found(&e) => {
                // Message no longer exists at this id — treat the local row
                // as stale and prune it. A sync will repopulate the folder
                // with current server state.
                log::info!(
                    "Graph move 404 for id={}: pruning stale local row",
                    graph_id
                );
                result.deleted_db_ids.push(db_id.clone());
                stale_count += 1;
            }
            Err(e) => {
                log::warn!("Graph move failed for id={}: {}", graph_id, e);
                result.errors.push(format!("move: {}", e));
            }
        }
    }

    // Delete only messages that weren't already moved (a Move action
    // implicitly deletes from source). On per-message error keep going.
    let to_delete: Vec<&(String, String)> = deletes
        .iter()
        .filter(|(db_id, _)| !moved_db_set.contains(db_id))
        .collect();
    if !to_delete.is_empty() {
        log::info!("Graph: deleting {} messages", to_delete.len());
        for (db_id, graph_id) in &to_delete {
            match client.delete_message(graph_id).await {
                Ok(_) => result.deleted_db_ids.push(db_id.clone()),
                Err(e) if is_graph_item_not_found(&e) => {
                    log::info!(
                        "Graph delete 404 for id={}: pruning stale local row",
                        graph_id
                    );
                    result.deleted_db_ids.push(db_id.clone());
                    stale_count += 1;
                }
                Err(e) => {
                    log::warn!("Graph delete failed for id={}: {}", graph_id, e);
                    result.errors.push(format!("delete: {}", e));
                }
            }
        }
    }

    if stale_count > 0 {
        log::warn!(
            "Graph: pruned {} stale local row(s); a folder sync will repopulate",
            stale_count
        );
    }

    Ok(result)
}

/// Execute filter actions against a JMAP server. Each JMAP method call is
/// batched so partial-failure reporting from the server is not propagated;
/// either a batch succeeds and every planned id in it is taken to have
/// applied, or the function returns Err. Copy is unsupported.
async fn execute_jmap_filter_actions(
    account: &db::accounts::AccountFull,
    folder_path: &str,
    action_plan: &[(MessageData, Vec<FilterAction>)],
) -> Result<ExecutionResult> {
    let jmap_config = crate::commands::sync_cmd::build_jmap_config(account).await?;
    let conn = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;

    // (db_id, jmap_id, target) for moves; (db_id, jmap_id) for deletes
    let mut moves: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut deletes: Vec<(String, String)> = Vec::new();
    let mut flag_add: HashMap<String, Vec<String>> = HashMap::new();
    let mut flag_remove: HashMap<String, Vec<String>> = HashMap::new();
    let mut mark_read_ids: Vec<String> = Vec::new();
    let mut mark_unread_ids: Vec<String> = Vec::new();

    for (msg, actions) in action_plan {
        // The JMAP mailbox id is stored as `folder_path`; use that to strip
        // the exact prefix instead of splitting on `_`, which is unsafe
        // because mailbox ids and email ids are server-opaque.
        let Some(jmap_id) = jmap_id_from_db_id(&account.id, &msg.folder_path, &msg.id) else {
            log::warn!(
                "Skipping JMAP filter action for message '{}' with unexpected id format",
                msg.id
            );
            continue;
        };
        for action in actions {
            match action {
                FilterAction::Move { target } => {
                    moves
                        .entry(target.clone())
                        .or_default()
                        .push((msg.id.clone(), jmap_id.clone()));
                }
                FilterAction::Delete => deletes.push((msg.id.clone(), jmap_id.clone())),
                FilterAction::Flag { value } => {
                    flag_add
                        .entry(value.clone())
                        .or_default()
                        .push(jmap_id.clone());
                }
                FilterAction::Unflag { value } => {
                    flag_remove
                        .entry(value.clone())
                        .or_default()
                        .push(jmap_id.clone());
                }
                FilterAction::MarkRead => mark_read_ids.push(jmap_id.clone()),
                FilterAction::MarkUnread => mark_unread_ids.push(jmap_id.clone()),
                FilterAction::Copy { target } => {
                    log::warn!(
                        "Filter copy action to '{}' not supported on JMAP protocol; skipping",
                        target
                    );
                }
                FilterAction::Stop => {}
            }
        }
    }

    if !mark_read_ids.is_empty() {
        log::info!("JMAP: marking {} messages as read", mark_read_ids.len());
        conn.set_flags(&jmap_config, &mark_read_ids, &["seen"], true)
            .await?;
    }
    if !mark_unread_ids.is_empty() {
        log::info!("JMAP: marking {} messages as unread", mark_unread_ids.len());
        conn.set_flags(&jmap_config, &mark_unread_ids, &["seen"], false)
            .await?;
    }
    for (flag, ids) in &flag_add {
        log::info!("JMAP: adding flag '{}' to {} messages", flag, ids.len());
        conn.set_flags(&jmap_config, ids, &[flag.as_str()], true)
            .await?;
    }
    for (flag, ids) in &flag_remove {
        log::info!("JMAP: removing flag '{}' from {} messages", flag, ids.len());
        conn.set_flags(&jmap_config, ids, &[flag.as_str()], false)
            .await?;
    }

    let mut result = ExecutionResult::default();
    let mut moved_db_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (target, pairs) in &moves {
        let jmap_ids: Vec<String> = pairs.iter().map(|(_, jid)| jid.clone()).collect();
        log::info!("JMAP: moving {} messages to '{}'", jmap_ids.len(), target);
        conn.move_emails(&jmap_config, &jmap_ids, folder_path, target)
            .await?;
        for (db_id, _) in pairs {
            moved_db_set.insert(db_id.clone());
            result.moved_db_ids.push(db_id.clone());
        }
    }

    let to_delete: Vec<&(String, String)> = deletes
        .iter()
        .filter(|(db_id, _)| !moved_db_set.contains(db_id))
        .collect();
    if !to_delete.is_empty() {
        let jmap_ids: Vec<String> = to_delete.iter().map(|(_, jid)| jid.clone()).collect();
        log::info!("JMAP: deleting {} messages", jmap_ids.len());
        conn.delete_emails(&jmap_config, &jmap_ids).await?;
        for (db_id, _) in &to_delete {
            result.deleted_db_ids.push(db_id.clone());
        }
    }

    Ok(result)
}

/// Load all messages in a folder as MessageData structs for filter matching.
fn load_folder_messages(
    conn: &rusqlite::Connection,
    account_id: &str,
    folder_path: &str,
) -> Result<Vec<MessageData>> {
    let mut stmt = conn.prepare(
        "SELECT id, uid, folder_path, from_name, from_email, to_addresses, cc_addresses, \
                subject, size, has_attachments, flags \
         FROM messages \
         WHERE account_id = ?1 AND folder_path = ?2",
    )?;

    let rows = stmt
        .query_map(rusqlite::params![account_id, folder_path], |row| {
            let id: String = row.get(0)?;
            let uid: u32 = row.get(1)?;
            let folder: String = row.get(2)?;
            let from_name: Option<String> = row.get(3)?;
            let from_email: String = row.get(4)?;
            let to_json: String = row.get(5)?;
            let cc_json: String = row.get(6)?;
            let subject: Option<String> = row.get(7)?;
            let size: i64 = row.get(8)?;
            let has_attachments: bool = row.get(9)?;
            let flags_json: String = row.get(10)?;

            Ok((
                id,
                uid,
                folder,
                from_name,
                from_email,
                to_json,
                cc_json,
                subject,
                size,
                has_attachments,
                flags_json,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut messages = Vec::with_capacity(rows.len());
    for (
        id,
        uid,
        folder,
        from_name,
        from_email,
        to_json,
        cc_json,
        subject,
        size,
        has_attach,
        flags_json,
    ) in rows
    {
        let to_addresses: Vec<AddressEntry> = serde_json::from_str(&to_json).unwrap_or_default();
        let cc_addresses: Vec<AddressEntry> = serde_json::from_str(&cc_json).unwrap_or_default();
        let flags: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();

        messages.push(MessageData {
            id,
            uid,
            folder_path: folder,
            from_name,
            from_email,
            to_addresses,
            cc_addresses,
            subject,
            size: size as u64,
            has_attachments: has_attach,
            flags,
        });
    }

    Ok(messages)
}

/// Capitalize the first letter of a flag name for IMAP format (e.g., "seen" -> "Seen").
fn capitalize_flag(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_graph_id_from_db_id_strips_account_prefix() {
        let acc = "acc123";
        let db_id = "acc123_AAMkAGI2T-foo_bar";
        assert_eq!(graph_id_from_db_id(acc, db_id), "AAMkAGI2T-foo_bar");
    }

    #[test]
    fn test_graph_id_from_db_id_returns_input_when_prefix_missing() {
        assert_eq!(
            graph_id_from_db_id("acc123", "raw_graph_id"),
            "raw_graph_id"
        );
    }

    #[test]
    fn test_jmap_id_from_db_id_strips_known_prefix() {
        let id = jmap_id_from_db_id("acc1", "inbox", "acc1_inbox_M123");
        assert_eq!(id.as_deref(), Some("M123"));
    }

    #[test]
    fn test_jmap_id_from_db_id_preserves_underscores_in_email_id() {
        // Email id contains underscores; previous splitn(3, '_') would have
        // truncated this. Prefix-strip handles it correctly.
        let id = jmap_id_from_db_id("acc1", "box", "acc1_box_email_with_underscores");
        assert_eq!(id.as_deref(), Some("email_with_underscores"));
    }

    #[test]
    fn test_jmap_id_from_db_id_handles_underscore_in_mailbox_id() {
        // Server-opaque mailbox id with underscores — splitn-based parsing
        // would have returned "child_M9" or worse here; prefix-strip is exact.
        let id = jmap_id_from_db_id("acc1", "parent_child", "acc1_parent_child_M9");
        assert_eq!(id.as_deref(), Some("M9"));
    }

    #[test]
    fn test_jmap_id_from_db_id_returns_none_when_prefix_missing() {
        assert!(jmap_id_from_db_id("acc1", "inbox", "different_acc_inbox_M1").is_none());
        assert!(jmap_id_from_db_id("acc1", "inbox", "acc1_other_M1").is_none());
        assert!(jmap_id_from_db_id("acc1", "inbox", "no_underscores").is_none());
    }

    #[test]
    fn test_is_graph_item_not_found_matches_404_with_code() {
        let e = Error::Other(
            "Graph POST /me/messages/AAA=/move returned 404 Not Found: \
             {\"error\":{\"code\":\"ErrorItemNotFound\",\"message\":\"...\"}}"
                .into(),
        );
        assert!(is_graph_item_not_found(&e));
    }

    #[test]
    fn test_is_graph_item_not_found_rejects_other_404s() {
        // 404 without the Graph error code (e.g. folder lookup) should not
        // be treated as a stale message id.
        let e = Error::Other("Graph GET /me/mailFolders/X returned 404 Not Found: {}".into());
        assert!(!is_graph_item_not_found(&e));
    }

    #[test]
    fn test_is_graph_item_not_found_rejects_other_statuses() {
        let e = Error::Other(
            "Graph POST /me/messages/AAA=/move returned 403 Forbidden: \
             {\"error\":{\"code\":\"ErrorItemNotFound\"}}"
                .into(),
        );
        assert!(!is_graph_item_not_found(&e));
    }
}
