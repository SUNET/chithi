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
            sender_name TEXT NOT NULL DEFAULT '',
            email TEXT NOT NULL,
            username TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            signature TEXT NOT NULL DEFAULT '',
            auth_method TEXT NOT NULL DEFAULT '',
            oidc_token_endpoint TEXT NOT NULL DEFAULT '',
            oidc_client_id TEXT NOT NULL DEFAULT '',
            pgp_attach_pubkey_on_sign INTEGER NOT NULL DEFAULT 1,
            pgp_autocrypt_header INTEGER NOT NULL DEFAULT 1,
            pgp_encrypt_subject INTEGER NOT NULL DEFAULT 1,
            pgp_encrypt_drafts INTEGER NOT NULL DEFAULT 1,
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
            graph_delta_link TEXT,
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
            snippet TEXT,
            graph_prune_pending INTEGER NOT NULL DEFAULT 0,
            graph_filters_pending INTEGER NOT NULL DEFAULT 0
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
            recurrence_kind TEXT NOT NULL DEFAULT 'unknown',
            organizer_email TEXT,
            attendees_json TEXT,
            my_status TEXT,
            pending_rsvp_status TEXT,
            manually_managed_at TEXT,
            source_message_id TEXT,
            ical_data TEXT,
            remote_id TEXT,
            etag TEXT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_events_time ON calendar_events(start_time, end_time);
        CREATE INDEX IF NOT EXISTS idx_events_account_uid
            ON calendar_events(account_id, uid);
        CREATE INDEX IF NOT EXISTS idx_events_account_remote
            ON calendar_events(account_id, remote_id);

        CREATE TABLE IF NOT EXISTS calendar_recurrence_objects (
            object_id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            event_id TEXT NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
            local_series_event_id TEXT REFERENCES calendar_events(id) ON DELETE SET NULL,
            provider_calendar_id TEXT,
            provider_series_id TEXT,
            provider_occurrence_id TEXT,
            recurrence_id TEXT,
            recurrence_timezone TEXT,
            recurrence_value_type TEXT,
            effective_title TEXT NOT NULL,
            effective_description TEXT,
            effective_location TEXT,
            effective_start TEXT NOT NULL,
            effective_end TEXT NOT NULL,
            effective_all_day INTEGER NOT NULL,
            effective_timezone TEXT,
            provider_native_data TEXT,
            provider_revision TEXT,
            object_kind TEXT NOT NULL,
            CHECK (object_id != '' AND account_id != '' AND event_id != ''),
            CHECK (effective_all_day IN (0, 1)),
            CHECK (object_kind IN ('master', 'occurrence', 'exception', 'exclusion')),
            CHECK (recurrence_value_type IS NULL OR
                   recurrence_value_type IN ('date', 'date-time')),
            CHECK (
                ((provider_series_id IS NOT NULL OR
                  provider_occurrence_id IS NOT NULL) AND
                 provider_calendar_id IS NOT NULL AND
                 length(trim(provider_calendar_id)) > 0 AND
                 instr(provider_calendar_id, char(0)) = 0 AND
                 provider_calendar_id NOT GLOB
                    ('*[' || char(1) || '-' || char(31) ||
                     char(127) || '-' || char(159) || ']*'))
                OR
                (provider_series_id IS NULL AND
                 provider_occurrence_id IS NULL AND
                 provider_calendar_id IS NULL)
            ),
            CHECK ((object_kind = 'master' AND recurrence_id IS NULL AND
                    recurrence_value_type IS NULL) OR
                   (object_kind != 'master' AND recurrence_id IS NOT NULL AND
                    recurrence_value_type IS NOT NULL))
        );
        CREATE INDEX IF NOT EXISTS idx_calendar_recurrence_event
            ON calendar_recurrence_objects(event_id);
        CREATE INDEX IF NOT EXISTS idx_calendar_recurrence_local_series
            ON calendar_recurrence_objects(local_series_event_id);
        CREATE INDEX IF NOT EXISTS idx_calendar_recurrence_provider_series
            ON calendar_recurrence_objects(
                account_id, provider_calendar_id, provider_series_id
            );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_calendar_recurrence_local_position
            ON calendar_recurrence_objects(
                local_series_event_id, recurrence_value_type, recurrence_id
            )
            WHERE local_series_event_id IS NOT NULL AND recurrence_id IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_calendar_recurrence_provider_position
            ON calendar_recurrence_objects(
                account_id, provider_calendar_id, provider_series_id,
                recurrence_value_type, recurrence_id
            )
            WHERE provider_calendar_id IS NOT NULL AND
                  provider_series_id IS NOT NULL AND recurrence_id IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_calendar_recurrence_provider_occurrence
            ON calendar_recurrence_objects(
                account_id, provider_calendar_id, provider_occurrence_id
            )
            WHERE provider_calendar_id IS NOT NULL AND
                  provider_occurrence_id IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_calendar_recurrence_event_master
            ON calendar_recurrence_objects(event_id)
            WHERE object_kind = 'master';
        CREATE UNIQUE INDEX IF NOT EXISTS idx_calendar_recurrence_provider_master
            ON calendar_recurrence_objects(
                account_id, provider_calendar_id, provider_series_id
            )
            WHERE object_kind = 'master' AND provider_calendar_id IS NOT NULL AND
                  provider_series_id IS NOT NULL;

        -- A recurrence position identifies the object and cannot be reinterpreted.
        CREATE TRIGGER IF NOT EXISTS calendar_recurrence_id_immutable
        BEFORE UPDATE OF provider_calendar_id, recurrence_id, recurrence_value_type
        ON calendar_recurrence_objects
        WHEN OLD.provider_calendar_id IS NOT NEW.provider_calendar_id
          OR OLD.recurrence_id IS NOT NEW.recurrence_id
          OR OLD.recurrence_value_type IS NOT NEW.recurrence_value_type
        BEGIN
            SELECT RAISE(ABORT, 'recurrence identity is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS calendar_recurrence_account_insert
        BEFORE INSERT ON calendar_recurrence_objects
        WHEN NOT EXISTS (
            SELECT 1 FROM calendar_events
            WHERE id = NEW.event_id AND account_id = NEW.account_id
        ) OR (
            NEW.local_series_event_id IS NOT NULL AND NOT EXISTS (
                SELECT 1 FROM calendar_events
                WHERE id = NEW.local_series_event_id
                  AND account_id = NEW.account_id
            )
        )
        BEGIN
            SELECT RAISE(ABORT, 'recurrence objects cannot cross accounts');
        END;

        CREATE TRIGGER IF NOT EXISTS calendar_recurrence_account_update
        BEFORE UPDATE OF account_id, event_id, local_series_event_id
        ON calendar_recurrence_objects
        WHEN NOT EXISTS (
            SELECT 1 FROM calendar_events
            WHERE id = NEW.event_id AND account_id = NEW.account_id
        ) OR (
            NEW.local_series_event_id IS NOT NULL AND NOT EXISTS (
                SELECT 1 FROM calendar_events
                WHERE id = NEW.local_series_event_id
                  AND account_id = NEW.account_id
            )
        )
        BEGIN
            SELECT RAISE(ABORT, 'recurrence objects cannot cross accounts');
        END;

        -- SET NULL preserves provider-backed objects; local-only objects have
        -- no durable series identity after their local master disappears.
        CREATE TRIGGER IF NOT EXISTS calendar_recurrence_prune_local_series
        BEFORE DELETE ON calendar_events
        BEGIN
            DELETE FROM calendar_recurrence_objects
            WHERE local_series_event_id = OLD.id AND provider_series_id IS NULL;
        END;

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

        -- Local link between a calendar event and a video-conferencing
        -- meeting created via the meet integrations (#148). Keyed on
        -- event_id so a single event has at most one meet binding;
        -- ON DELETE CASCADE drops the binding after the meeting-aware
        -- deletion helper has copied it into the durable cleanup queue.
        -- Survives normal resync (sync
        -- updates `calendar_events` rows in place by remote_id rather
        -- than DELETE+INSERT).
        CREATE TABLE IF NOT EXISTS meet_meetings (
            event_id TEXT PRIMARY KEY REFERENCES calendar_events(id) ON DELETE CASCADE,
            account_id TEXT NOT NULL,
            protocol TEXT NOT NULL,
            meeting_id TEXT NOT NULL,
            join_url TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_meet_meetings_account
            ON meet_meetings(account_id);

        -- Durable ownership for newly-created, discarded, replaced, or
        -- deletion-queued remote meetings. Deliberately has no foreign key:
        -- deleting an account or event must not silently erase retry state.
        CREATE TABLE IF NOT EXISTS meet_pending_meetings (
            lifecycle_id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL,
            protocol TEXT NOT NULL,
            meeting_id TEXT NOT NULL,
            join_url TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            cleanup_requested INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_meet_pending_meetings_account
            ON meet_pending_meetings(account_id);

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
    initialize_calendar_event_state(conn)?;

    Ok(())
}

/// Install durable mutation tokens and invitation evidence after all source
/// tables and migrated columns exist. Seeding and trigger installation commit
/// together, and repeated initialization preserves already-issued tokens.
fn initialize_calendar_event_state(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS calendar_event_revisions (
             revision INTEGER PRIMARY KEY AUTOINCREMENT,
             event_id TEXT NOT NULL UNIQUE
         );

         CREATE TABLE IF NOT EXISTS calendar_invitation_recurrence (
             event_id TEXT PRIMARY KEY REFERENCES calendar_events(id) ON DELETE CASCADE,
             recurrence_rule TEXT NOT NULL
         );

         CREATE TABLE IF NOT EXISTS calendar_invitation_sources (
             event_id TEXT PRIMARY KEY REFERENCES calendar_events(id) ON DELETE CASCADE,
             source_account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
             source_message_id TEXT NOT NULL,
             invitation_uid TEXT NOT NULL
         );
         CREATE UNIQUE INDEX IF NOT EXISTS idx_calendar_invitation_source
             ON calendar_invitation_sources(source_account_id, invitation_uid);

         -- Allocate globally, rather than incrementing a per-event counter:
         -- deleting/replacing an ID must never revive its committed token.
         -- Explicit DELETE/INSERT avoids inheriting the source statement's
         -- conflict policy for an existing revision row.
         CREATE TRIGGER IF NOT EXISTS calendar_event_revision_insert
         AFTER INSERT ON calendar_events BEGIN
             DELETE FROM calendar_event_revisions WHERE event_id = NEW.id;
             INSERT INTO calendar_event_revisions (event_id) VALUES (NEW.id);
         END;

         -- Unconditional: hidden state, timestamps, and no-op writes count.
         -- Changing the root ID retires its old token and replaces any token
         -- at the destination, including UPDATE OR REPLACE with recursion off.
         CREATE TRIGGER IF NOT EXISTS calendar_event_revision_update
         AFTER UPDATE ON calendar_events BEGIN
             DELETE FROM calendar_event_revisions WHERE event_id IN (OLD.id, NEW.id);
             INSERT INTO calendar_event_revisions (event_id)
                 SELECT id FROM calendar_events WHERE id IN (OLD.id, NEW.id);
         END;

         CREATE TRIGGER IF NOT EXISTS calendar_event_revision_delete
         AFTER DELETE ON calendar_events BEGIN
             DELETE FROM calendar_event_revisions WHERE event_id = OLD.id;
         END;

         -- A meeting replacement still fires INSERT when recursive_triggers
         -- is off. During event cascades, only surviving events get tokens.
         CREATE TRIGGER IF NOT EXISTS calendar_event_revision_meeting_insert
         AFTER INSERT ON meet_meetings BEGIN
             DELETE FROM calendar_event_revisions WHERE event_id = NEW.event_id;
             INSERT INTO calendar_event_revisions (event_id)
                 SELECT id FROM calendar_events WHERE id = NEW.event_id;
         END;

         CREATE TRIGGER IF NOT EXISTS calendar_event_revision_meeting_update
         AFTER UPDATE ON meet_meetings BEGIN
             DELETE FROM calendar_event_revisions
                 WHERE event_id IN (OLD.event_id, NEW.event_id);
             INSERT INTO calendar_event_revisions (event_id)
                 SELECT id FROM calendar_events WHERE id IN (OLD.event_id, NEW.event_id);
         END;

         CREATE TRIGGER IF NOT EXISTS calendar_event_revision_meeting_delete
         AFTER DELETE ON meet_meetings BEGIN
             DELETE FROM calendar_event_revisions WHERE event_id = OLD.event_id;
             INSERT INTO calendar_event_revisions (event_id)
                 SELECT id FROM calendar_events WHERE id = OLD.event_id;
         END;

         CREATE TRIGGER IF NOT EXISTS calendar_event_revision_recurrence_insert
         AFTER INSERT ON calendar_recurrence_objects BEGIN
             DELETE FROM calendar_event_revisions WHERE event_id = NEW.event_id;
             INSERT INTO calendar_event_revisions (event_id)
                 SELECT id FROM calendar_events WHERE id = NEW.event_id;
         END;

         CREATE TRIGGER IF NOT EXISTS calendar_event_revision_recurrence_update
         AFTER UPDATE ON calendar_recurrence_objects BEGIN
             DELETE FROM calendar_event_revisions
                 WHERE event_id IN (OLD.event_id, NEW.event_id);
             INSERT INTO calendar_event_revisions (event_id)
                 SELECT id FROM calendar_events
                 WHERE id IN (OLD.event_id, NEW.event_id);
         END;

         -- During an owning-event cascade the SELECT yields no row; the root
         -- delete trigger also removes any token allocated earlier in cascade.
         CREATE TRIGGER IF NOT EXISTS calendar_event_revision_recurrence_delete
         AFTER DELETE ON calendar_recurrence_objects BEGIN
             DELETE FROM calendar_event_revisions WHERE event_id = OLD.event_id;
             INSERT INTO calendar_event_revisions (event_id)
                 SELECT id FROM calendar_events WHERE id = OLD.event_id;
         END;

         -- Never infer invitation proof for legacy rows. INSERT invalidation
         -- also covers REPLACE when SQLite suppresses the implicit DELETE
         -- trigger; uid/remote_id assignment during initial push keeps proof.
         CREATE TRIGGER IF NOT EXISTS calendar_invitation_recurrence_insert
         AFTER INSERT ON calendar_events BEGIN
             DELETE FROM calendar_invitation_recurrence WHERE event_id = NEW.id;
         END;

         CREATE TRIGGER IF NOT EXISTS calendar_invitation_recurrence_delete
         AFTER DELETE ON calendar_events BEGIN
             DELETE FROM calendar_invitation_recurrence WHERE event_id = OLD.id;
         END;

         CREATE TRIGGER IF NOT EXISTS calendar_invitation_recurrence_update
         AFTER UPDATE OF id, recurrence_rule, recurrence_kind, ical_data,
                         source_message_id, account_id ON calendar_events
         WHEN OLD.id IS NOT NEW.id
           OR OLD.recurrence_rule IS NOT NEW.recurrence_rule
           OR OLD.recurrence_kind IS NOT NEW.recurrence_kind
           OR OLD.ical_data IS NOT NEW.ical_data
           OR OLD.source_message_id IS NOT NEW.source_message_id
           OR OLD.account_id IS NOT NEW.account_id
         BEGIN
             DELETE FROM calendar_invitation_recurrence WHERE event_id IN (OLD.id, NEW.id);
         END;

         INSERT INTO calendar_event_revisions (event_id)
             SELECT id FROM calendar_events
             WHERE NOT EXISTS (
                 SELECT 1 FROM calendar_event_revisions WHERE event_id = calendar_events.id
             );",
    )?;
    tx.commit()?;
    Ok(())
}

fn run_migrations(conn: &Connection) -> Result<()> {
    // Skip the legacy column-add and populate migrations on databases that
    // already finished Phase 3. Without this gate, fresh installs would
    // re-create columns we just dropped (the ADD COLUMN sequence below
    // sees the column missing and tries to add it back).
    //
    // Two ways to detect Phase 3:
    // 1. The migration marker is set (existing DB that already migrated).
    // 2. The CREATE TABLE in initialize() ran for a fresh install — i.e.
    //    `provider` was never created, so the legacy column is absent.
    //    Set the marker proactively so we skip the legacy add+drop dance
    //    AND avoid relying on `ALTER TABLE ... DROP COLUMN` support on
    //    fresh installs.
    let mut phase3_done = has_migration(conn, "service_bindings_drop_legacy_columns");
    if !phase3_done {
        let legacy_provider_present = conn
            .prepare("SELECT provider FROM accounts LIMIT 0")
            .is_ok();
        if !legacy_provider_present {
            log::info!(
                "Migration: detected Phase-3 schema (no `provider` column); marking drop-legacy migration done"
            );
            set_migration(conn, "service_bindings_drop_legacy_columns")?;
            phase3_done = true;
        }
    }

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

    // The local account label is not necessarily the user's name. Keep a
    // separate identity for outbound From headers and calendar messages.
    let has_sender_name = conn
        .prepare("SELECT sender_name FROM accounts LIMIT 0")
        .is_ok();
    if !has_sender_name {
        log::info!("Migration: adding sender_name column to accounts table");
        conn.execute_batch(
            "ALTER TABLE accounts ADD COLUMN sender_name TEXT NOT NULL DEFAULT '';",
        )?;
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

    // Manual invite acknowledgement is local workflow state, separate from
    // the provider-controlled iCalendar RSVP in `my_status`.
    let has_manually_managed_at = conn
        .prepare("SELECT manually_managed_at FROM calendar_events LIMIT 0")
        .is_ok();
    if !has_manually_managed_at {
        log::info!("Migration: adding manually_managed_at to calendar_events");
        conn.execute_batch("ALTER TABLE calendar_events ADD COLUMN manually_managed_at TEXT;")?;
    }

    // A locally sent response remains authoritative until provider sync
    // echoes the same status, preventing stale answered states from winning.
    let has_pending_rsvp_status = conn
        .prepare("SELECT pending_rsvp_status FROM calendar_events LIMIT 0")
        .is_ok();
    if !has_pending_rsvp_status {
        log::info!("Migration: adding pending_rsvp_status to calendar_events");
        conn.execute_batch("ALTER TABLE calendar_events ADD COLUMN pending_rsvp_status TEXT;")?;
    }

    // Legacy rows do not distinguish standalone events from occurrences.
    // Add the conservative default before recovering local ICS evidence.
    let has_recurrence_kind = conn
        .prepare("SELECT recurrence_kind FROM calendar_events LIMIT 0")
        .is_ok();
    if !has_recurrence_kind {
        log::info!("Migration: adding recurrence_kind to calendar_events");
        conn.execute_batch(
            "ALTER TABLE calendar_events
             ADD COLUMN recurrence_kind TEXT NOT NULL DEFAULT 'unknown';",
        )?;
    }
    recover_local_event_recurrence(conn)?;

    let has_cleanup_requested = conn
        .prepare("SELECT cleanup_requested FROM meet_pending_meetings LIMIT 0")
        .is_ok();
    if !has_cleanup_requested {
        log::info!("Migration: adding cleanup_requested to pending meetings");
        conn.execute_batch(
            "ALTER TABLE meet_pending_meetings
             ADD COLUMN cleanup_requested INTEGER NOT NULL DEFAULT 0;",
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

    // Per-account OpenPGP "Advanced settings" toggles. Default to 1 (on) so
    // both fresh installs and existing accounts get the four PGP-policy
    // features enabled out of the box; the user can untick each in the
    // account-edit form. Existence-probe + ALTER per column matches the
    // pattern used everywhere else in this file.
    for col in [
        "pgp_attach_pubkey_on_sign",
        "pgp_autocrypt_header",
        "pgp_encrypt_subject",
        "pgp_encrypt_drafts",
    ] {
        let has_col: bool = conn
            .prepare(&format!("SELECT {col} FROM accounts LIMIT 0"))
            .is_ok();
        if !has_col {
            log::info!("Migration: adding {col} column to accounts table");
            conn.execute_batch(&format!(
                "ALTER TABLE accounts ADD COLUMN {col} INTEGER NOT NULL DEFAULT 1;"
            ))?;
        }
    }

    // Microsoft Graph incremental sync: per-folder delta/next link, the
    // Graph analogue of `jmap_state`. NULL means "no delta state yet" and
    // triggers a full initial enumeration of the folder.
    let has_graph_delta_link: bool = conn
        .prepare("SELECT graph_delta_link FROM folders LIMIT 0")
        .is_ok();
    if !has_graph_delta_link {
        log::info!("Migration: adding graph_delta_link column to folders table");
        conn.execute_batch("ALTER TABLE folders ADD COLUMN graph_delta_link TEXT;")?;
    }

    // Graph sync durability markers on messages:
    // - graph_prune_pending: set on every row of a folder when a full
    //   delta enumeration starts, cleared as the enumeration lists each
    //   message; rows still marked when the enumeration completes were
    //   deleted server-side while we had no delta state. Survives
    //   interrupted multi-cycle enumerations, unlike in-memory tracking.
    // - graph_filters_pending: set (in the same transaction as the
    //   insert) on rows whose sync-time filter run hasn't happened yet,
    //   cleared after the filter pass; a crash between insert and filter
    //   run is retried on the next cycle instead of silently skipped.
    for col in ["graph_prune_pending", "graph_filters_pending"] {
        let has_col: bool = conn
            .prepare(&format!("SELECT {col} FROM messages LIMIT 0"))
            .is_ok();
        if !has_col {
            log::info!("Migration: adding {col} column to messages table");
            conn.execute_batch(&format!(
                "ALTER TABLE messages ADD COLUMN {col} INTEGER NOT NULL DEFAULT 0;"
            ))?;
        }
    }

    Ok(())
}

/// Recover local-only legacy rows from retained, matching ICS evidence.
/// Run independently of column creation so an interrupted startup is retryable.
fn recover_local_event_recurrence(conn: &Connection) -> Result<()> {
    use crate::calendar::{ical::parse_ical_data, RecurrenceKind};

    struct Candidate {
        id: String,
        uid: String,
        start_time: String,
        all_day: Option<bool>,
        recurrence_rule: Option<String>,
        ical_data: String,
    }

    let tx = conn.unchecked_transaction()?;
    let candidates = {
        let mut stmt = tx.prepare(
            "SELECT id, uid, start_time, all_day, recurrence_rule, ical_data
             FROM calendar_events
             WHERE recurrence_kind = 'unknown'
               AND (remote_id IS NULL OR remote_id = '')
               AND uid IS NOT NULL AND uid != '' AND start_time != ''
               AND ical_data IS NOT NULL AND ical_data != ''
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Candidate {
                    id: row.get(0)?,
                    uid: row.get(1)?,
                    start_time: row.get(2)?,
                    all_day: row.get(3)?,
                    recurrence_rule: row.get(4)?,
                    ical_data: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    let mut recovered = 0;
    for candidate in candidates {
        let parsed = parse_ical_data(&candidate.ical_data);
        let [invite] = parsed.as_slice() else {
            continue;
        };
        if invite.recurrence_kind == RecurrenceKind::Unknown
            || invite.uid != candidate.uid
            || invite.dtstart != candidate.start_time
            || Some(invite.all_day) != candidate.all_day
        {
            continue;
        }
        if invite.recurrence_kind == RecurrenceKind::Standalone
            && candidate
                .recurrence_rule
                .as_deref()
                .is_some_and(|rule| !rule.is_empty())
        {
            continue;
        }

        recovered += tx.execute(
            "UPDATE calendar_events SET recurrence_kind = ?1
             WHERE id = ?2 AND recurrence_kind = 'unknown'
               AND (remote_id IS NULL OR remote_id = '')",
            rusqlite::params![invite.recurrence_kind.as_str(), candidate.id],
        )?;
    }
    tx.commit()?;
    if recovered > 0 {
        log::info!("Recovered recurrence classification for {recovered} local calendar events");
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
                // The Phase-1 populate migration runs against rows
                // that pre-date the meet binding (#148), so emit
                // nothing for them.
                meet_url: "",
                meet_protocol: "",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recurrence_identity_schema_is_empty_preserving_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recurrence-identity.db");
        {
            let conn = Connection::open(&path).unwrap();
            initialize(&conn).unwrap();
            seed_recovery_account(&conn);
            insert_recovery_row(&conn, "legacy-event", None, Some("provider-event"));
            assert_eq!(
                conn.query_row(
                    "SELECT COUNT(*) FROM calendar_recurrence_objects",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
                0
            );
            conn.execute_batch(
                "INSERT INTO calendar_recurrence_objects
                     (object_id, account_id, event_id, provider_calendar_id,
                       provider_series_id,
                       recurrence_id, recurrence_value_type, effective_title,
                       effective_start, effective_end, effective_all_day,
                       object_kind)
                 VALUES ('occurrence', 'account', 'legacy-event',
                          'provider-calendar', 'provider-series',
                          '2026-09-15', 'date',
                          'Occurrence', '2026-09-15', '2026-09-16', 1,
                         'occurrence');",
            )
            .unwrap();
        }

        for _ in 0..2 {
            let conn = Connection::open(&path).unwrap();
            initialize(&conn).unwrap();
            assert_eq!(
                conn.query_row(
                    "SELECT COUNT(*) FROM calendar_recurrence_objects",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
                1
            );
            for index in [
                "idx_calendar_recurrence_event",
                "idx_calendar_recurrence_local_series",
                "idx_calendar_recurrence_provider_series",
                "idx_calendar_recurrence_local_position",
                "idx_calendar_recurrence_provider_position",
                "idx_calendar_recurrence_provider_occurrence",
                "idx_calendar_recurrence_event_master",
                "idx_calendar_recurrence_provider_master",
            ] {
                assert_eq!(
                    conn.query_row(
                        "SELECT COUNT(*) FROM sqlite_schema
                         WHERE type = 'index' AND name = ?1",
                        [index],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                    1,
                    "{index}"
                );
            }
            for trigger in [
                "calendar_recurrence_id_immutable",
                "calendar_recurrence_prune_local_series",
                "calendar_recurrence_account_insert",
                "calendar_recurrence_account_update",
                "calendar_event_revision_recurrence_insert",
                "calendar_event_revision_recurrence_update",
                "calendar_event_revision_recurrence_delete",
            ] {
                assert_eq!(
                    conn.query_row(
                        "SELECT COUNT(*) FROM sqlite_schema
                         WHERE type = 'trigger' AND name = ?1",
                        [trigger],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                    1,
                    "{trigger}"
                );
            }
        }
    }

    fn recovery_ics(uid: &str, recurrence: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Chithi//Recovery Test//EN\r\n\
             BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20260901T090000Z\r\n\
             DTSTART:20260913T100000Z\r\nDTEND:20260913T110000Z\r\n\
             SUMMARY:Retained Event\r\n{recurrence}END:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    fn seed_recovery_account(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('account', 'Test', 'test@example.com', 'test@example.com');
             INSERT INTO calendars (id, account_id, name)
             VALUES ('calendar', 'account', 'Calendar');",
        )
        .unwrap();
    }

    fn insert_recovery_row(
        conn: &Connection,
        id: &str,
        ical_data: Option<&str>,
        remote_id: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO calendar_events
                (id, account_id, calendar_id, uid, title, start_time, end_time,
                 ical_data, remote_id, updated_at)
             VALUES (?1, 'account', 'calendar', 'recovery@example.com', 'Legacy Event',
                     '2026-09-13T10:00:00Z', '2026-09-13T11:00:00Z', ?2, ?3,
                     '2026-09-01T09:00:00Z')",
            rusqlite::params![id, ical_data, remote_id],
        )
        .unwrap();
    }

    fn invitation_connection(recursive: bool) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "recursive_triggers", recursive)
            .unwrap();
        initialize(&conn).unwrap();
        seed_recovery_account(&conn);
        insert_recovery_row(&conn, "event", None, None);
        conn.execute_batch(
            "UPDATE calendar_events
             SET recurrence_rule = 'FREQ=WEEKLY', recurrence_kind = 'series';
             INSERT INTO accounts (id, display_name, email, username)
             VALUES ('other-account', 'Other', 'other@example.com', 'other@example.com');",
        )
        .unwrap();
        conn
    }

    fn store_invitation_proof(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO calendar_invitation_recurrence (event_id, recurrence_rule)
             VALUES ('event', 'FREQ=WEEKLY');",
        )
        .unwrap();
    }

    fn invitation_proof_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM calendar_invitation_recurrence",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn invitation_proof_migration_starts_empty_and_preserves_proof_on_restart() {
        for recursive in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("invitation-proof.db");
            {
                let conn = Connection::open(&path).unwrap();
                conn.pragma_update(None, "recursive_triggers", recursive)
                    .unwrap();
                initialize(&conn).unwrap();
                seed_recovery_account(&conn);
                insert_recovery_row(&conn, "event", None, None);
                conn.execute_batch(
                    "DROP TRIGGER calendar_invitation_recurrence_insert;
                     DROP TRIGGER calendar_invitation_recurrence_update;
                     DROP TRIGGER calendar_invitation_recurrence_delete;
                     DROP TABLE calendar_invitation_recurrence;
                     UPDATE calendar_events
                     SET recurrence_rule = 'FREQ=WEEKLY', recurrence_kind = 'series';",
                )
                .unwrap();
                initialize(&conn).unwrap();
                assert_eq!(invitation_proof_count(&conn), 0);
                store_invitation_proof(&conn);
            }
            for _ in 0..2 {
                let conn = Connection::open(&path).unwrap();
                conn.pragma_update(None, "recursive_triggers", recursive)
                    .unwrap();
                initialize(&conn).unwrap();
                let proof: String = conn
                    .query_row(
                        "SELECT recurrence_rule FROM calendar_invitation_recurrence
                         WHERE event_id = 'event'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(proof, "FREQ=WEEKLY");
                assert_eq!(invitation_proof_count(&conn), 1);
            }
        }
    }

    #[test]
    fn invitation_proof_is_invalidated_by_actual_evidence_changes_including_nulls() {
        for recursive in [false, true] {
            let conn = invitation_connection(recursive);
            for assignment in [
                "recurrence_rule = 'FREQ=DAILY'",
                "recurrence_rule = NULL",
                "recurrence_rule = ''",
                "recurrence_kind = 'standalone'",
                "ical_data = 'retained invitation'",
                "ical_data = NULL",
                "ical_data = ''",
                "source_message_id = 'source-message'",
                "source_message_id = NULL",
                "source_message_id = ''",
                "account_id = 'other-account'",
                "id = 'renamed'",
            ] {
                store_invitation_proof(&conn);
                conn.execute(
                    &format!("UPDATE calendar_events SET {assignment} WHERE id = 'event'"),
                    [],
                )
                .unwrap();
                assert_eq!(invitation_proof_count(&conn), 0, "{assignment}");
            }
        }
    }

    #[test]
    fn invitation_proof_survives_noops_initial_push_and_unrelated_changes() {
        for recursive in [false, true] {
            let conn = invitation_connection(recursive);
            store_invitation_proof(&conn);
            for assignment in [
                "id = id, recurrence_rule = recurrence_rule, recurrence_kind = recurrence_kind,
                 ical_data = ical_data, source_message_id = source_message_id,
                 account_id = account_id",
                "remote_id = 'initial-jmap-id', uid = 'initial-jmap-uid'",
                "title = 'Edited title', location = 'Room', description = 'Description'",
                "pending_rsvp_status = 'ACCEPTED', manually_managed_at = CURRENT_TIMESTAMP",
                "updated_at = CURRENT_TIMESTAMP, etag = 'new-etag'",
            ] {
                conn.execute(
                    &format!("UPDATE calendar_events SET {assignment} WHERE id = 'event'"),
                    [],
                )
                .unwrap();
                assert_eq!(invitation_proof_count(&conn), 1, "{assignment}");
            }
        }
    }

    #[test]
    fn invitation_proof_is_cleared_on_delete_reinsert_and_identical_replacement() {
        for recursive in [false, true] {
            for foreign_keys in [false, true] {
                let conn = invitation_connection(recursive);
                conn.pragma_update(None, "foreign_keys", foreign_keys)
                    .unwrap();
                store_invitation_proof(&conn);
                conn.execute_batch(
                    "INSERT OR REPLACE INTO calendar_events
                     SELECT * FROM calendar_events WHERE id = 'event';",
                )
                .unwrap();
                assert_eq!(invitation_proof_count(&conn), 0);
                store_invitation_proof(&conn);
                conn.execute("DELETE FROM calendar_events WHERE id = 'event'", [])
                    .unwrap();
                assert_eq!(invitation_proof_count(&conn), 0);
                if !foreign_keys {
                    // Simulate leftover evidence so INSERT invalidation is
                    // exercised independently of FK and DELETE cleanup.
                    store_invitation_proof(&conn);
                }
                insert_recovery_row(&conn, "event", None, None);
                assert_eq!(invitation_proof_count(&conn), 0);
            }
        }
    }

    #[test]
    fn invitation_proof_invalidation_rolls_back_with_the_event_write() {
        for recursive in [false, true] {
            let conn = invitation_connection(recursive);
            store_invitation_proof(&conn);
            for sql in [
                "UPDATE calendar_events SET recurrence_rule = NULL WHERE id = 'event'",
                "DELETE FROM calendar_events WHERE id = 'event'",
                "INSERT OR REPLACE INTO calendar_events
                 SELECT * FROM calendar_events WHERE id = 'event'",
            ] {
                let tx = conn.unchecked_transaction().unwrap();
                tx.execute(sql, []).unwrap();
                assert_eq!(invitation_proof_count(&tx), 0);
                tx.rollback().unwrap();
                assert_eq!(invitation_proof_count(&conn), 1);
                let rule: String = conn
                    .query_row(
                        "SELECT recurrence_rule FROM calendar_events WHERE id = 'event'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(rule, "FREQ=WEEKLY");
            }
        }
    }

    #[test]
    fn local_recurrence_recovery_classifies_trustworthy_single_event_ics() {
        use crate::calendar::RecurrenceKind;

        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed_recovery_account(&conn);
        let cases = [
            ("standalone", "", None, RecurrenceKind::Standalone),
            ("empty-remote", "", Some(""), RecurrenceKind::Standalone),
            (
                "series",
                "RRULE:FREQ=WEEKLY\r\n",
                None,
                RecurrenceKind::Series,
            ),
            (
                "rdate",
                "RDATE:20260920T100000Z\r\n",
                None,
                RecurrenceKind::Series,
            ),
            (
                "occurrence",
                "RECURRENCE-ID:20260913T100000Z\r\n",
                None,
                RecurrenceKind::Occurrence,
            ),
        ];
        let mut expected = Vec::new();
        for (id, recurrence, remote_id, kind) in cases {
            let ics = recovery_ics("recovery@example.com", recurrence);
            insert_recovery_row(&conn, id, Some(&ics), remote_id);
            let mut event = crate::db::calendar::get_event(&conn, id).unwrap();
            event.recurrence_kind = kind;
            expected.push(event);
        }
        conn.execute_batch(
            "DROP TRIGGER calendar_invitation_recurrence_update;
             ALTER TABLE calendar_events DROP COLUMN recurrence_kind;",
        )
        .unwrap();

        initialize(&conn).unwrap();

        for event in expected {
            let recovered = crate::db::calendar::get_event(&conn, &event.id).unwrap();
            assert_eq!(
                serde_json::to_value(recovered).unwrap(),
                serde_json::to_value(event).unwrap()
            );
        }
        let untouched_timestamps: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM calendar_events WHERE updated_at = '2026-09-01T09:00:00Z'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(untouched_timestamps, 5);
    }

    #[test]
    fn local_recurrence_recovery_leaves_untrusted_or_provider_rows_unchanged() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        seed_recovery_account(&conn);
        let valid = recovery_ics("recovery@example.com", "");
        let cases = [
            ("no-ics", None, None),
            ("empty-ics", Some(String::new()), None),
            ("malformed", Some("not iCalendar".into()), None),
            (
                "missing-metadata",
                Some(valid.replace("DTSTAMP:20260901T090000Z\r\n", "")),
                None,
            ),
            (
                "missing-uid",
                Some(valid.replace("UID:recovery@example.com\r\n", "")),
                None,
            ),
            ("ambiguous", Some(format!("{valid}{valid}")), None),
            (
                "uid-mismatch",
                Some(recovery_ics("another@example.com", "")),
                None,
            ),
            (
                "start-mismatch",
                Some(valid.replace("DTSTART:20260913T100000Z", "DTSTART:20260920T100000Z")),
                None,
            ),
            ("provider", Some(valid.clone()), Some("remote-event")),
            (
                "stale-provider-ics",
                Some(valid.clone()),
                Some("remote-series"),
            ),
            ("contradictory-rule", Some(valid.clone()), None),
            ("whitespace-rule", Some(valid.clone()), None),
            ("missing-local-uid", Some(valid.clone()), None),
            ("all-day-mismatch", Some(valid.clone()), None),
            ("known-occurrence", Some(valid), None),
        ];
        for (id, ical_data, remote_id) in &cases {
            insert_recovery_row(&conn, id, ical_data.as_deref(), *remote_id);
        }
        conn.execute_batch(
            "UPDATE calendar_events SET recurrence_rule = 'FREQ=WEEKLY'
             WHERE id IN ('contradictory-rule', 'stale-provider-ics');
             UPDATE calendar_events SET recurrence_rule = ' ' WHERE id = 'whitespace-rule';
             UPDATE calendar_events SET uid = NULL WHERE id = 'missing-local-uid';
             UPDATE calendar_events SET all_day = 1 WHERE id = 'all-day-mismatch';
             UPDATE calendar_events SET recurrence_kind = 'occurrence' WHERE id = 'known-occurrence';",
        )
        .unwrap();
        let expected: Vec<_> = cases
            .iter()
            .map(|(id, _, _)| crate::db::calendar::get_event(&conn, id).unwrap())
            .collect();

        initialize(&conn).unwrap();
        initialize(&conn).unwrap();

        for event in expected {
            let actual = crate::db::calendar::get_event(&conn, &event.id).unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(event).unwrap()
            );
        }
    }

    #[test]
    fn local_recurrence_recovery_rolls_back_and_resumes_idempotently_after_restart() {
        use crate::calendar::RecurrenceKind;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-recurrence-recovery.db");
        {
            let conn = Connection::open(&path).unwrap();
            initialize(&conn).unwrap();
            seed_recovery_account(&conn);
            insert_recovery_row(
                &conn,
                "a-standalone",
                Some(&recovery_ics("recovery@example.com", "")),
                None,
            );
            insert_recovery_row(
                &conn,
                "b-series",
                Some(&recovery_ics(
                    "recovery@example.com",
                    "RRULE:FREQ=WEEKLY\r\n",
                )),
                None,
            );
            conn.execute_batch(
                "CREATE TRIGGER interrupt_recurrence_recovery
                 BEFORE UPDATE OF recurrence_kind ON calendar_events
                 WHEN NEW.id = 'b-series'
                 BEGIN SELECT RAISE(ABORT, 'simulated interruption'); END;",
            )
            .unwrap();

            assert!(initialize(&conn).is_err());
            for id in ["a-standalone", "b-series"] {
                assert_eq!(
                    crate::db::calendar::get_event(&conn, id)
                        .unwrap()
                        .recurrence_kind,
                    RecurrenceKind::Unknown
                );
            }
            conn.execute_batch("DROP TRIGGER interrupt_recurrence_recovery;")
                .unwrap();
        }

        for _ in 0..2 {
            let conn = Connection::open(&path).unwrap();
            initialize(&conn).unwrap();
            for (id, kind) in [
                ("a-standalone", RecurrenceKind::Standalone),
                ("b-series", RecurrenceKind::Series),
            ] {
                assert_eq!(
                    crate::db::calendar::get_event(&conn, id)
                        .unwrap()
                        .recurrence_kind,
                    kind
                );
            }
        }
    }

    #[test]
    fn recurrence_kind_fresh_schema_defaults_unknown_and_rejects_null() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        let column: (String, bool, String) = conn
            .query_row(
                "SELECT type, \"notnull\", dflt_value
                 FROM pragma_table_info('calendar_events') WHERE name = 'recurrence_kind'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(column, ("TEXT".into(), true, "'unknown'".into()));

        conn.execute_batch(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('account', 'Test', 'test@example.com', 'test@example.com');
             INSERT INTO calendar_events
                (id, account_id, calendar_id, title, start_time, end_time)
             VALUES ('event', 'account', 'calendar', 'Event',
                     '2026-09-13T10:00:00Z', '2026-09-13T11:00:00Z');",
        )
        .unwrap();
        let stored: String = conn
            .query_row(
                "SELECT recurrence_kind FROM calendar_events WHERE id = 'event'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "unknown");
        assert!(conn
            .execute(
                "UPDATE calendar_events SET recurrence_kind = NULL WHERE id = 'event'",
                [],
            )
            .is_err());
    }

    #[test]
    fn recurrence_kind_migration_is_conservative_and_idempotent_across_restarts() {
        use crate::calendar::RecurrenceKind;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calendar-recurrence.db");
        let legacy_rows = [
            ("local", None, None),
            ("empty-rule", Some(""), Some("remote-empty")),
            ("series", Some("FREQ=WEEKLY"), Some("remote-series")),
            ("occurrence", None, Some("remote-occurrence")),
        ];
        {
            let conn = Connection::open(&path).unwrap();
            initialize(&conn).unwrap();
            conn.execute_batch(
                "DROP TRIGGER calendar_invitation_recurrence_update;
                 ALTER TABLE calendar_events DROP COLUMN recurrence_kind;
                 INSERT INTO accounts (id, display_name, email, username)
                 VALUES ('account', 'Test', 'test@example.com', 'test@example.com');
                 INSERT INTO calendars (id, account_id, name)
                 VALUES ('calendar', 'account', 'Calendar');",
            )
            .unwrap();
            for (id, rule, remote_id) in legacy_rows {
                conn.execute(
                    "INSERT INTO calendar_events
                        (id, account_id, calendar_id, title, start_time, end_time,
                         recurrence_rule, remote_id)
                     VALUES (?1, 'account', 'calendar', 'Legacy Event',
                             '2026-09-13T10:00:00Z', '2026-09-13T11:00:00Z', ?2, ?3)",
                    rusqlite::params![id, rule, remote_id],
                )
                .unwrap();
            }
        }

        {
            let conn = Connection::open(&path).unwrap();
            initialize(&conn).unwrap();
            for (id, rule, remote_id) in legacy_rows {
                let event = crate::db::calendar::get_event(&conn, id).unwrap();
                assert_eq!(event.recurrence_kind, RecurrenceKind::Unknown, "{id}");
                assert_eq!(event.recurrence_rule.as_deref(), rule);
                assert_eq!(event.remote_id.as_deref(), remote_id);
                assert!(event.ensure_mutable().is_err());
            }
            conn.execute_batch(
                "UPDATE calendar_events SET recurrence_kind = 'standalone' WHERE id = 'local';
                 UPDATE calendar_events SET recurrence_kind = 'series' WHERE id = 'series';
                 UPDATE calendar_events SET recurrence_kind = 'occurrence' WHERE id = 'occurrence';",
            )
            .unwrap();
        }

        for _ in 0..2 {
            let conn = Connection::open(&path).unwrap();
            initialize(&conn).unwrap();
            let mut stmt = conn
                .prepare("SELECT id, recurrence_kind FROM calendar_events ORDER BY id")
                .unwrap();
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(
                rows,
                vec![
                    ("empty-rule".into(), "unknown".into()),
                    ("local".into(), "standalone".into()),
                    ("occurrence".into(), "occurrence".into()),
                    ("series".into(), "series".into()),
                ]
            );
        }
    }

    /// Fresh install and migration re-run must both yield the Graph sync
    /// columns (delta link + durability markers) and be idempotent.
    #[test]
    fn graph_sync_columns_exist_and_migrations_are_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();

        for probe in [
            "SELECT sender_name FROM accounts LIMIT 0",
            "SELECT graph_delta_link FROM folders LIMIT 0",
            "SELECT graph_prune_pending FROM messages LIMIT 0",
            "SELECT graph_filters_pending FROM messages LIMIT 0",
        ] {
            conn.prepare(probe)
                .unwrap_or_else(|e| panic!("missing column for `{probe}`: {e}"));
        }

        // Running initialize again must not fail (existence probes skip
        // the ALTERs on an already-migrated database).
        initialize(&conn).unwrap();

        // Marker defaults: inserted rows start unmarked.
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('a1', 'Test', 't@example.com', 't@example.com')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, folder_path, date)
             VALUES ('a1_m1', 'a1', 'f1', '2026-07-23T00:00:00Z')",
            [],
        )
        .unwrap();
        let (prune, filters): (i64, i64) = conn
            .query_row(
                "SELECT graph_prune_pending, graph_filters_pending
                 FROM messages WHERE id = 'a1_m1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((prune, filters), (0, 0));
    }

    #[test]
    fn sender_name_migration_preserves_accounts_and_defaults_empty() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('a1', 'Personal', 'ada@example.com', 'ada@example.com')",
            [],
        )
        .unwrap();
        conn.execute_batch("ALTER TABLE accounts DROP COLUMN sender_name;")
            .unwrap();

        initialize(&conn).unwrap();
        initialize(&conn).unwrap();

        let account: (String, String) = conn
            .query_row(
                "SELECT display_name, sender_name FROM accounts WHERE id = 'a1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(account, ("Personal".into(), String::new()));
    }

    #[test]
    fn pending_meeting_ownership_survives_account_deletion() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, display_name, email, username)
             VALUES ('account', 'Test', 'test@example.com', 'test@example.com')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meet_pending_meetings
                (lifecycle_id, account_id, protocol, meeting_id, join_url)
             VALUES ('lifecycle', 'account', 'zoom', 'meeting', 'https://example.test')",
            [],
        )
        .unwrap();

        conn.execute("DELETE FROM accounts WHERE id = 'account'", [])
            .unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM meet_pending_meetings
                 WHERE lifecycle_id = 'lifecycle'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn pending_cleanup_flag_migration_preserves_rows_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meet_pending_meetings (
                lifecycle_id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                protocol TEXT NOT NULL,
                meeting_id TEXT NOT NULL,
                join_url TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             INSERT INTO meet_pending_meetings
                (lifecycle_id, account_id, protocol, meeting_id, join_url)
             VALUES ('existing', 'account', 'zoom', 'meeting', 'https://example.test');",
        )
        .unwrap();

        initialize(&conn).unwrap();
        initialize(&conn).unwrap();

        let cleanup_requested: bool = conn
            .query_row(
                "SELECT cleanup_requested FROM meet_pending_meetings
                 WHERE lifecycle_id = 'existing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!cleanup_requested);
    }

    #[test]
    fn invite_management_migration_preserves_existing_rsvp() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        conn.execute_batch(
            "ALTER TABLE calendar_events DROP COLUMN manually_managed_at;
             ALTER TABLE calendar_events DROP COLUMN pending_rsvp_status;
             INSERT INTO accounts (id, display_name, email, username)
             VALUES ('account', 'Test', 'test@example.com', 'test@example.com');
             INSERT INTO calendars (id, account_id, name)
             VALUES ('calendar', 'account', 'Calendar');
             INSERT INTO calendar_events
                (id, account_id, calendar_id, title, start_time, end_time, my_status)
             VALUES
                ('invite', 'account', 'calendar', 'Invite',
                 '2026-08-27T10:00:00Z', '2026-08-27T11:00:00Z', 'accepted');",
        )
        .unwrap();

        initialize(&conn).unwrap();
        initialize(&conn).unwrap();

        let (status, managed_at, pending_status): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT my_status, manually_managed_at, pending_rsvp_status
                 FROM calendar_events WHERE id = 'invite'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "accepted");
        assert!(managed_at.is_none());
        assert!(pending_status.is_none());
    }
}
