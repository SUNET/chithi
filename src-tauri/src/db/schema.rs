use rusqlite::Connection;

use crate::error::Result;

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA foreign_keys=ON;

        -- Phase 3: identity-only schema. Per-protocol settings (mail
        -- host/port, JMAP url, CalDAV url, etc.) live in service_bindings.
        -- Pre-Phase-3 databases keep their legacy columns until the
        -- service_bindings_drop_legacy_columns migration drops them.
        CREATE TABLE IF NOT EXISTS accounts (
            id TEXT PRIMARY KEY,
            display_name TEXT NOT NULL,
            email TEXT NOT NULL,
            username TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            signature TEXT NOT NULL DEFAULT '',
            auth_method TEXT NOT NULL DEFAULT '',
            oidc_token_endpoint TEXT NOT NULL DEFAULT '',
            oidc_client_id TEXT NOT NULL DEFAULT '',
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
            uid_next INTEGER DEFAULT 0,
            parent_id TEXT,
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

        -- Per-service binding for an account. One identity (accounts row)
        -- can have one mail binding, one calendar binding, and one contacts
        -- binding, each with its own protocol and protocol-specific config.
        -- Phase 1: populated alongside the legacy per-protocol columns on
        -- accounts; nothing reads from here yet. Phases 2/3 migrate the
        -- dispatch reads and drop the legacy columns.
        CREATE TABLE IF NOT EXISTS service_bindings (
            id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            service TEXT NOT NULL,
            protocol TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            sync_interval_seconds INTEGER,
            config_json TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(account_id, service, protocol)
        );
        CREATE INDEX IF NOT EXISTS idx_bindings_account ON service_bindings(account_id);

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
    // Skip the legacy column-add and populate migrations on databases that
    // already finished Phase 3. Without this gate, fresh installs would
    // re-create columns we just dropped (the ADD COLUMN sequence below
    // sees the column missing and tries to add it back).
    let phase3_done = has_migration(conn, "service_bindings_drop_legacy_columns");

    if !phase3_done {
        // Add jmap_url column if it doesn't exist (added in JMAP support)
        let has_jmap_url: bool = conn
            .prepare("SELECT jmap_url FROM accounts LIMIT 0")
            .is_ok();
        if !has_jmap_url {
            log::info!("Migration: adding jmap_url column to accounts table");
            conn.execute_batch(
                "ALTER TABLE accounts ADD COLUMN jmap_url TEXT NOT NULL DEFAULT '';",
            )?;
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
    }

    // Add signature column if it doesn't exist (still part of accounts post-Phase-3).
    let has_signature: bool = conn
        .prepare("SELECT signature FROM accounts LIMIT 0")
        .is_ok();
    if !has_signature {
        log::info!("Migration: adding signature column to accounts table");
        conn.execute_batch("ALTER TABLE accounts ADD COLUMN signature TEXT NOT NULL DEFAULT '';")?;
    }

    if !phase3_done {
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
    }

    // Add oidc_token_endpoint column if it doesn't exist (kept post-Phase-3).
    let has_oidc_token_endpoint: bool = conn
        .prepare("SELECT oidc_token_endpoint FROM accounts LIMIT 0")
        .is_ok();
    if !has_oidc_token_endpoint {
        log::info!("Migration: adding oidc_token_endpoint column to accounts table");
        conn.execute_batch(
            "ALTER TABLE accounts ADD COLUMN oidc_token_endpoint TEXT NOT NULL DEFAULT '';",
        )?;
    }

    // Add oidc_client_id column if it doesn't exist (kept post-Phase-3).
    let has_oidc_client_id: bool = conn
        .prepare("SELECT oidc_client_id FROM accounts LIMIT 0")
        .is_ok();
    if !has_oidc_client_id {
        log::info!("Migration: adding oidc_client_id column to accounts table");
        conn.execute_batch(
            "ALTER TABLE accounts ADD COLUMN oidc_client_id TEXT NOT NULL DEFAULT '';",
        )?;
    }

    // Add is_subscribed column to calendars if it doesn't exist
    let has_is_subscribed: bool = conn
        .prepare("SELECT is_subscribed FROM calendars LIMIT 0")
        .is_ok();
    if !has_is_subscribed {
        log::info!("Migration: adding is_subscribed column to calendars table");
        conn.execute_batch(
            "ALTER TABLE calendars ADD COLUMN is_subscribed INTEGER NOT NULL DEFAULT 1;",
        )?;
    }

    // Add uid_next column to folders for IMAP preflight sync optimization
    let has_uid_next: bool = conn.prepare("SELECT uid_next FROM folders LIMIT 0").is_ok();
    if !has_uid_next {
        log::info!("Migration: adding uid_next column to folders table");
        conn.execute_batch("ALTER TABLE folders ADD COLUMN uid_next INTEGER DEFAULT 0;")?;
    }

    if !phase3_done {
        // Add calendar_sync_enabled column for per-account calendar-sync toggle
        let has_calendar_sync_enabled: bool = conn
            .prepare("SELECT calendar_sync_enabled FROM accounts LIMIT 0")
            .is_ok();
        if !has_calendar_sync_enabled {
            log::info!("Migration: adding calendar_sync_enabled column to accounts table");
            conn.execute_batch(
                "ALTER TABLE accounts ADD COLUMN calendar_sync_enabled INTEGER NOT NULL DEFAULT 1;",
            )?;
        }
    }

    // Add parent_id column to folders. Existing DBs that were populated by
    // older JMAP sync builds already had it; fresh installs didn't because
    // the CREATE TABLE in initialize() was never updated to match. Without
    // this column the first JMAP folder upsert fails with "no column named
    // parent_id".
    let has_folder_parent_id: bool = conn
        .prepare("SELECT parent_id FROM folders LIMIT 0")
        .is_ok();
    if !has_folder_parent_id {
        log::info!("Migration: adding parent_id column to folders table");
        conn.execute_batch("ALTER TABLE folders ADD COLUMN parent_id TEXT;")?;
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

    // Add auth_method column on accounts. Phase 1 of the service-bindings
    // refactor: stored alongside the legacy `provider` column so dispatch
    // code keeps working. Phase 2 starts reading auth_method instead;
    // Phase 3 drops `provider`.
    let has_auth_method: bool = conn
        .prepare("SELECT auth_method FROM accounts LIMIT 0")
        .is_ok();
    if !has_auth_method {
        log::info!("Migration: adding auth_method column to accounts table");
        conn.execute_batch(
            "ALTER TABLE accounts ADD COLUMN auth_method TEXT NOT NULL DEFAULT '';",
        )?;
    }

    // Backfill auth_method for any rows that haven't been populated yet
    // (covers both fresh-from-migration rows above and any older rows that
    // were created before this migration ran). Idempotent. Reads legacy
    // columns, so skip if Phase 3 already dropped them.
    if !has_migration(conn, "auth_method_backfill_v1") {
        if !phase3_done {
            log::info!("Migration: backfilling auth_method from provider/jmap_auth_method");
            backfill_auth_method(conn)?;
        }
        set_migration(conn, "auth_method_backfill_v1")?;
    }

    // One-time populate of service_bindings from legacy account columns.
    // Re-runnable (it deletes existing rows for an account before inserting),
    // but gated by a marker so the common-case startup is a single SELECT.
    if !has_migration(conn, "service_bindings_initial_populate") {
        if !phase3_done {
            log::info!("Migration: deriving service_bindings from existing accounts");
            populate_service_bindings(conn)?;
            log::info!("Migration: service_bindings populated");
        }
        set_migration(conn, "service_bindings_initial_populate")?;
    }

    // Phase 3: drop the legacy per-protocol columns from accounts. Their
    // data has already been mirrored into service_bindings by the populate
    // migration above, so dropping them is safe. SQLite supports
    // ALTER TABLE DROP COLUMN since 3.35 (March 2021). Run as a single
    // batch in a transaction so a partial failure doesn't leave the
    // schema half-dropped.
    if !has_migration(conn, "service_bindings_drop_legacy_columns") {
        log::info!("Migration: dropping legacy per-protocol columns from accounts");
        drop_legacy_account_columns(conn)?;
        set_migration(conn, "service_bindings_drop_legacy_columns")?;
        log::info!("Migration: legacy columns dropped");
    }

    // Canonicalize Message-ID / In-Reply-To and rethread.
    //
    // Older builds stored these strings verbatim from the IMAP envelope,
    // which on some servers (notably Microsoft Exchange/M365) included a
    // leading whitespace inside the bracketed value. Exact-match thread
    // joins (`WHERE message_id = ?`) then silently failed and replies
    // landed in their own one-message threads. Trim+wrap once, then
    // recompute thread_id for every message so existing mail heals
    // without waiting for a fresh full sync.
    if !has_migration(conn, "messageid_normalize_v1") {
        log::info!("Migration: normalizing message_id / in_reply_to and rethreading");
        normalize_message_ids_and_rethread(conn)?;
        set_migration(conn, "messageid_normalize_v1")?;
        log::info!("Migration: message-id normalization complete");
    }

    Ok(())
}

/// One-time backfill: rewrite stored message_id / in_reply_to to their
/// canonical `<core>` form, then propagate ancestor thread_ids down so
/// existing fragmented threads heal. Pure SQL — no per-row Rust loop —
/// so even tens of thousands of messages finish in well under a second
/// on commodity hardware. Wrapped in a single transaction.
fn normalize_message_ids_and_rethread(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;

    // 1) Canonicalize message_id and in_reply_to.
    //
    // SQLite doesn't have a built-in regex, so we use REPLACE chains to
    // strip every `<`, `>`, ASCII space, and tab from the existing value,
    // then re-wrap. The `WHERE` guards skip already-canonical rows so we
    // don't rewrite the entire table on every startup.
    tx.execute_batch(
        "UPDATE messages
         SET message_id = '<' || REPLACE(REPLACE(REPLACE(REPLACE(message_id, '<', ''), '>', ''), ' ', ''), CHAR(9), '') || '>'
         WHERE message_id IS NOT NULL
           AND TRIM(message_id) != ''
           AND message_id != '<' || REPLACE(REPLACE(REPLACE(REPLACE(message_id, '<', ''), '>', ''), ' ', ''), CHAR(9), '') || '>';

         UPDATE messages
         SET in_reply_to = '<' || REPLACE(REPLACE(REPLACE(REPLACE(in_reply_to, '<', ''), '>', ''), ' ', ''), CHAR(9), '') || '>'
         WHERE in_reply_to IS NOT NULL
           AND TRIM(in_reply_to) != ''
           AND in_reply_to != '<' || REPLACE(REPLACE(REPLACE(REPLACE(in_reply_to, '<', ''), '>', ''), ' ', ''), CHAR(9), '') || '>';",
    )?;

    // 2) Propagate parent thread_ids. Each iteration runs one indexed
    // self-join on (account_id, message_id) — the existing
    // `idx_msg_message_id` index makes this a cheap lookup. Each pass
    // pushes thread_ids one generation deeper, so a chain of depth N
    // converges in N-1 passes. Cap at 32 (matching the compose-side
    // chain cap) so a pathological cycle can't spin forever.
    // Gmail label folders mean the same Message-ID can sit in several
    // `messages` rows for one account. The scalar subquery in SET would
    // then have multiple candidates and SQLite picks one non-deterministically;
    // pin it down with `ORDER BY thread_id LIMIT 1` so the migration is
    // reproducible (and so a future SQLite that tightens scalar-subquery
    // semantics doesn't error out at startup).
    for _ in 0..32 {
        let changed = tx.execute(
            "UPDATE messages
             SET thread_id = (
                 SELECT parent.thread_id FROM messages AS parent
                 WHERE parent.account_id = messages.account_id
                   AND parent.message_id = messages.in_reply_to
                   AND parent.thread_id IS NOT NULL
                   AND parent.thread_id != ''
                 ORDER BY parent.thread_id
                 LIMIT 1
             )
             WHERE in_reply_to IS NOT NULL
               AND in_reply_to != ''
               AND EXISTS (
                 SELECT 1 FROM messages AS parent
                 WHERE parent.account_id = messages.account_id
                   AND parent.message_id = messages.in_reply_to
                   AND parent.thread_id IS NOT NULL
                   AND parent.thread_id != ''
                   AND parent.thread_id IS NOT messages.thread_id
               )",
            [],
        )?;
        if changed == 0 {
            break;
        }
    }

    tx.commit()?;
    log::info!("Migration messageid_normalize_v1: canonicalized + rethreaded");
    Ok(())
}

/// Drop every legacy per-protocol column from `accounts`. Each column is
/// dropped only if it actually exists, so the function is safe to run
/// against partially-migrated databases or fresh installs (where the
/// columns might be present from the legacy ADD COLUMN migrations
/// running before this gate flipped).
fn drop_legacy_account_columns(conn: &Connection) -> Result<()> {
    const LEGACY_COLUMNS: &[&str] = &[
        "provider",
        "mail_protocol",
        "imap_host",
        "imap_port",
        "smtp_host",
        "smtp_port",
        "use_tls",
        "jmap_url",
        "jmap_auth_method",
        "caldav_url",
        "calendar_sync_enabled",
    ];

    let tx = conn.unchecked_transaction()?;
    for col in LEGACY_COLUMNS {
        let exists = tx
            .prepare(&format!("SELECT {col} FROM accounts LIMIT 0"))
            .is_ok();
        if exists {
            tx.execute_batch(&format!("ALTER TABLE accounts DROP COLUMN {col};"))?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Backfill `accounts.auth_method` from the legacy (`provider`,
/// `jmap_auth_method`) pair. Single UPDATE that only touches rows whose
/// `auth_method` is still empty, so re-running is harmless.
fn backfill_auth_method(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "UPDATE accounts
         SET auth_method = CASE
             WHEN provider = 'gmail' THEN 'oauth-google'
             WHEN provider = 'o365'  THEN 'oauth-microsoft'
             WHEN jmap_auth_method = 'oidc' THEN 'oauth-jmap-oidc'
             ELSE 'password'
         END
         WHERE auth_method IS NULL OR auth_method = '';",
    )?;
    Ok(())
}

/// Read every existing account, derive its bindings, and INSERT them.
/// Pulls legacy columns directly via SQL (intentionally bypassing
/// get_account_full so the keyring is never touched at startup).
fn populate_service_bindings(conn: &Connection) -> Result<()> {
    use crate::db::service_bindings::{rebuild_for_account, LegacyBindingFields};

    // Fail-soft if the legacy columns have already been dropped (i.e.
    // a fresh-install DB created with the Phase 3 schema). The migration
    // marker logic below normally prevents this branch from running, but
    // an out-of-order replay shouldn't error out the app.
    if conn
        .prepare("SELECT mail_protocol FROM accounts LIMIT 0")
        .is_err()
    {
        log::info!("populate_service_bindings: legacy columns absent, skipping (Phase 3+ schema)");
        return Ok(());
    }

    /// Tuple representation of the legacy column row. Named only to keep
    /// clippy happy about the long type literal — it's not used elsewhere.
    struct LegacyRow {
        id: String,
        provider: String,
        mail_protocol: String,
        imap_host: String,
        imap_port: u16,
        smtp_host: String,
        smtp_port: u16,
        jmap_url: String,
        caldav_url: String,
        use_tls: bool,
        enabled: bool,
        jmap_auth_method: String,
        oidc_token_endpoint: String,
        oidc_client_id: String,
        calendar_sync_enabled: bool,
    }

    let mut stmt = conn.prepare(
        "SELECT id, provider, mail_protocol, imap_host, imap_port,
                smtp_host, smtp_port, jmap_url, caldav_url, use_tls,
                enabled, jmap_auth_method, oidc_token_endpoint, oidc_client_id,
                calendar_sync_enabled
         FROM accounts",
    )?;
    let rows: Vec<LegacyRow> = stmt
        .query_map([], |row| {
            Ok(LegacyRow {
                id: row.get(0)?,
                provider: row.get(1)?,
                mail_protocol: row.get(2)?,
                imap_host: row.get(3)?,
                imap_port: row.get::<_, u32>(4)? as u16,
                smtp_host: row.get(5)?,
                smtp_port: row.get::<_, u32>(6)? as u16,
                jmap_url: row.get(7)?,
                caldav_url: row.get(8)?,
                use_tls: row.get(9)?,
                enabled: row.get(10)?,
                jmap_auth_method: row.get(11)?,
                oidc_token_endpoint: row.get(12)?,
                oidc_client_id: row.get(13)?,
                calendar_sync_enabled: row.get(14)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    for r in &rows {
        rebuild_for_account(
            conn,
            &r.id,
            LegacyBindingFields {
                account_id: &r.id,
                enabled: r.enabled,
                provider: &r.provider,
                mail_protocol: &r.mail_protocol,
                imap_host: &r.imap_host,
                imap_port: r.imap_port,
                smtp_host: &r.smtp_host,
                smtp_port: r.smtp_port,
                use_tls: r.use_tls,
                jmap_url: &r.jmap_url,
                jmap_auth_method: &r.jmap_auth_method,
                oidc_token_endpoint: &r.oidc_token_endpoint,
                oidc_client_id: &r.oidc_client_id,
                caldav_url: &r.caldav_url,
                calendar_sync_enabled: r.calendar_sync_enabled,
                // Migration preserves legacy semantics: mail follows the
                // row's enabled flag, contacts default to on, no per-binding
                // intervals (use frontend defaults).
                mail_sync_enabled: None,
                contacts_sync_enabled: None,
                mail_sync_interval_seconds: None,
                calendar_sync_interval_seconds: None,
                contacts_sync_interval_seconds: None,
            },
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
