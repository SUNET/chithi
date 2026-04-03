use serde::Deserialize;
use tauri::State;

use crate::db;
use crate::error::Result;
use crate::mail::smtp;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ComposeMessage {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
}

#[tauri::command]
pub async fn send_message(
    state: State<'_, AppState>,
    account_id: String,
    message: ComposeMessage,
) -> Result<()> {
    log::info!(
        "Send message command: account={} to={:?} subject='{}'",
        account_id,
        message.to,
        message.subject
    );

    // Get the account configuration from the database
    let account = {
        let conn = state.db.lock().await;
        db::accounts::get_account_full(&conn, &account_id)?
    };

    log::debug!(
        "Sending via SMTP {}:{} as {}",
        account.smtp_host,
        account.smtp_port,
        account.email
    );

    // Send the message via SMTP
    smtp::send_message(
        &account.smtp_host,
        account.smtp_port,
        &account.username,
        &account.password,
        account.use_tls,
        &account.email,
        &message.to,
        &message.cc,
        &message.bcc,
        &message.subject,
        &message.body_text,
        message.body_html.as_deref(),
    )
    .await?;

    log::info!(
        "Message sent successfully for account {} to {:?}",
        account_id,
        message.to
    );

    Ok(())
}
