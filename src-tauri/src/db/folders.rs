use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::Result;

#[derive(Debug, Clone, Serialize)]
pub struct Folder {
    pub name: String,
    pub path: String,
    pub folder_type: Option<String>,
    pub unread_count: i64,
    pub total_count: i64,
    pub children: Vec<Folder>,
    #[serde(skip_serializing)]
    pub parent_id: Option<String>,
}

pub fn upsert_folder(
    conn: &Connection,
    account_id: &str,
    name: &str,
    path: &str,
    folder_type: Option<&str>,
    parent_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO folders (account_id, name, path, folder_type, parent_id)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(account_id, path) DO UPDATE SET name = ?2, folder_type = ?4, parent_id = ?5",
        params![account_id, name, path, folder_type, parent_id],
    )?;
    Ok(())
}

pub fn list_folders(conn: &Connection, account_id: &str) -> Result<Vec<Folder>> {
    let mut stmt = conn.prepare(
        "SELECT name, path, folder_type, unread_count, total_count, parent_id
         FROM folders WHERE account_id = ?1 ORDER BY
         CASE folder_type
           WHEN 'inbox' THEN 0
           WHEN 'drafts' THEN 1
           WHEN 'sent' THEN 2
           WHEN 'junk' THEN 3
           WHEN 'trash' THEN 4
           WHEN 'archive' THEN 5
           ELSE 6
         END, name",
    )?;
    let folders = stmt
        .query_map(params![account_id], |row| {
            Ok(Folder {
                name: row.get(0)?,
                path: row.get(1)?,
                folder_type: row.get(2)?,
                unread_count: row.get(3)?,
                total_count: row.get(4)?,
                children: vec![],
                parent_id: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(folders)
}

/// Build a nested folder tree from a flat list of folders.
/// IMAP folders use `/` in their path to denote hierarchy (e.g., "INBOX/IETF/123").
/// JMAP folders use `parent_id` to reference the parent mailbox's path/id.
/// Top-level folders (no parent, or parent not in list) become roots.
pub fn build_folder_tree(mut folders: Vec<Folder>) -> Vec<Folder> {
    use std::collections::HashMap;

    let has_parent_ids = folders.iter().any(|f| f.parent_id.is_some());

    let mut by_path: HashMap<String, usize> = HashMap::new();
    for (i, f) in folders.iter().enumerate() {
        by_path.insert(f.path.clone(), i);
    }

    let mut children_map: HashMap<String, Vec<usize>> = HashMap::new();
    let mut is_child = vec![false; folders.len()];

    for i in 0..folders.len() {
        let parent_path = if has_parent_ids {
            // JMAP: parent_id references another mailbox's path/id
            folders[i].parent_id.clone()
        } else {
            // IMAP: derive parent from path hierarchy
            folders[i].path.rsplit_once('/').map(|(p, _)| p.to_string())
        };

        if let Some(pp) = parent_path {
            if by_path.contains_key(&pp) {
                children_map.entry(pp).or_default().push(i);
                is_child[i] = true;
            }
        }
    }

    // Process deepest parents first so children are fully built before attaching.
    // For JMAP (parent_id), compute depth by counting hops to root.
    // For IMAP (path-based), use '/' count as depth proxy.
    let mut parent_paths: Vec<String> = children_map.keys().cloned().collect();
    if has_parent_ids {
        // Compute depth for each folder by following parent_id chain.
        // Guard against cycles with a visited set.
        let depth_of = |path: &str| -> usize {
            let mut depth = 0usize;
            let mut current = path.to_string();
            let mut visited = std::collections::HashSet::new();
            while let Some(idx) = by_path.get(&current) {
                if !visited.insert(current.clone()) {
                    log::warn!("Cycle detected in folder parent_id chain at {}", current);
                    break;
                }
                if let Some(ref pid) = folders[*idx].parent_id {
                    depth += 1;
                    current = pid.clone();
                } else {
                    break;
                }
            }
            depth
        };
        parent_paths.sort_by_key(|path| std::cmp::Reverse(depth_of(path)));
    } else {
        parent_paths.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
    }

    for parent_path in parent_paths {
        if let Some(child_indices) = children_map.remove(&parent_path) {
            let children: Vec<Folder> = child_indices.iter()
                .map(|&i| folders[i].clone())
                .collect();
            if let Some(&parent_idx) = by_path.get(&parent_path) {
                folders[parent_idx].children = children;
            }
        }
    }

    folders.into_iter()
        .enumerate()
        .filter(|(i, _)| !is_child[*i])
        .map(|(_, f)| f)
        .collect()
}

pub fn delete_folder(conn: &Connection, account_id: &str, path: &str) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;

    let delete_result = (|| -> Result<()> {
        conn.execute(
            "WITH RECURSIVE folder_tree(path) AS (
                SELECT path FROM folders WHERE account_id = ?1 AND path = ?2
                UNION
                SELECT f.path
                FROM folders f
                JOIN folder_tree ft
                  ON f.account_id = ?1
                 AND (
                     f.parent_id = ft.path
                     OR (
                         f.parent_id IS NULL
                         AND substr(f.path, 1, length(ft.path) + 1) = ft.path || '/'
                     )
                 )
            )
            DELETE FROM messages
             WHERE account_id = ?1
               AND folder_path IN (SELECT path FROM folder_tree)",
            params![account_id, path],
        )?;

        conn.execute(
            "WITH RECURSIVE folder_tree(path) AS (
                SELECT path FROM folders WHERE account_id = ?1 AND path = ?2
                UNION
                SELECT f.path
                FROM folders f
                JOIN folder_tree ft
                  ON f.account_id = ?1
                 AND (
                     f.parent_id = ft.path
                     OR (
                         f.parent_id IS NULL
                         AND substr(f.path, 1, length(ft.path) + 1) = ft.path || '/'
                     )
                 )
            )
            DELETE FROM folders
             WHERE account_id = ?1
               AND path IN (SELECT path FROM folder_tree)",
            params![account_id, path],
        )?;

        Ok(())
    })();

    match delete_result {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(err);
        }
    }

    log::info!("Deleted folder '{}' from local DB for account {}", path, account_id);
    Ok(())
}

