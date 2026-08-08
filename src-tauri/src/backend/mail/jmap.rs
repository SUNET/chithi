//! JMAP mail backend. Async HTTP throughout; bodies are fetched on
//! demand rather than prefetched (the trait default `prefetch_bodies`
//! no-op applies).

use async_trait::async_trait;

use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::jmap_sync;
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

/// Stateless executor: JMAP ops are async HTTP, no persistent
/// connection needed. Move/delete/flag ops are already applied by the
/// optimistic command path — only queued sends do real work here.
pub(super) struct JmapOpExecutor;

#[async_trait]
impl MailOpExecutor for JmapOpExecutor {
    async fn execute(&mut self, ctx: &MailSyncCtx, account_id: &str, op: MailOp) -> Result<()> {
        let account = {
            let conn = ctx.db.reader();
            crate::db::accounts::get_account_full(&conn, account_id)?
        };
        let jmap_config = crate::auth::build_jmap_config(&account).await?;
        let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;

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
            MailOp::DeleteMessages { by_folder } => {
                let _ = by_folder;
                log::debug!("JMAP delete handled by optimistic path");
            }
            MailOp::SetFlags { mutations } => {
                for mutation in mutations {
                    let email_ids = mutation
                        .message_refs
                        .into_iter()
                        .map(|message_ref| {
                            message_ref.into_jmap_email_id().ok_or_else(|| {
                                Error::Other(
                                    "JMAP executor received a non-JMAP message reference".into(),
                                )
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let flag_strs: Vec<&str> = mutation.flags.iter().map(String::as_str).collect();
                    conn_jmap
                        .set_flags(&jmap_config, &email_ids, &flag_strs, mutation.add)
                        .await?;
                }
            }
            MailOp::SendRaw { raw_message, .. } => {
                conn_jmap.send_email(&jmap_config, &raw_message).await?;
            }
            _ => {}
        }
        Ok(())
    }
}
