use rusqlite::{params, Connection};

use crate::error::{Error, Result};
use crate::filters::rules::{FilterAction, FilterRule, MatchType, Condition};

/// List all filter rules, optionally filtered by account_id.
/// If account_id is None, returns all rules (including global ones).
/// If account_id is Some, returns rules for that account plus global rules (account_id IS NULL).
pub fn list_filters(conn: &Connection, account_id: Option<&str>) -> Result<Vec<FilterRule>> {
    let (query, params_vec): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = match account_id {
        Some(aid) => (
            "SELECT id, account_id, name, enabled, priority, match_type, \
                    conditions_json, actions_json, stop_processing \
             FROM filter_rules \
             WHERE account_id = ?1 OR account_id IS NULL \
             ORDER BY priority DESC"
                .to_string(),
            vec![Box::new(aid.to_string())],
        ),
        None => (
            "SELECT id, account_id, name, enabled, priority, match_type, \
                    conditions_json, actions_json, stop_processing \
             FROM filter_rules \
             ORDER BY priority DESC"
                .to_string(),
            vec![],
        ),
    };

    let mut stmt = conn.prepare(&query)?;

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|p| p.as_ref()).collect();

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            let id: String = row.get(0)?;
            let account_id: Option<String> = row.get(1)?;
            let name: String = row.get(2)?;
            let enabled: bool = row.get(3)?;
            let priority: i32 = row.get(4)?;
            let match_type_str: String = row.get(5)?;
            let conditions_json: String = row.get(6)?;
            let actions_json: String = row.get(7)?;
            let stop_processing: bool = row.get(8)?;

            Ok((
                id,
                account_id,
                name,
                enabled,
                priority,
                match_type_str,
                conditions_json,
                actions_json,
                stop_processing,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut rules = Vec::with_capacity(rows.len());
    for (id, account_id, name, enabled, priority, match_type_str, cond_json, act_json, stop) in rows
    {
        let match_type = match match_type_str.as_str() {
            "any" => MatchType::Any,
            _ => MatchType::All,
        };
        let conditions: Vec<Condition> =
            serde_json::from_str(&cond_json).unwrap_or_default();
        let actions: Vec<FilterAction> =
            serde_json::from_str(&act_json).unwrap_or_default();

        rules.push(FilterRule {
            id,
            account_id,
            name,
            enabled,
            priority,
            match_type,
            conditions,
            actions,
            stop_processing: stop,
        });
    }

    Ok(rules)
}

/// Get a single filter rule by id.
pub fn get_filter(conn: &Connection, id: &str) -> Result<FilterRule> {
    let row = conn
        .query_row(
            "SELECT id, account_id, name, enabled, priority, match_type, \
                    conditions_json, actions_json, stop_processing \
             FROM filter_rules WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, i32>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, bool>(8)?,
                ))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                Error::Other(format!("Filter rule not found: {}", id))
            }
            other => Error::Database(other),
        })?;

    let (id, account_id, name, enabled, priority, match_type_str, cond_json, act_json, stop) = row;

    let match_type = match match_type_str.as_str() {
        "any" => MatchType::Any,
        _ => MatchType::All,
    };
    let conditions: Vec<Condition> =
        serde_json::from_str(&cond_json).unwrap_or_default();
    let actions: Vec<FilterAction> =
        serde_json::from_str(&act_json).unwrap_or_default();

    Ok(FilterRule {
        id,
        account_id,
        name,
        enabled,
        priority,
        match_type,
        conditions,
        actions,
        stop_processing: stop,
    })
}

/// Insert a new filter rule.
pub fn insert_filter(conn: &Connection, rule: &FilterRule) -> Result<()> {
    let match_type_str = match rule.match_type {
        MatchType::All => "all",
        MatchType::Any => "any",
    };
    let conditions_json =
        serde_json::to_string(&rule.conditions).map_err(|e| Error::Other(e.to_string()))?;
    let actions_json =
        serde_json::to_string(&rule.actions).map_err(|e| Error::Other(e.to_string()))?;

    conn.execute(
        "INSERT INTO filter_rules \
         (id, account_id, name, enabled, priority, match_type, conditions_json, actions_json, stop_processing) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            rule.id,
            rule.account_id,
            rule.name,
            rule.enabled,
            rule.priority,
            match_type_str,
            conditions_json,
            actions_json,
            rule.stop_processing,
        ],
    )?;
    log::info!("Inserted filter rule: id={} name='{}'", rule.id, rule.name);
    Ok(())
}

/// Update an existing filter rule.
pub fn update_filter(conn: &Connection, rule: &FilterRule) -> Result<()> {
    let match_type_str = match rule.match_type {
        MatchType::All => "all",
        MatchType::Any => "any",
    };
    let conditions_json =
        serde_json::to_string(&rule.conditions).map_err(|e| Error::Other(e.to_string()))?;
    let actions_json =
        serde_json::to_string(&rule.actions).map_err(|e| Error::Other(e.to_string()))?;

    let rows = conn.execute(
        "UPDATE filter_rules \
         SET account_id = ?1, name = ?2, enabled = ?3, priority = ?4, \
             match_type = ?5, conditions_json = ?6, actions_json = ?7, stop_processing = ?8 \
         WHERE id = ?9",
        params![
            rule.account_id,
            rule.name,
            rule.enabled,
            rule.priority,
            match_type_str,
            conditions_json,
            actions_json,
            rule.stop_processing,
            rule.id,
        ],
    )?;

    if rows == 0 {
        return Err(Error::Other(format!(
            "Filter rule not found for update: {}",
            rule.id
        )));
    }

    log::info!("Updated filter rule: id={} name='{}'", rule.id, rule.name);
    Ok(())
}

/// Delete a filter rule by id.
pub fn delete_filter(conn: &Connection, id: &str) -> Result<()> {
    let rows = conn.execute("DELETE FROM filter_rules WHERE id = ?1", params![id])?;
    if rows == 0 {
        return Err(Error::Other(format!(
            "Filter rule not found for deletion: {}",
            id
        )));
    }
    log::info!("Deleted filter rule: id={}", id);
    Ok(())
}