pub fn update_folder_counts(
    conn: &Connection,
    account_id: &str,
    path: &str,
    unread: i64,
    total: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE folders SET unread_count = ?1, total_count = ?2
         WHERE account_id = ?3 AND path = ?4",
        params![unread, total, account_id, path],
    )?;
    Ok(())
}

pub fn update_last_seen_uid(
    conn: &Connection,
    account_id: &str,
    path: &str,
    uid: u32,
) -> Result<()> {
    conn.execute(
        "UPDATE folders SET last_seen_uid = ?1 WHERE account_id = ?2 AND path = ?3",
        params![uid, account_id, path],
    )?;
    Ok(())
}

/// Get the stored sync state (uid_next, total_count) for preflight checks.
pub fn get_folder_sync_state(
    conn: &Connection,
    account_id: &str,
    path: &str,
) -> Result<(u32, i64)> {
    let result = conn.query_row(
        "SELECT uid_next, total_count FROM folders WHERE account_id = ?1 AND path = ?2",
        params![account_id, path],
        |row| Ok((row.get::<_, u32>(0).unwrap_or(0), row.get::<_, i64>(1).unwrap_or(0))),
    );
    Ok(result.unwrap_or((0, 0)))
}

/// Update the stored uid_next after a successful folder sync.
pub fn update_uid_next(
    conn: &Connection,
    account_id: &str,
    path: &str,
    uid_next: u32,
) -> Result<()> {
    conn.execute(
        "UPDATE folders SET uid_next = ?1 WHERE account_id = ?2 AND path = ?3",
        params![uid_next, account_id, path],
    )?;
    Ok(())
}

pub fn get_last_seen_uid(conn: &Connection, account_id: &str, path: &str) -> Result<u32> {
    let uid: u32 = conn
        .query_row(
            "SELECT last_seen_uid FROM folders WHERE account_id = ?1 AND path = ?2",
            params![account_id, path],
            |row| row.get(0),
        )
        .unwrap_or(0);
    Ok(uid)
}

pub fn get_jmap_state(conn: &Connection, account_id: &str, path: &str) -> Result<Option<String>> {
    let state: Option<String> = conn
        .query_row(
            "SELECT jmap_state FROM folders WHERE account_id = ?1 AND path = ?2",
            params![account_id, path],
            |row| row.get(0),
        )
        .unwrap_or(None);
    Ok(state)
}

pub fn update_jmap_state(
    conn: &Connection,
    account_id: &str,
    path: &str,
    state: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE folders SET jmap_state = ?1 WHERE account_id = ?2 AND path = ?3",
        params![state, account_id, path],
    )?;
    Ok(())
}

/// Recalculate folder counts from the messages table for all folders of an account.
pub fn recalculate_folder_counts(conn: &Connection, account_id: &str) -> Result<()> {
    log::debug!("Recalculating folder counts for account {}", account_id);
    conn.execute(
        "UPDATE folders SET
            total_count = (
                SELECT COUNT(*) FROM messages
                WHERE messages.account_id = folders.account_id
                  AND messages.folder_path = folders.path
            ),
            unread_count = (
                SELECT COUNT(*) FROM messages
                WHERE messages.account_id = folders.account_id
                  AND messages.folder_path = folders.path
                  AND messages.flags NOT LIKE '%seen%'
            )
         WHERE account_id = ?1",
        params![account_id],
    )?;
    Ok(())
}

