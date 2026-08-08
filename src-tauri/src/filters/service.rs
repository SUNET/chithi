use std::collections::HashMap;
use std::sync::Arc;

use crate::db;
use crate::db::pool::DbPool;
use crate::error::{Error, Result};
use crate::filters::engine::{self, AddressEntry, MessageData};
use crate::filters::rules::{FilterAction, FilterRule};
use crate::mail::compat::BackendMessageRef;
use crate::mail::imap::{ImapConfig, ImapConnection};

/// Detect Graph "item not found" errors — i.e. the local id is stale because
/// the message was moved or deleted server-side. Matches both the HTTP 404
/// status and the Graph-specific `ErrorItemNotFound` error code so we don't
/// false-positive on other "not found" responses (e.g. folder not found).
fn is_graph_item_not_found(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("404") && s.contains("ErrorItemNotFound")
}

/// Apply all enabled filters for an account to all messages in a given folder.
/// Returns the number of messages that had at least one action applied.
pub(crate) async fn apply_filters_to_folder(
    db: &Arc<DbPool>,
    account_id: &str,
    folder_path: &str,
) -> Result<u32> {
    // 1. Load filters from DB
    let (rules, messages, account) = {
        let conn = db.reader();

        let rules = db::filters::list_filters(&conn, Some(account_id))?;
        let enabled_rules: Vec<FilterRule> = rules.into_iter().filter(|r| r.enabled).collect();

        if enabled_rules.is_empty() {
            log::info!("No enabled filters for account {}", account_id);
            return Ok(0);
        }

        // 2. Load all messages in the folder
        let messages = load_folder_messages(&conn, account_id, folder_path)?;

        if messages.is_empty() {
            log::info!("No messages in folder '{}'", folder_path);
            return Ok(0);
        }

        let account = db::accounts::get_account_full(&conn, account_id)?;
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
    let action_plan = build_filter_action_plan(&rules, &messages);

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
        "graph" => execute_graph_filter_actions(account_id, &action_plan).await?,
        "jmap" => execute_jmap_filter_actions(&account, folder_path, &action_plan).await?,
        "imap" => {
            execute_imap_filter_actions(account_id, &account, folder_path, &action_plan).await?
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
        let conn = db.writer().await;
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

/// Run the filter engine over a slice of messages and collect the per-message
/// action plan. Pure function: the only side effect is reading rule and
/// message fields. Shared between the explicit `apply_filters_to_folder`
/// command and the sync-time `apply_filters_to_new_messages` helper.
fn build_filter_action_plan(
    rules: &[FilterRule],
    messages: &[MessageData],
) -> Vec<(MessageData, Vec<FilterAction>)> {
    let mut plan = Vec::new();
    for msg in messages {
        let actions = engine::apply_filters(rules, msg);
        if !actions.is_empty() {
            plan.push((msg.clone(), actions));
        }
    }
    plan
}

const MAX_IN_PARAMS: usize = 900;

/// Load `MessageData` rows for a specific set of message IDs, chunked to
/// stay under SQLite's bound-parameter limit (999 by default).
fn load_messages_by_ids(
    conn: &rusqlite::Connection,
    account_id: &str,
    folder_path: &str,
    ids: &[String],
) -> Result<Vec<MessageData>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut all = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(MAX_IN_PARAMS) {
        let msgs = load_messages_by_ids_chunk(conn, account_id, folder_path, chunk)?;
        all.extend(msgs);
    }
    Ok(all)
}

/// Load a single chunk of message IDs (must fit within SQLite parameter limit).
fn load_messages_by_ids_chunk(
    conn: &rusqlite::Connection,
    account_id: &str,
    folder_path: &str,
    ids: &[String],
) -> Result<Vec<MessageData>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders: String = (0..ids.len())
        .map(|i| format!("?{}", i + 3))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id, uid, folder_path, from_name, from_email, to_addresses, cc_addresses, \
                subject, size, has_attachments, flags \
         FROM messages \
         WHERE account_id = ?1 AND folder_path = ?2 AND id IN ({})",
        placeholders
    );

    let mut stmt = conn.prepare(&sql)?;

    let mut params: Vec<&dyn rusqlite::types::ToSql> = Vec::with_capacity(ids.len() + 2);
    params.push(&account_id);
    params.push(&folder_path);
    for id in ids {
        params.push(id);
    }

    let rows = stmt
        .query_map(params.as_slice(), |row| {
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

/// Outcome of a sync-time filter pass. `affected` counts messages that had
/// at least one action planned; `failed_db_ids` lists messages whose
/// planned server-side actions did not all complete — the Graph sync keeps
/// their durable `graph_filters_pending` markers set so the pass is
/// retried on the next cycle instead of being silently dropped.
#[derive(Default)]
pub(crate) struct FilterPassOutcome {
    pub(crate) affected: u32,
    pub(crate) failed_db_ids: Vec<String>,
}

/// Apply enabled filter rules to a bounded set of newly-synced messages in
/// one folder, dispatching to the protocol-specific executor that matches the
/// account's mail binding. Called from the sync paths for JMAP and Graph
/// accounts; IMAP sync handles its own filter pass inline so it can reuse
/// the open `imap::Session`.
///
/// Executor errors are logged and folded into the outcome's
/// `failed_db_ids` (a total executor failure marks every matched message
/// failed) so a transient JMAP/Graph failure cannot poison the
/// surrounding sync — the messages are already in the DB and are retried
/// on the next cycle (Graph) or via the manual "Apply Filters" button.
pub(crate) async fn apply_filters_to_new_messages(
    db: &Arc<DbPool>,
    account_id: &str,
    folder_path: &str,
    new_ids: &[String],
) -> Result<FilterPassOutcome> {
    if new_ids.is_empty() {
        return Ok(FilterPassOutcome::default());
    }

    let (rules, messages, account) = {
        let conn = db.reader();
        let all_rules = db::filters::list_filters(&conn, Some(account_id))?;
        let enabled_rules: Vec<FilterRule> = all_rules.into_iter().filter(|r| r.enabled).collect();
        if enabled_rules.is_empty() {
            return Ok(FilterPassOutcome::default());
        }
        let messages = load_messages_by_ids(&conn, account_id, folder_path, new_ids)?;
        if messages.is_empty() {
            return Ok(FilterPassOutcome::default());
        }
        let account = db::accounts::get_account_full(&conn, account_id)?;
        (enabled_rules, messages, account)
    };

    let action_plan = build_filter_action_plan(&rules, &messages);
    if action_plan.is_empty() {
        return Ok(FilterPassOutcome::default());
    }
    let affected_count = action_plan.len() as u32;
    let matched_ids: Vec<String> = action_plan.iter().map(|(m, _)| m.id.clone()).collect();

    log::info!(
        "Sync-time filters: {} new messages, {} matched in '{}' (protocol={})",
        messages.len(),
        affected_count,
        folder_path,
        account.mail_protocol_str()
    );

    // Dispatch by protocol. IMAP is handled by sync.rs's inline executor
    // (which reuses the open Session); anything we don't know how to dispatch
    // is a silent no-op so a misconfigured account never blocks sync.
    let result = match account.mail_protocol_str() {
        "jmap" => match execute_jmap_filter_actions(&account, folder_path, &action_plan).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "Sync-time JMAP filter execution failed for '{}': {}",
                    folder_path,
                    e
                );
                return Ok(FilterPassOutcome {
                    affected: 0,
                    failed_db_ids: matched_ids,
                });
            }
        },
        "graph" => match execute_graph_filter_actions(account_id, &action_plan).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "Sync-time Graph filter execution failed for '{}': {}",
                    folder_path,
                    e
                );
                return Ok(FilterPassOutcome {
                    affected: 0,
                    failed_db_ids: matched_ids,
                });
            }
        },
        "imap" => return Ok(FilterPassOutcome::default()),
        other => {
            log::debug!(
                "Sync-time filters: protocol '{}' has no sync-time executor; skipping",
                other
            );
            return Ok(FilterPassOutcome::default());
        }
    };

    // Prune local rows whose server-side state was confirmed changed. Mirrors
    // the cleanup at the end of `apply_filters_to_folder`.
    let mut to_remove = result.moved_db_ids;
    to_remove.extend(result.deleted_db_ids);
    to_remove.sort();
    to_remove.dedup();
    if !to_remove.is_empty() {
        let conn = db.writer().await;
        db::messages::delete_messages_by_ids(&conn, &to_remove)?;
    }

    if !result.errors.is_empty() {
        log::warn!(
            "Sync-time filters: {} executor error(s) in '{}'; first: {}",
            result.errors.len(),
            folder_path,
            result.errors[0]
        );
    }

    let mut failed_db_ids = result.failed_db_ids;
    failed_db_ids.sort();
    failed_db_ids.dedup();
    Ok(FilterPassOutcome {
        affected: affected_count,
        failed_db_ids,
    })
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
    /// DB ids whose planned actions did NOT all complete server-side.
    /// The Graph sync uses this to keep those messages' durable
    /// `graph_filters_pending` markers set so the pass is retried, while
    /// still clearing markers for messages that finished.
    failed_db_ids: Vec<String>,
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
    // really changed server-side — and, on failure, WHICH messages still
    // owe their filter pass (result.failed_db_ids).
    let mut moves: Vec<(String, String, String)> = Vec::new(); // (db_id, graph_id, target)
    let mut deletes: Vec<(String, String)> = Vec::new(); // (db_id, graph_id)
    let mut mark_read: Vec<(String, String)> = Vec::new(); // (db_id, graph_id)
    let mut mark_unread: Vec<(String, String)> = Vec::new(); // (db_id, graph_id)

    for (msg, actions) in action_plan {
        let graph_id = BackendMessageRef::graph_from_db_id(account_id, &msg.id)
            .into_graph_item_id()
            .expect("Graph parser must return a Graph reference");
        for action in actions {
            match action {
                FilterAction::Move { target } => {
                    moves.push((msg.id.clone(), graph_id.clone(), target.clone()));
                }
                FilterAction::Delete => {
                    deletes.push((msg.id.clone(), graph_id.clone()));
                }
                FilterAction::MarkRead => mark_read.push((msg.id.clone(), graph_id.clone())),
                FilterAction::MarkUnread => mark_unread.push((msg.id.clone(), graph_id.clone())),
                FilterAction::Flag { value } if value.eq_ignore_ascii_case("seen") => {
                    mark_read.push((msg.id.clone(), graph_id.clone()));
                }
                FilterAction::Unflag { value } if value.eq_ignore_ascii_case("seen") => {
                    mark_unread.push((msg.id.clone(), graph_id.clone()));
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

    if !mark_read.is_empty() {
        log::info!("Graph: marking {} messages as read", mark_read.len());
        let graph_ids: Vec<String> = mark_read.iter().map(|(_, g)| g.clone()).collect();
        match client.set_read_status_batch(&graph_ids, true).await {
            Ok(outcomes) => {
                for ((db_id, graph_id), outcome) in mark_read.iter().zip(outcomes) {
                    if let Err(e) = outcome {
                        log::warn!("Graph mark-read failed for id={}: {}", graph_id, e);
                        result.errors.push(format!("mark-read {}: {}", graph_id, e));
                        result.failed_db_ids.push(db_id.clone());
                    }
                }
            }
            Err(e) => {
                log::warn!("Graph mark-read batch failed: {}", e);
                result.errors.push(format!("mark-read: {}", e));
                result
                    .failed_db_ids
                    .extend(mark_read.iter().map(|(d, _)| d.clone()));
            }
        }
    }
    if !mark_unread.is_empty() {
        log::info!("Graph: marking {} messages as unread", mark_unread.len());
        let graph_ids: Vec<String> = mark_unread.iter().map(|(_, g)| g.clone()).collect();
        match client.set_read_status_batch(&graph_ids, false).await {
            Ok(outcomes) => {
                for ((db_id, graph_id), outcome) in mark_unread.iter().zip(outcomes) {
                    if let Err(e) = outcome {
                        log::warn!("Graph mark-unread failed for id={}: {}", graph_id, e);
                        result
                            .errors
                            .push(format!("mark-unread {}: {}", graph_id, e));
                        result.failed_db_ids.push(db_id.clone());
                    }
                }
            }
            Err(e) => {
                log::warn!("Graph mark-unread batch failed: {}", e);
                result.errors.push(format!("mark-unread: {}", e));
                result
                    .failed_db_ids
                    .extend(mark_unread.iter().map(|(d, _)| d.clone()));
            }
        }
    }

    // Moves and deletes go through Graph JSON batching (20 sub-requests
    // per round trip). The old one-HTTP-call-per-message loop turned a
    // filter run over a large folder into hundreds of sequential round
    // trips and a large slice of the mailbox throttling budget.
    let mut moved_db_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stale_count = 0u32;

    // A message can carry several Move actions (multiple matching rules
    // without stop-processing). Only the FIRST planned move is executed —
    // `moves` preserves rule-priority order, and executing later ones
    // would race a stale id anyway. Deduplicate BEFORE grouping by
    // target: the group map iterates in arbitrary order, so without this
    // the winning destination would be nondeterministic.
    let mut planned_move: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut moves_by_target: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (db_id, graph_id, target) in &moves {
        if !planned_move.insert(db_id.clone()) {
            log::debug!(
                "Graph: ignoring secondary move of {} to '{}' (first planned target wins)",
                db_id,
                target
            );
            continue;
        }
        moves_by_target
            .entry(target.clone())
            .or_default()
            .push((db_id.clone(), graph_id.clone()));
    }
    for (target, items) in &moves_by_target {
        log::info!("Graph: moving {} messages to '{}'", items.len(), target);
        let graph_ids: Vec<String> = items.iter().map(|(_, gid)| gid.clone()).collect();
        match client.move_messages_batch(&graph_ids, target).await {
            Ok(outcomes) => {
                for ((db_id, graph_id), outcome) in items.iter().zip(outcomes) {
                    match outcome {
                        Ok(()) => {
                            moved_db_set.insert(db_id.clone());
                            result.moved_db_ids.push(db_id.clone());
                        }
                        Err(e) if is_graph_item_not_found(&e) => {
                            // Message no longer exists at this id — treat the
                            // local row as stale and prune it. A sync will
                            // repopulate the folder with current server state.
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
                            result.failed_db_ids.push(db_id.clone());
                        }
                    }
                }
            }
            Err(e) => {
                log::warn!("Graph move batch to '{}' failed: {}", target, e);
                result.errors.push(format!("move batch: {}", e));
                result
                    .failed_db_ids
                    .extend(items.iter().map(|(d, _)| d.clone()));
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
        let graph_ids: Vec<String> = to_delete.iter().map(|(_, gid)| gid.clone()).collect();
        match client.delete_messages_batch(&graph_ids).await {
            Ok(outcomes) => {
                for ((db_id, graph_id), outcome) in to_delete.iter().zip(outcomes) {
                    match outcome {
                        Ok(()) => result.deleted_db_ids.push(db_id.clone()),
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
                            result.failed_db_ids.push(db_id.clone());
                        }
                    }
                }
            }
            Err(e) => {
                log::warn!("Graph delete batch failed: {}", e);
                result.errors.push(format!("delete batch: {}", e));
                result
                    .failed_db_ids
                    .extend(to_delete.iter().map(|(d, _)| d.clone()));
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
    let jmap_config = crate::auth::build_jmap_config(account).await?;
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
        let Some(jmap_id) =
            BackendMessageRef::jmap_from_db_id(&account.id, &msg.folder_path, &msg.id)
                .and_then(BackendMessageRef::into_jmap_email_id)
        else {
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

    // ---- Filter service helper tests ----
    //
    // These cover the code path that JMAP and Graph sync take on each batch of
    // newly inserted messages. They exercise the planning + DB cleanup layers
    // directly against in-memory SQLite. The network-touching protocol
    // executors (`execute_jmap_filter_actions` / `execute_graph_filter_actions`)
    // are intentionally not invoked — those are already exercised in production
    // via `apply_filters_to_folder`.

    use crate::db;
    use crate::filters::rules::{
        Condition, ConditionField, ConditionOp, FilterAction, FilterRule, MatchType,
    };
    use rusqlite::Connection;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        // Schema initialization enables foreign keys; insert a parent
        // accounts row before any messages or rules so FK constraints
        // pass.
        db::schema::initialize(&conn).expect("initialize schema");
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, username) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["acc1", "Test", "test@example.com", "test@example.com"],
        )
        .unwrap();
        conn
    }

    fn insert_test_message(
        conn: &Connection,
        id: &str,
        folder: &str,
        from_email: &str,
        subject: &str,
    ) {
        let msg = db::messages::NewMessage {
            id: id.to_string(),
            account_id: "acc1".to_string(),
            folder_path: folder.to_string(),
            uid: 0,
            message_id: None,
            in_reply_to: None,
            thread_id: None,
            subject: Some(subject.to_string()),
            from_name: None,
            from_email: from_email.to_string(),
            to_addresses: "[]".to_string(),
            cc_addresses: "[]".to_string(),
            date: "2026-05-15T00:00:00Z".to_string(),
            size: 100,
            has_attachments: false,
            is_encrypted: false,
            is_signed: false,
            flags: "[]".to_string(),
            maildir_path: String::new(),
            snippet: None,
        };
        db::messages::insert_message(conn, &msg).unwrap();
    }

    fn make_rule(id: &str, contains: &str, actions: Vec<FilterAction>) -> FilterRule {
        FilterRule {
            id: id.to_string(),
            account_id: Some("acc1".to_string()),
            name: format!("rule {}", id),
            enabled: true,
            priority: 0,
            match_type: MatchType::All,
            conditions: vec![Condition {
                field: ConditionField::From,
                op: ConditionOp::Contains,
                value: contains.to_string(),
            }],
            actions,
            stop_processing: false,
        }
    }

    #[test]
    fn load_messages_by_ids_returns_only_requested_rows() {
        let conn = setup_test_db();
        insert_test_message(&conn, "acc1_inbox_M1", "inbox", "alice@example.com", "Hi");
        insert_test_message(&conn, "acc1_inbox_M2", "inbox", "bob@example.com", "Hi");
        insert_test_message(&conn, "acc1_inbox_M3", "inbox", "carol@example.com", "Hi");

        let loaded = load_messages_by_ids(
            &conn,
            "acc1",
            "inbox",
            &["acc1_inbox_M1".into(), "acc1_inbox_M3".into()],
        )
        .unwrap();

        let mut ids: Vec<String> = loaded.into_iter().map(|m| m.id).collect();
        ids.sort();
        assert_eq!(ids, vec!["acc1_inbox_M1", "acc1_inbox_M3"]);
    }

    #[test]
    fn load_messages_by_ids_is_folder_scoped() {
        // An id that happens to live in a different folder must not be
        // returned even if the caller asks for it under the wrong folder.
        let conn = setup_test_db();
        insert_test_message(&conn, "acc1_inbox_M1", "inbox", "alice@example.com", "Hi");
        insert_test_message(&conn, "acc1_arch_M2", "archive", "bob@example.com", "Hi");

        let loaded =
            load_messages_by_ids(&conn, "acc1", "inbox", &["acc1_arch_M2".into()]).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_messages_by_ids_short_circuits_on_empty_ids() {
        let conn = setup_test_db();
        let loaded = load_messages_by_ids(&conn, "acc1", "inbox", &[]).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn build_filter_action_plan_only_emits_for_matches() {
        // Three messages: one matches the rule, one doesn't, one is the
        // "pre-existing" message that the caller would not have passed
        // through `load_messages_by_ids` at all.
        let matching = MessageData {
            id: "acc1_inbox_M1".into(),
            uid: 0,
            folder_path: "inbox".into(),
            from_name: None,
            from_email: "newsletter@example.com".into(),
            to_addresses: vec![],
            cc_addresses: vec![],
            subject: Some("hello".into()),
            size: 100,
            has_attachments: false,
            flags: vec![],
        };
        let unrelated = MessageData {
            id: "acc1_inbox_M2".into(),
            uid: 0,
            folder_path: "inbox".into(),
            from_name: None,
            from_email: "friend@example.com".into(),
            to_addresses: vec![],
            cc_addresses: vec![],
            subject: Some("hello".into()),
            size: 100,
            has_attachments: false,
            flags: vec![],
        };

        let rules = vec![
            make_rule(
                "r1",
                "newsletter",
                vec![FilterAction::Move {
                    target: "archive_box".into(),
                }],
            ),
            make_rule("r2", "newsletter", vec![FilterAction::MarkRead]),
        ];

        let plan = build_filter_action_plan(&rules, &[matching.clone(), unrelated]);

        // Only the matching message is in the plan, and it has both actions.
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].0.id, "acc1_inbox_M1");
        assert_eq!(plan[0].1.len(), 2);
        let has_move = plan[0]
            .1
            .iter()
            .any(|a| matches!(a, FilterAction::Move { target } if target == "archive_box"));
        let has_mark_read = plan[0]
            .1
            .iter()
            .any(|a| matches!(a, FilterAction::MarkRead));
        assert!(has_move);
        assert!(has_mark_read);
    }

    #[test]
    fn delete_messages_by_ids_prunes_only_specified_rows() {
        // Simulates what apply_filters_to_new_messages does after a JMAP/Graph
        // executor confirms a move/delete: the DB cleanup must remove exactly
        // those ids and leave everything else untouched.
        let conn = setup_test_db();
        insert_test_message(&conn, "acc1_inbox_M1", "inbox", "a@example.com", "x");
        insert_test_message(&conn, "acc1_inbox_M2", "inbox", "b@example.com", "x");
        insert_test_message(&conn, "acc1_inbox_M3", "inbox", "c@example.com", "x");

        let to_remove = vec!["acc1_inbox_M1".to_string(), "acc1_inbox_M3".to_string()];
        db::messages::delete_messages_by_ids(&conn, &to_remove).unwrap();

        let remaining: Vec<String> = conn
            .prepare("SELECT id FROM messages WHERE account_id = 'acc1'")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(remaining, vec!["acc1_inbox_M2"]);
    }
}
