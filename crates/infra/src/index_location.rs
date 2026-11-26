//! Maps a file location to its local query copy, without opening a store/client.
use archive_domain::{ErrorKind, Location, OperationError};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub fn index_working_path(home: &Path, location: &Location, namespace: &str) -> PathBuf {
    match location {
        Location::Local(path) => path.clone(),
        Location::Remote(id) => {
            let mut hash = Sha256::new();
            hash.update(namespace.as_bytes());
            hash.update([0]);
            hash.update(id.uri().as_bytes());
            home.join(".private-archive/cache/index")
                .join(format!("{:x}", hash.finalize()))
                .join("working.db")
        }
    }
}

pub fn prepare_index_path(
    home: &Path,
    location: &Location,
    namespace: &str,
) -> Result<PathBuf, OperationError> {
    let path = index_working_path(home, location, namespace);
    if matches!(location, Location::Remote(_)) {
        let parent = path.parent().unwrap();
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent).map_err(|e| {
            OperationError::new(ErrorKind::Io, "index-cache-directory", e.to_string())
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| OperationError::new(ErrorKind::Io, "index-cache-directory", e.to_string()),
            )?;
        }
    }
    Ok(path)
}
