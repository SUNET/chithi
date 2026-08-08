use tauri::State;

use crate::db;
use crate::error::Result;
use crate::filters::rules::FilterRule;
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

    crate::filters::service::apply_filters_to_folder(&state.db, &account_id, &folder_path).await
}