/// Guess folder type from name for common IMAP folder names
pub fn guess_folder_type(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    // Gmail uses [Gmail]/... prefixed names
    let normalized = lower
        .trim_start_matches("[gmail]/")
        .trim_start_matches("[google mail]/");
    match normalized {
        "inbox" => Some("inbox"),
        "sent" | "sent mail" | "sent messages" | "sent items" => Some("sent"),
        "drafts" | "draft" => Some("drafts"),
        "trash" | "deleted" | "deleted messages" | "deleted items" | "bin" => Some("trash"),
        "junk" | "spam" | "bulk mail" => Some("junk"),
        "archive" | "all mail" | "all" => Some("archive"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_folder(name: &str, path: &str, folder_type: Option<&str>) -> Folder {
        Folder {
            name: name.to_string(),
            path: path.to_string(),
            folder_type: folder_type.map(|s| s.to_string()),
            unread_count: 0,
            total_count: 0,
            children: vec![],
            parent_id: None,
        }
    }

    #[test]
    fn test_flat_folders_stay_flat() {
        let folders = vec![
            make_folder("Inbox", "INBOX", Some("inbox")),
            make_folder("Sent", "Sent", Some("sent")),
            make_folder("Drafts", "Drafts", Some("drafts")),
        ];
        let tree = build_folder_tree(folders);
        assert_eq!(tree.len(), 3);
        assert!(tree.iter().all(|f| f.children.is_empty()));
    }

    #[test]
    fn test_one_level_nesting() {
        let folders = vec![
            make_folder("Inbox", "INBOX", Some("inbox")),
            make_folder("IETF", "INBOX/IETF", None),
            make_folder("Infra", "INBOX/Infra", None),
        ];
        let tree = build_folder_tree(folders);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].children.len(), 2);
    }

    #[test]
    fn test_deep_nesting() {
        let folders = vec![
            make_folder("Inbox", "INBOX", Some("inbox")),
            make_folder("IETF", "INBOX/IETF", None),
            make_folder("123", "INBOX/IETF/123", None),
            make_folder("456", "INBOX/IETF/456", None),
        ];
        let tree = build_folder_tree(folders);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].children.len(), 2);
    }

    #[test]
    fn test_orphan_stays_at_root() {
        let folders = vec![
            make_folder("123", "INBOX/IETF/123", None),
            make_folder("Sent", "Sent", Some("sent")),
        ];
        let tree = build_folder_tree(folders);
        assert_eq!(tree.len(), 2);
    }

    #[test]
    fn test_jmap_parent_id_nesting() {
        let folders = vec![
            Folder {
                name: "Inbox".to_string(),
                path: "m1".to_string(),
                folder_type: Some("inbox".to_string()),
                unread_count: 3,
                total_count: 10,
                children: vec![],
                parent_id: None,
            },
            Folder {
                name: "IETF".to_string(),
                path: "m2".to_string(),
                folder_type: None,
                unread_count: 0,
                total_count: 5,
                children: vec![],
                parent_id: Some("m1".to_string()),
            },
            Folder {
                name: "123".to_string(),
                path: "m3".to_string(),
                folder_type: None,
                unread_count: 0,
                total_count: 2,
                children: vec![],
                parent_id: Some("m2".to_string()),
            },
            Folder {
                name: "Sent".to_string(),
                path: "m4".to_string(),
                folder_type: Some("sent".to_string()),
                unread_count: 0,
                total_count: 0,
                children: vec![],
                parent_id: None,
            },
        ];
        let tree = build_folder_tree(folders);
        assert_eq!(tree.len(), 2); // Inbox, Sent

        let inbox = tree.iter().find(|f| f.path == "m1").unwrap();
        assert_eq!(inbox.children.len(), 1); // IETF
        assert_eq!(inbox.children[0].children.len(), 1); // 123
    }

    #[test]
    fn test_mixed_hierarchy() {
        let folders = vec![
            make_folder("Inbox", "INBOX", Some("inbox")),
            make_folder("IETF", "INBOX/IETF", None),
            make_folder("123", "INBOX/IETF/123", None),
            make_folder("Infra", "INBOX/Infra", None),
            make_folder("Mastodon", "INBOX/Infra/Mastodon", None),
            make_folder("Sent", "Sent", Some("sent")),
            make_folder("Drafts", "Drafts", Some("drafts")),
        ];
        let tree = build_folder_tree(folders);
        assert_eq!(tree.len(), 3);

        let inbox = tree.iter().find(|f| f.path == "INBOX").unwrap();
        assert_eq!(inbox.children.len(), 2);

        let ietf = inbox.children.iter().find(|f| f.path == "INBOX/IETF").unwrap();
        assert_eq!(ietf.children.len(), 1);

        let infra = inbox.children.iter().find(|f| f.path == "INBOX/Infra").unwrap();
        assert_eq!(infra.children.len(), 1);
    }

    fn create_delete_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE folders (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id TEXT NOT NULL,
                name TEXT NOT NULL,
                path TEXT NOT NULL,
                folder_type TEXT,
                unread_count INTEGER DEFAULT 0,
                total_count INTEGER DEFAULT 0,
                parent_id TEXT,
                UNIQUE(account_id, path)
            );
            CREATE TABLE messages (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                folder_path TEXT NOT NULL
            );
            ",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_delete_folder_removes_imap_descendants_and_messages() {
        let conn = create_delete_test_db();
        conn.execute(
            "INSERT INTO folders (account_id, name, path, parent_id) VALUES (?1, ?2, ?3, ?4)",
            params!["acc1", "Inbox", "INBOX", Option::<String>::None],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (account_id, name, path, parent_id) VALUES (?1, ?2, ?3, ?4)",
            params!["acc1", "IETF", "INBOX/IETF", Option::<String>::None],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (account_id, name, path, parent_id) VALUES (?1, ?2, ?3, ?4)",
            params!["acc1", "123", "INBOX/IETF/123", Option::<String>::None],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (account_id, name, path, parent_id) VALUES (?1, ?2, ?3, ?4)",
            params!["acc1", "Sent", "Sent", Option::<String>::None],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, folder_path) VALUES (?1, ?2, ?3)",
            params!["m1", "acc1", "INBOX"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, folder_path) VALUES (?1, ?2, ?3)",
            params!["m2", "acc1", "INBOX/IETF"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, folder_path) VALUES (?1, ?2, ?3)",
            params!["m3", "acc1", "INBOX/IETF/123"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, folder_path) VALUES (?1, ?2, ?3)",
            params!["m4", "acc1", "Sent"],
        )
        .unwrap();

        delete_folder(&conn, "acc1", "INBOX").unwrap();

        let remaining_folders: Vec<String> = conn
            .prepare("SELECT path FROM folders WHERE account_id = ?1 ORDER BY path")
            .unwrap()
            .query_map(params!["acc1"], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let remaining_messages: Vec<String> = conn
            .prepare("SELECT id FROM messages WHERE account_id = ?1 ORDER BY id")
            .unwrap()
            .query_map(params!["acc1"], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(remaining_folders, vec!["Sent"]);
        assert_eq!(remaining_messages, vec!["m4"]);
    }

    #[test]
    fn test_delete_folder_removes_parent_id_descendants_and_messages() {
        let conn = create_delete_test_db();
        conn.execute(
            "INSERT INTO folders (account_id, name, path, parent_id) VALUES (?1, ?2, ?3, ?4)",
            params!["acc1", "Inbox", "m1", Option::<String>::None],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (account_id, name, path, parent_id) VALUES (?1, ?2, ?3, ?4)",
            params!["acc1", "IETF", "m2", Some("m1")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (account_id, name, path, parent_id) VALUES (?1, ?2, ?3, ?4)",
            params!["acc1", "123", "m3", Some("m2")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (account_id, name, path, parent_id) VALUES (?1, ?2, ?3, ?4)",
            params!["acc1", "Sent", "m4", Option::<String>::None],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, folder_path) VALUES (?1, ?2, ?3)",
            params!["m1-msg", "acc1", "m1"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, folder_path) VALUES (?1, ?2, ?3)",
            params!["m2-msg", "acc1", "m2"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, folder_path) VALUES (?1, ?2, ?3)",
            params!["m3-msg", "acc1", "m3"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, folder_path) VALUES (?1, ?2, ?3)",
            params!["m4-msg", "acc1", "m4"],
        )
        .unwrap();

        delete_folder(&conn, "acc1", "m2").unwrap();

        let remaining_folders: Vec<String> = conn
            .prepare("SELECT path FROM folders WHERE account_id = ?1 ORDER BY path")
            .unwrap()
            .query_map(params!["acc1"], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let remaining_messages: Vec<String> = conn
            .prepare("SELECT id FROM messages WHERE account_id = ?1 ORDER BY id")
            .unwrap()
            .query_map(params!["acc1"], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(remaining_folders, vec!["m1", "m4"]);
        assert_eq!(remaining_messages, vec!["m1-msg", "m4-msg"]);
    }
}
