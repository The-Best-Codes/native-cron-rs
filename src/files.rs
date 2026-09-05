use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::Result;

/// Returns true if a path exists (following symlinks), without erroring if it does not.
pub fn path_exists(path: &Path) -> bool {
    path.try_exists().unwrap_or(false)
}

/// Writes `contents` to `path` atomically: the data is written to a sibling
/// temporary file and then renamed into place, so readers never observe a
/// partially written file. Parent directories are created as needed, and on
/// Unix the resulting file is given user-only `0600` permissions.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)?;

    let unique = std::process::id().to_string() + "-" + &unique_suffix();
    let temp_path = directory.join(format!(
        "{}.{unique}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("native-cron")
    ));

    fs::write(&temp_path, contents)?;
    set_owner_only_permissions(&temp_path)?;

    match fs::rename(&temp_path, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Some platforms (notably Windows) cannot rename over an existing file.
            if is_rename_conflict(&error) {
                fs::remove_file(path).ok();
                fs::rename(&temp_path, path)?;
                Ok(())
            } else {
                fs::remove_file(&temp_path).ok();
                Err(error.into())
            }
        }
    }
}

fn is_rename_conflict(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
    )
}

fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// Creates the parent directory of `path`, if any, so an output file can be
/// written there.
pub fn ensure_output_directory(path: Option<&Path>) -> Result<()> {
    if let Some(path) = path {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// Resolves `path` relative to `base` unless it is already absolute.
pub fn resolve_from(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}
