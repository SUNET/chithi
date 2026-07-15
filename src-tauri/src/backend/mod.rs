//! Provider backends (ADR 0050).
//!
//! Each service domain (calendar, contacts, mail) has a trait with one
//! unit-struct implementor per provider, a static [`calendar::registry`]
//! and a `for_account` lookup keyed on the account's enabled service
//! binding — the same shape as `crate::meet::MeetProvider`. Command
//! handlers resolve a backend and make one trait call; they never
//! name-match on protocol strings.
//!
//! Unlike `meet`, backend methods take per-call context (`&AccountFull`,
//! and `&DbPool` for syncs) because provider syncs interleave remote I/O
//! with incremental local upserts and sync-token persistence.

pub mod calendar;
pub mod contacts;
pub mod mail;
