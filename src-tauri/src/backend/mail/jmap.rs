//! JMAP mail backend. Async HTTP throughout; bodies are fetched on
//! demand rather than prefetched (the trait default `prefetch_bodies`
//! no-op applies).

use async_trait::async_trait;

use crate::db::accounts::AccountFull;
use crate::error::{Error, Result};
use crate::mail::jmap_sync;
use crate::mail::search::{SearchHit, SearchQuery};
use crate::ops::flags::FlagTarget;
use crate::ops::queue::MailOp;

use super::{
    BodyFetchRequest, DraftSaveRequest, DraftStorageFormat, MailBackend, MailOpExecutor,
    MailSyncCtx,
};

pub struct JmapMailBackend;

fn body_fetch_email_id(request: &BodyFetchRequest) -> Result<&str> {
    match &request.message_ref {
        crate::mail::compat::BackendMessageRef::Jmap { email_id, .. } => Ok(email_id),
        _ => Err(Error::Other(
            "JMAP body fetch received a non-JMAP message reference".into(),
        )),
    }
}

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

    async fn fetch_body_to_disk(
        &self,
        ctx: &MailSyncCtx,
        account: &AccountFull,
        request: &BodyFetchRequest,
    ) -> Result<String> {
        let email_id = body_fetch_email_id(request)?;

        let config = crate::auth::build_jmap_config(account).await?;
        jmap_sync::fetch_and_store_jmap_body(
            &config,
            &ctx.data_dir,
            &account.id,
            &request.folder_path,
            email_id,
            &request.flags,
        )
        .await
    }

    async fn search_messages(
        &self,
        account: &AccountFull,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>> {
        let config = crate::auth::build_jmap_config(account).await?;
        let connection = crate::mail::jmap::JmapConnection::connect(&config).await?;
        connection.search_account(&config, &account.id, query).await
    }

    fn draft_storage_format(&self) -> DraftStorageFormat {
        DraftStorageFormat::RawMime
    }

    async fn save_draft(&self, account: &AccountFull, request: &DraftSaveRequest) -> Result<()> {
        let config = crate::auth::build_jmap_config(account).await?;
        let connection = crate::mail::jmap::JmapConnection::connect(&config).await?;
        connection.save_draft(&config, &request.raw_message).await
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
            MailOp::CopyMessages {
                message_refs,
                target_folder,
            } => {
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
                conn_jmap
                    .copy_emails(&jmap_config, &email_ids, &target_folder)
                    .await?;
            }
            MailOp::MoveMessages {
                message_refs,
                target_folder,
            } => {
                let mut by_mailbox = std::collections::HashMap::<String, Vec<String>>::new();
                for message_ref in message_refs {
                    let crate::mail::compat::BackendMessageRef::Jmap {
                        mailbox_id,
                        email_id,
                    } = message_ref
                    else {
                        return Err(Error::Other(
                            "JMAP executor received a non-JMAP message reference".into(),
                        ));
                    };
                    let email_ids = by_mailbox.entry(mailbox_id).or_default();
                    if !email_ids.contains(&email_id) {
                        email_ids.push(email_id);
                    }
                }
                let account = {
                    let conn = ctx.db.reader();
                    crate::db::accounts::get_account_full(&conn, account_id)?
                };
                let jmap_config = crate::auth::build_jmap_config(&account).await?;
                let conn_jmap = crate::mail::jmap::JmapConnection::connect(&jmap_config).await?;
                for (source_mailbox, email_ids) in by_mailbox {
                    conn_jmap
                        .move_emails(&jmap_config, &email_ids, &source_mailbox, &target_folder)
                        .await?;
                }
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

#[cfg(test)]
mod tests {
    use super::body_fetch_email_id;
    use crate::backend::mail::BodyFetchRequest;
    use crate::mail::compat::{BackendMessageRef, BodyLocation};

    #[test]
    fn body_fetch_accepts_opaque_email_id_with_underscores() {
        let request = BodyFetchRequest {
            message_id: "db-id".into(),
            message_ref: BackendMessageRef::jmap("mailbox", "opaque_id_value"),
            folder_path: "mailbox".into(),
            flags: Vec::new(),
            body_location: BodyLocation::NotFetched,
        };
        assert_eq!(body_fetch_email_id(&request).unwrap(), "opaque_id_value");
    }

    #[test]
    fn body_fetch_rejects_non_jmap_reference() {
        let request = BodyFetchRequest {
            message_id: "db-id".into(),
            message_ref: BackendMessageRef::imap("INBOX", 1),
            folder_path: "INBOX".into(),
            flags: Vec::new(),
            body_location: BodyLocation::NotFetched,
        };
        assert!(body_fetch_email_id(&request).is_err());
    }
}
