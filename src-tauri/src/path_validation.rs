//! Defensive path resolution for DB-derived file reads.
//!
//! The maildir path we store per message is a relative path under the
//! per-user `data_dir`. If a DB row is ever crafted or corrupted (path
//! traversal segments, absolute path, symlink escape) a naive
//! `data_dir.join(path_from_db)` followed by `std::fs::read` would read
//! files outside the data dir. `resolve_under` centralises the check.

use crate::error::{Error, Result};
use std::path::{Component, Path, PathBuf};

/// Join `rel` under `base`, canonicalise the result, and verify it is still
/// contained within the canonicalised `base`.
///
/// Returns `Err` if:
/// - `rel` is empty, absolute, or contains any `..` component;
/// - the joined path cannot be resolved (e.g. the file does not exist);
/// - the resolved canonical path escapes `base` (e.g. via a symlink).
///
/// `base` is canonicalised per call; for tight loops prefer
/// [`resolve_under_canonical`] with a pre-canonicalised base.
pub fn resolve_under(base: &Path, rel: impl AsRef<Path>) -> Result<PathBuf> {
    let canonical_base = std::fs::canonicalize(base).map_err(|e| {
        Error::Other(format!(
            "Failed to resolve data directory {}: {}",
            base.display(),
            e
        ))
    })?;
    resolve_under_canonical(&canonical_base, rel)
}

/// Variant of [`resolve_under`] that takes an already-canonicalised base.
/// Use when resolving many paths under the same base to avoid repeated
/// filesystem calls for the base itself.
pub fn resolve_under_canonical(canonical_base: &Path, rel: impl AsRef<Path>) -> Result<PathBuf> {
    let rel = rel.as_ref();

    if rel.as_os_str().is_empty() {
        return Err(Error::Other("Empty relative path".to_string()));
    }
    if rel.is_absolute() {
        return Err(Error::Other(format!(
            "Absolute path not allowed: {}",
            rel.display()
        )));
    }
    if rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Error::Other(format!(
            "Path traversal not allowed: {}",
            rel.display()
        )));
    }

    let joined = canonical_base.join(rel);
    let canonical = std::fs::canonicalize(&joined).map_err(|e| {
        Error::Other(format!(
            "Failed to resolve path {}: {}",
            joined.display(),
            e
        ))
    })?;

    if !canonical.starts_with(canonical_base) {
        return Err(Error::Other(format!(
            "Path {} escapes base {}",
            canonical.display(),
            canonical_base.display()
        )));
    }

    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn resolves_normal_relative_path() {
        let dir = tempdir();
        let base = dir.path();
        fs::create_dir_all(base.join("acct/INBOX/cur")).unwrap();
        let file = base.join("acct/INBOX/cur/1.mail");
        fs::write(&file, b"hello").unwrap();

        let resolved = resolve_under(base, "acct/INBOX/cur/1.mail").unwrap();
        assert_eq!(resolved, fs::canonicalize(&file).unwrap());
    }

    #[test]
    fn rejects_empty_path() {
        let dir = tempdir();
        let err = resolve_under(dir.path(), "").unwrap_err();
        assert!(format!("{}", err).to_lowercase().contains("empty"));
    }

    #[test]
    fn rejects_absolute_path() {
        let dir = tempdir();
        let err = resolve_under(dir.path(), "/etc/passwd").unwrap_err();
        assert!(format!("{}", err).to_lowercase().contains("absolute"));
    }

    #[test]
    fn rejects_parent_traversal_segment() {
        let dir = tempdir();
        let err = resolve_under(dir.path(), "acct/../../etc/passwd").unwrap_err();
        assert!(format!("{}", err).to_lowercase().contains("traversal"));
    }

    #[test]
    fn rejects_nonexistent_file() {
        let dir = tempdir();
        let err = resolve_under(dir.path(), "does/not/exist").unwrap_err();
        assert!(format!("{}", err).to_lowercase().contains("resolve"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let outer = tempdir();
        let inner = tempdir();
        let target = outer.path().join("secret.txt");
        fs::write(&target, b"secret").unwrap();
        let link = inner.path().join("escape");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = resolve_under(inner.path(), "escape").unwrap_err();
        assert!(format!("{}", err).to_lowercase().contains("escape"));
    }

    #[test]
    fn canonical_variant_skips_redundant_base_canonicalisation() {
        let dir = tempdir();
        let canonical_base = fs::canonicalize(dir.path()).unwrap();
        fs::write(canonical_base.join("a.mail"), b"x").unwrap();

        let resolved = resolve_under_canonical(&canonical_base, "a.mail").unwrap();
        assert_eq!(resolved, canonical_base.join("a.mail"));
    }
}
