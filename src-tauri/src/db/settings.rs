use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::Result;

// `app_metadata` is a generic key/value table (see schema.rs); these are
// the two keys used to remember what the user was looking at (#191).
const KEY_LAST_ACCOUNT_ID: &str = "last_open_account_id";
const KEY_LAST_FOLDER_PATH: &str = "last_open_folder_path";

/// The account/folder the user was viewing when the app last closed (or
/// last navigated to), used to restore that view on the next startup.
/// Either field is `None` on a fresh install or before the first
/// navigation is persisted.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LastView {
    pub account_id: Option<String>,
    pub folder_path: Option<String>,
}

fn get_metadata(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM app_metadata WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Read the last-viewed account/folder persisted by `save_last_view`.
pub fn get_last_view(conn: &Connection) -> Result<LastView> {
    Ok(LastView {
        account_id: get_metadata(conn, KEY_LAST_ACCOUNT_ID)?,
        folder_path: get_metadata(conn, KEY_LAST_FOLDER_PATH)?,
    })
}

/// Persist the account/folder the user is currently viewing, so the next
/// startup can restore it (issue #191). Called (debounced) by the
/// frontend on every folder/account navigation.
pub fn save_last_view(conn: &Connection, account_id: &str, folder_path: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO app_metadata (key, value) VALUES (?1, ?2)",
        params![KEY_LAST_ACCOUNT_ID, account_id],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO app_metadata (key, value) VALUES (?1, ?2)",
        params![KEY_LAST_FOLDER_PATH, folder_path],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn get_last_view_defaults_to_none_on_fresh_db() {
        let conn = setup_db();
        let view = get_last_view(&conn).unwrap();
        assert_eq!(view.account_id, None);
        assert_eq!(view.folder_path, None);
    }

    #[test]
    fn save_and_get_last_view_round_trips() {
        let conn = setup_db();
        save_last_view(&conn, "acc-1", "INBOX").unwrap();
        let view = get_last_view(&conn).unwrap();
        assert_eq!(view.account_id.as_deref(), Some("acc-1"));
        assert_eq!(view.folder_path.as_deref(), Some("INBOX"));
    }

    #[test]
    fn save_last_view_overwrites_previous_value() {
        let conn = setup_db();
        save_last_view(&conn, "acc-1", "INBOX").unwrap();
        save_last_view(&conn, "acc-2", "Archive").unwrap();
        let view = get_last_view(&conn).unwrap();
        assert_eq!(view.account_id.as_deref(), Some("acc-2"));
        assert_eq!(view.folder_path.as_deref(), Some("Archive"));
    }
}
