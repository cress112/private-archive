mod loader;
mod saver;
pub use loader::LocalLoader;
pub use saver::LocalSaver;

use archive_domain::{EntryKind, ErrorKind, Location, ObjectMetadata, OperationError};
use std::path::{Path, PathBuf};

fn local_path(location: &Location) -> Result<&Path, OperationError> {
    match location {
        Location::Local(path) if path.is_absolute() => Ok(path),
        _ => Err(
            OperationError::new(ErrorKind::Input, "local", "expected an absolute local path")
                .at(location),
        ),
    }
}

/// Remove only terminal separators before lstat. An explicitly named symlink
/// must not turn into an ordinary directory just because the input ends in '/'.
fn lstat_path(path: &Path) -> PathBuf {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let bytes = path.as_os_str().as_bytes();
    let end = bytes.iter().rposition(|b| *b != b'/').map_or(1, |i| i + 1);
    std::ffi::OsString::from_vec(bytes[..end.min(bytes.len())].to_vec()).into()
}

fn metadata(location: Location, stat: std::fs::Metadata) -> ObjectMetadata {
    use std::os::unix::fs::PermissionsExt;
    let kind = if stat.is_symlink() {
        EntryKind::Symlink
    } else if stat.is_file() {
        EntryKind::File
    } else if stat.is_dir() {
        EntryKind::Directory
    } else {
        EntryKind::Special
    };
    ObjectMetadata {
        location,
        kind,
        size: (kind == EntryKind::File).then_some(stat.len()),
        checksum: None,
        original_mtime: stat.modified().ok(),
        original_creation_time: stat.created().ok(),
        unix_mode: Some(stat.permissions().mode() & 0o7777),
        data_class: None,
        restore_state: archive_domain::RestoreState::NotRequired,
        queried_at: Some(std::time::SystemTime::now()),
        store_modified: stat.modified().ok(),
        version_token: None,
    }
}

fn unsupported(location: &Location) -> OperationError {
    OperationError::new(
        ErrorKind::Unsupported,
        "local",
        "symlinks and special files are unsupported",
    )
    .at(location)
}

fn worker_error(error: tokio::task::JoinError) -> OperationError {
    OperationError::new(ErrorKind::Io, "local-worker", error.to_string())
}
