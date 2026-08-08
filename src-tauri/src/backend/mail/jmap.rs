//! JMAP mail backend. Async HTTP throughout; bodies are fetched on
//! demand rather than prefetched (the trait default `prefetch_bodies`
//! no-op applies).

use async_trait::async_trait;

use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::jmap_sync;
use crate::ops::flags::FlagTarget;
use crate::ops::queue::MailOp;

use super::{MailBackend, MailOpExecutor, MailSyncCtx};

pub struct JmapMailBackend;

#[async_trait]
impl MailBackend for JmapMailBackend {
    fn protocol(&self) -> &'static str {
        "jmap"
    }

    async fn sync_account(
        &self,
        ctx: &MailSyncCtx,
        account: &AccountFull,
        current_folder: Option<String>,
    ) -> Result<()> {
        log::info!(
            "Syncing account {} ({}) via JMAP (url={})",
            account.display_name,
            account.email,
            account.jmap_url
        );

        let jmap_config = crate::auth::build_jmap_config(account).await?;

        jmap_sync::sync_jmap_account(
            ctx.app.clone(),
            ctx.db.clone(),
            ctx.data_dir.clone(),
            account.id.clone(),
            account.display_name.clone(),
            jmap_config,
            current_folder,
        )
        .await
    }

    async fn sync_folder(
        &self,
        ctx: &MailSyncCtx,
        account: &AccountFull,
        folder_path: &str,
    ) -> Result<u32> {
        let jmap_config = crate::auth::build_jmap_config(account).await?;
        jmap_sync::sync_jmap_folder_public(
            ctx.app.clone(),
            ctx.db.clone(),
            account.id.clone(),
            account.display_name.clone(),
            folder_path.to_string(),
            jmap_config,
        )
        .await
    }

    fn op_executor(&self) -> Box<dyn MailOpExecutor> {
        Box::new(JmapOpExecutor)
    }
}

/// Stateless executor: JMAP operations use an async HTTP connection per op.
pub(super) struct JmapOpExecutor;

#[async_trait]
impl MailOpExecutor for JmapOpExecutor {
    async fn execute(&mut self, ctx: &MailSyncCtx, account_id: &str, op: MailOp) -> Result<()> {
        match op {
            MailOp::MoveMessages {
                by_folder,
                target_folder,
            } => {
                for (source_mailbox, uids) in &by_folder {
                    // UIDs are actually JMAP email IDs stored as u32 — extract from message IDs
                    // For JMAP, `by_folder` won't have actual UIDs, so this path isn't used.
                    // JMAP moves are handled differently (by JMAP email ID, not UID).
                    let _ = (source_mailbox, uids, &target_folder);
                }
                log::debug!("JMAP move handled by optimistic path");
            }
            MailOp::DeleteMessages { message_refs } => {
                let email_ids = message_refs
                    .into_iter()
                    .map(|message_ref| {
                        message_ref.into_jmap_email_id().ok_or_else(|| {
                            Error::Other(
                                "JMAP executor received a non-JMAP message reference".into(),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let email_ids = email_ids.into_iter().fold(Vec::new(), |mut unique, id| {
                    if !unique.contains(&id) {
                        unique.push(id);
                    }
                    unique
                });
                let account = {
                    let conn = ctx.db.reader();
                    crate::db::accounts::get_account_full(&conn, account_id)?
                };
                let jmap_config = crate::auth::build_jmap_config(&account).await?;
                let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
                conn_jmap.delete_emails(&jmap_config, &email_ids).await?;
            }
            MailOp::SetFlags { mutations } => {
                let prepared = mutations
                    .into_iter()
                    .map(|mutation| {
                        let FlagTarget::Messages(message_refs) = mutation.target else {
                            return Err(Error::Other(
                                "JMAP executor received an IMAP bulk flag target".into(),
                            ));
                        };
                        let email_ids = message_refs
                            .into_iter()
                            .map(|message_ref| {
                                message_ref.into_jmap_email_id().ok_or_else(|| {
                                    Error::Other(
                                        "JMAP executor received a non-JMAP message reference"
                                            .into(),
                                    )
                                })
                            })
                            .collect::<Result<Vec<_>>>()?;
                        Ok((email_ids, mutation.flags, mutation.add))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let account = {
                    let conn = ctx.db.reader();
                    crate::db::accounts::get_account_full(&conn, account_id)?
                };
                let jmap_config = crate::auth::build_jmap_config(&account).await?;
                let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
                for (email_ids, flags, add) in prepared {
                    let flag_strs: Vec<&str> = flags.iter().map(String::as_str).collect();
                    conn_jmap
                        .set_flags(&jmap_config, &email_ids, &flag_strs, add)
                        .await?;
                }
            }
            MailOp::SendRaw { raw_message, .. } => {
                let account = {
                    let conn = ctx.db.reader();
                    crate::db::accounts::get_account_full(&conn, account_id)?
                };
                let jmap_config = crate::auth::build_jmap_config(&account).await?;
                let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
                conn_jmap.send_email(&jmap_config, &raw_message).await?;
            }
            _ => {}
        }
        Ok(())
    }
}
