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

        CREATE TABLE IF NOT EXISTS contact_books (
            id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            remote_id TEXT,
            sync_type TEXT NOT NULL DEFAULT 'local',
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS contacts (
            id TEXT PRIMARY KEY,
            book_id TEXT NOT NULL REFERENCES contact_books(id) ON DELETE CASCADE,
            uid TEXT,
            display_name TEXT NOT NULL,
            emails_json TEXT DEFAULT '[]',
            phones_json TEXT DEFAULT '[]',
            addresses_json TEXT DEFAULT '[]',
            organization TEXT,
            title TEXT,
            notes TEXT,
            vcard_data TEXT,
            remote_id TEXT,
            etag TEXT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_contacts_book ON contacts(book_id);
        CREATE INDEX IF NOT EXISTS idx_contacts_name ON contacts(display_name);

        CREATE TABLE IF NOT EXISTS collected_contacts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            email TEXT NOT NULL,
            name TEXT,
            last_used TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            use_count INTEGER NOT NULL DEFAULT 1,
            UNIQUE(account_id, email)
        );
        CREATE INDEX IF NOT EXISTS idx_collected_email ON collected_contacts(email);

        CREATE TABLE IF NOT EXISTS app_metadata (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        -- FTS5 virtual table for fast message text search (quick filter)
        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            subject,
            from_name,
            from_email,
            to_addresses,
            cc_addresses,
            snippet,
            content=messages,
            content_rowid=rowid
        );

        -- Triggers to keep FTS index in sync with messages table
        CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages BEGIN
            INSERT INTO messages_fts(rowid, subject, from_name, from_email, to_addresses, cc_addresses, snippet)
            VALUES (new.rowid, new.subject, new.from_name, new.from_email, new.to_addresses, new.cc_addresses, new.snippet);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_email, to_addresses, cc_addresses, snippet)
            VALUES ('delete', old.rowid, old.subject, old.from_name, old.from_email, old.to_addresses, old.cc_addresses, old.snippet);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_fts_update AFTER UPDATE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, from_email, to_addresses, cc_addresses, snippet)
            VALUES ('delete', old.rowid, old.subject, old.from_name, old.from_email, old.to_addresses, old.cc_addresses, old.snippet);
            INSERT INTO messages_fts(rowid, subject, from_name, from_email, to_addresses, cc_addresses, snippet)
            VALUES (new.rowid, new.subject, new.from_name, new.from_email, new.to_addresses, new.cc_addresses, new.snippet);
        END;
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

    // Add signature column if it doesn't exist
    let has_signature: bool = conn
        .prepare("SELECT signature FROM accounts LIMIT 0")
        .is_ok();
    if !has_signature {
        log::info!("Migration: adding signature column to accounts table");
        conn.execute_batch(
            "ALTER TABLE accounts ADD COLUMN signature TEXT NOT NULL DEFAULT '';",
        )?;
    }

    // Add jmap_auth_method column if it doesn't exist
    let has_jmap_auth_method: bool = conn
        .prepare("SELECT jmap_auth_method FROM accounts LIMIT 0")
        .is_ok();
    if !has_jmap_auth_method {
        log::info!("Migration: adding jmap_auth_method column to accounts table");
        conn.execute_batch(
            "ALTER TABLE accounts ADD COLUMN jmap_auth_method TEXT NOT NULL DEFAULT 'basic';",
        )?;
    }

    // Add oidc_token_endpoint column if it doesn't exist
    let has_oidc_token_endpoint: bool = conn
        .prepare("SELECT oidc_token_endpoint FROM accounts LIMIT 0")
        .is_ok();
    if !has_oidc_token_endpoint {
        log::info!("Migration: adding oidc_token_endpoint column to accounts table");
        conn.execute_batch(
            "ALTER TABLE accounts ADD COLUMN oidc_token_endpoint TEXT NOT NULL DEFAULT '';",
        )?;
    }

    // Add oidc_client_id column if it doesn't exist
    let has_oidc_client_id: bool = conn
        .prepare("SELECT oidc_client_id FROM accounts LIMIT 0")
        .is_ok();
    if !has_oidc_client_id {
        log::info!("Migration: adding oidc_client_id column to accounts table");
        conn.execute_batch(
            "ALTER TABLE accounts ADD COLUMN oidc_client_id TEXT NOT NULL DEFAULT '';",
        )?;
    }

    // Populate FTS index for existing messages (one-time migration)
    if !has_migration(conn, "fts5_initial_populate") {
        log::info!("Migration: populating FTS5 index for existing messages");
        conn.execute_batch(
            "INSERT OR IGNORE INTO messages_fts(rowid, subject, from_name, from_email, to_addresses, cc_addresses, snippet)
             SELECT rowid, subject, from_name, from_email, to_addresses, cc_addresses, snippet FROM messages;"
        )?;
        set_migration(conn, "fts5_initial_populate")?;
        log::info!("Migration: FTS5 index populated");
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
