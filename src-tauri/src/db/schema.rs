use rusqlite::Connection;

use crate::error::Result;

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA foreign_keys=ON;

        CREATE TABLE IF NOT EXISTS accounts (
            id TEXT PRIMARY KEY,
            display_name TEXT NOT NULL,
            email TEXT NOT NULL,
            provider TEXT NOT NULL,
            mail_protocol TEXT NOT NULL DEFAULT 'imap',
            imap_host TEXT NOT NULL DEFAULT '',
            imap_port INTEGER NOT NULL DEFAULT 993,
            smtp_host TEXT NOT NULL DEFAULT '',
            smtp_port INTEGER NOT NULL DEFAULT 587,
            jmap_url TEXT NOT NULL DEFAULT '',
            username TEXT NOT NULL,
            password TEXT NOT NULL,
            use_tls INTEGER NOT NULL DEFAULT 1,
            enabled INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS folders (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            path TEXT NOT NULL,
            folder_type TEXT,
            uidvalidity INTEGER,
            last_seen_uid INTEGER DEFAULT 0,
            jmap_state TEXT,
            unread_count INTEGER DEFAULT 0,
            total_count INTEGER DEFAULT 0,
            UNIQUE(account_id, path)
        );
        CREATE INDEX IF NOT EXISTS idx_folders_account ON folders(account_id);

        CREATE TABLE IF NOT EXISTS messages (
            id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            folder_path TEXT NOT NULL,
            uid INTEGER,
            message_id TEXT,
            in_reply_to TEXT,
            thread_id TEXT,
            subject TEXT,
            from_name TEXT,
            from_email TEXT,
            to_addresses TEXT,
            cc_addresses TEXT,
            date TEXT NOT NULL,
            size INTEGER,
            has_attachments INTEGER DEFAULT 0,
            is_encrypted INTEGER DEFAULT 0,
            is_signed INTEGER DEFAULT 0,
            flags TEXT DEFAULT '[]',
            maildir_path TEXT,
            snippet TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_msg_folder ON messages(account_id, folder_path);
        CREATE INDEX IF NOT EXISTS idx_msg_date ON messages(date);
        CREATE INDEX IF NOT EXISTS idx_msg_thread ON messages(thread_id);
        CREATE INDEX IF NOT EXISTS idx_msg_message_id ON messages(message_id);

        CREATE TABLE IF NOT EXISTS calendar_events (
            id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            calendar_id TEXT NOT NULL,
            uid TEXT,
            title TEXT NOT NULL,
            description TEXT,
            location TEXT,
            start_time TEXT NOT NULL,
            end_time TEXT NOT NULL,
            all_day INTEGER DEFAULT 0,
            timezone TEXT,
            recurrence_rule TEXT,
            organizer_email TEXT,
            attendees_json TEXT,
            my_status TEXT,
            source_message_id TEXT,
            ical_data TEXT,
            remote_id TEXT,
            etag TEXT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_events_time ON calendar_events(start_time, end_time);

        CREATE TABLE IF NOT EXISTS calendars (
            id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            color TEXT DEFAULT '#4285f4',
            is_default INTEGER DEFAULT 0,
            remote_id TEXT,
            UNIQUE(account_id, remote_id)
        );

        CREATE TABLE IF NOT EXISTS filter_rules (
            id TEXT PRIMARY KEY,
            account_id TEXT REFERENCES accounts(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            enabled INTEGER DEFAULT 1,
            priority INTEGER DEFAULT 0,
            match_type TEXT NOT NULL,
            conditions_json TEXT NOT NULL,
            actions_json TEXT NOT NULL,
            stop_processing INTEGER DEFAULT 1,
            apply_to_existing INTEGER DEFAULT 0,
            created_at TEXT DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS outbox (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            action_type TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            status TEXT DEFAULT 'pending',
            retry_count INTEGER DEFAULT 0,
            error_message TEXT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_outbox_status ON outbox(status);

        CREATE TABLE IF NOT EXISTS app_metadata (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        ",
    )?;

    // Migrations for existing databases
    run_migrations(conn)?;

    Ok(())
}

fn run_migrations(conn: &Connection) -> Result<()> {
    // Add jmap_url column if it doesn't exist (added in JMAP support)
    let has_jmap_url: bool = conn
        .prepare("SELECT jmap_url FROM accounts LIMIT 0")
        .is_ok();
    if !has_jmap_url {
        log::info!("Migration: adding jmap_url column to accounts table");
        conn.execute_batch("ALTER TABLE accounts ADD COLUMN jmap_url TEXT NOT NULL DEFAULT '';")?;
    }

    // Add caldav_url column if it doesn't exist (added in CalDAV support)
    let has_caldav_url: bool = conn
        .prepare("SELECT caldav_url FROM accounts LIMIT 0")
        .is_ok();
    if !has_caldav_url {
        log::info!("Migration: adding caldav_url column to accounts table");
        conn.execute_batch(
            "ALTER TABLE accounts ADD COLUMN caldav_url TEXT NOT NULL DEFAULT '';",
        )?;
    }

    Ok(())
}

/// Check if a one-time migration has been completed.
pub fn has_migration(conn: &Connection, key: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM app_metadata WHERE key = ?1",
        rusqlite::params![key],
        |_| Ok(()),
    )
    .is_ok()
}

/// Mark a one-time migration as completed.
pub fn set_migration(conn: &Connection, key: &str) -> crate::error::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO app_metadata (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(())
}
