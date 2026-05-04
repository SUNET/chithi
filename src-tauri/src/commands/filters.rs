use std::collections::HashMap;
use tauri::State;

use crate::db;
use crate::error::{Error, Result};
use crate::filters::engine::{self, AddressEntry, MessageData};
use crate::filters::rules::{FilterAction, FilterRule};
use crate::mail::imap::{ImapConfig, ImapConnection};
use crate::state::AppState;

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

    // Build IMAP config — O365 needs XOAUTH2 token refresh
    let (imap_password, imap_xoauth2) = if account.auth_method == "oauth-microsoft" {
        let tokens = crate::oauth::load_tokens(&account_id)?
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
        crate::oauth::store_tokens(&account_id, &new)?;
        (new.access_token, true)
    } else {
        (account.password, false)
    };
    let imap_config = ImapConfig {
        host: account.imap_host,
        port: account.imap_port,
        username: account.username,
        password: imap_password,
        use_tls: account.use_tls,
        use_xoauth2: imap_xoauth2,
    };

    log::info!(
        "Running {} filters against {} messages in '{}'",
        rules.len(),
        messages.len(),
        folder_path
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

    // 4. Group IMAP actions by type and execute
    let mut move_targets: HashMap<String, Vec<u32>> = HashMap::new();
    let mut copy_targets: HashMap<String, Vec<u32>> = HashMap::new();
    let mut delete_uids: Vec<u32> = Vec::new();
    let mut flag_add: HashMap<String, Vec<u32>> = HashMap::new();
    let mut flag_remove: HashMap<String, Vec<u32>> = HashMap::new();
    let mut mark_read_uids: Vec<u32> = Vec::new();
    let mut mark_unread_uids: Vec<u32> = Vec::new();

    // Track which message DB ids get moved/deleted so we can clean up local DB
    let mut moved_message_ids: Vec<String> = Vec::new();
    let mut deleted_message_ids: Vec<String> = Vec::new();

    for (msg, actions) in &action_plan {
        for action in actions {
            match action {
                FilterAction::Move { target } => {
                    move_targets
                        .entry(target.clone())
                        .or_default()
                        .push(msg.uid);
                    moved_message_ids.push(msg.id.clone());
                }
                FilterAction::Copy { target } => {
                    copy_targets
                        .entry(target.clone())
                        .or_default()
                        .push(msg.uid);
                }
                FilterAction::Delete => {
                    delete_uids.push(msg.uid);
                    deleted_message_ids.push(msg.id.clone());
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
                FilterAction::MarkRead => {
                    mark_read_uids.push(msg.uid);
                }
                FilterAction::MarkUnread => {
                    mark_unread_uids.push(msg.uid);
                }
                FilterAction::Stop => {
                    // Stop is handled by the engine; no IMAP action needed
                }
            }
        }
    }

    let folder = folder_path.clone();

    // 5. Execute IMAP actions in a blocking thread
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut conn = ImapConnection::connect(&imap_config)?;
        conn.select_folder(&folder)?;

        // Mark read
        if !mark_read_uids.is_empty() {
            log::info!("Marking {} messages as read", mark_read_uids.len());
            conn.set_flags(&mark_read_uids, &["\\Seen"], true)?;
        }

        // Mark unread
        if !mark_unread_uids.is_empty() {
            log::info!("Marking {} messages as unread", mark_unread_uids.len());
            conn.set_flags(&mark_unread_uids, &["\\Seen"], false)?;
        }

        // Add flags
        for (flag, uids) in &flag_add {
            log::info!("Adding flag '{}' to {} messages", flag, uids.len());
            conn.set_flags(uids, &[flag.as_str()], true)?;
        }

        // Remove flags
        for (flag, uids) in &flag_remove {
            log::info!("Removing flag '{}' from {} messages", flag, uids.len());
            conn.set_flags(uids, &[flag.as_str()], false)?;
        }

        // Copy (before move/delete which may expunge)
        for (target, uids) in &copy_targets {
            log::info!("Copying {} messages to '{}'", uids.len(), target);
            conn.copy_messages(uids, target)?;
        }

        // Move
        for (target, uids) in &move_targets {
            log::info!("Moving {} messages to '{}'", uids.len(), target);
            conn.move_messages(uids, target)?;
        }

        // Delete (only messages not already moved)
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

    // 6. Update local DB: remove moved/deleted messages
    {
        let conn = state.db.writer().await;
        let mut to_remove = moved_message_ids;
        to_remove.extend(deleted_message_ids);
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

    log::info!(
        "Apply filters to folder complete: {} messages affected",
        affected_count
    );

    Ok(affected_count)
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
