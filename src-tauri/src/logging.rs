use std::path::Path;

use crate::error::Result;

/// Set up file + stderr logging. Log file goes into the data directory
/// next to the SQLite database so everything lives in one place.
///
/// File: `<data_dir>/emails.log`  (INFO and above)
/// Stderr: DEBUG and above (only in debug builds)
pub fn init(data_dir: &Path) -> Result<()> {
    let log_path = data_dir.join("emails.log");

    let file_config = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{} {} {}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.target(),
                message,
            ))
        })
        .level(log::LevelFilter::Info)
        // Keep our own crate at debug level in the log file too
        .level_for("emails", log::LevelFilter::Debug)
        .chain(fern::log_file(&log_path).map_err(|e| {
            crate::error::Error::Other(format!(
                "Failed to open log file {}: {}",
                log_path.display(),
                e
            ))
        })?);

    let mut root = fern::Dispatch::new().chain(file_config);

    // In debug builds also log to stderr for `cargo run` convenience
    if cfg!(debug_assertions) {
        let stderr_config = fern::Dispatch::new()
            .format(|out, message, record| {
                out.finish(format_args!(
                    "[{} {}] {}",
                    record.level(),
                    record.target(),
                    message,
                ))
            })
            .level(log::LevelFilter::Debug)
            .level_for("emails", log::LevelFilter::Trace)
            .chain(std::io::stderr());
        root = root.chain(stderr_config);
    }

    root.apply()
        .map_err(|e| crate::error::Error::Other(format!("Failed to init logging: {}", e)))?;

    log::info!("Logging initialised → {}", log_path.display());
    Ok(())
}
