use crate::error::{Error, Result};

/// Application service name used as the "service" parameter in keyring entries.
const SERVICE: &str = "in.kushaldas.chithi";

/// Store a password for the given account ID in the system keyring.
pub fn set_password(account_id: &str, password: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, account_id)
        .map_err(|e| Error::Keyring(format!("Failed to create keyring entry: {}", e)))?;
    entry
        .set_password(password)
        .map_err(|e| Error::Keyring(format!("Failed to store password: {}", e)))?;
    log::info!("Stored password in keyring for account {}", account_id);
    Ok(())
}

/// Retrieve the password for the given account ID from the system keyring.
/// Returns `Ok(None)` when no entry exists (expected for OIDC/OAuth accounts
/// whose credential material lives under a different service name), and
/// `Err` only for real keyring failures (locked, IPC broken, etc.).
pub fn get_password(account_id: &str) -> Result<Option<String>> {
    let entry = keyring::Entry::new(SERVICE, account_id)
        .map_err(|e| Error::Keyring(format!("Failed to create keyring entry: {}", e)))?;
    match entry.get_password() {
        Ok(pw) => Ok(Some(pw)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(Error::Keyring(format!("Failed to retrieve password: {}", e))),
    }
}

/// Delete the password for the given account ID from the system keyring.
pub fn delete_password(account_id: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, account_id)
        .map_err(|e| Error::Keyring(format!("Failed to create keyring entry: {}", e)))?;
    match entry.delete_credential() {
        Ok(()) => {
            log::info!("Deleted password from keyring for account {}", account_id);
            Ok(())
        }
        Err(keyring::Error::NoEntry) => Ok(()), // Already gone
        Err(e) => Err(Error::Keyring(format!("Failed to delete password: {}", e))),
    }
}
