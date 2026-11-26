//! ETags and immutable transfer-cache files belong to this adapter only.
use super::{IDLE_TIMEOUT, S3Store, read_request, sdk_error, upload::mutate};
use archive_domain::*;
use archive_index::SnapshotFile;
use archive_sync::*;
use aws_sdk_s3::{primitives::ByteStream, types::StorageClass};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) struct S3IndexFiles {
    store: S3Store,
    location: Location,
    cache: PathBuf,
}
impl S3IndexFiles {
    pub(super) fn new(store: S3Store, location: Location, cache: PathBuf) -> Self {
        Self {
            store,
            location,
            cache,
        }
    }
}
struct File(tempfile::NamedTempFile);
impl SnapshotFile for File {
    fn path(&self) -> &Path {
        self.0.path()
    }
}
enum Baseline {
    Unknown,
    Missing,
    Version(String),
}
struct Session {
    store: S3Store,
    location: Location,
    cache: PathBuf,
    loaded: bool,
    attempted: bool,
    baseline: Baseline,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    snapshot: String,
    etag: String,
    sha256: String,
}
fn io(e: impl std::fmt::Display) -> OperationError {
    OperationError::new(ErrorKind::Io, "index-file", e.to_string())
}
fn cache_dir(path: &Path) -> Result<(), OperationError> {
    std::fs::create_dir_all(path).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    Ok(())
}
/// Copy to a private owned file; never hand a mutable working DB to the cache.
async fn copy_file(
    path: &Path,
    parent: &Path,
    cancel: &CancelContext,
) -> Result<(File, String), OperationError> {
    let file = tempfile::NamedTempFile::new_in(parent).map_err(io)?;
    let mut input = tokio::fs::File::open(path).await.map_err(io)?;
    let mut output = tokio::fs::File::from_std(file.reopen().map_err(io)?);
    let mut buffer = vec![0; LOCAL_BUFFER_BYTES];
    let mut hash = Sha256::new();
    loop {
        cancel.check("index-file-copy")?;
        let n = tokio::select! {
            _ = cancel.cancelled() => { cancel.check("index-file-copy")?; unreachable!() },
            n = input.read(&mut buffer) => n.map_err(io)?,
        };
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        tokio::select! {
            _ = cancel.cancelled() => { cancel.check("index-file-copy")?; unreachable!() },
            r = output.write_all(&buffer[..n]) => r.map_err(io)?,
        }
    }
    output.sync_all().await.map_err(io)?;
    Ok((File(file), format!("{:x}", hash.finalize())))
}
async fn cached(parent: &Path, cancel: &CancelContext) -> Option<(File, String)> {
    // Invalid/missing optimization metadata is not an authority for DB contents.
    let path = parent.join("transfer.json");
    let length = std::fs::metadata(&path).ok()?.len();
    if length > 16 * 1024 {
        return None;
    }
    let record: Cache = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if !record.snapshot.starts_with("snapshot-")
        || record.snapshot.contains(['/', '\\'])
        || record.etag.is_empty()
    {
        return None;
    }
    let (file, hash) = copy_file(&parent.join(&record.snapshot), parent, cancel)
        .await
        .ok()?;
    (hash == record.sha256).then_some((file, record.etag))
}
async fn publish(
    parent: &Path,
    file: &dyn SnapshotFile,
    etag: &str,
    cancel: &CancelContext,
) -> Result<(), OperationError> {
    let (copy, sha256) = copy_file(file.path(), parent, cancel).await?;
    // Serialize only the tiny publication/retirement section, never copying,
    // DB work or network I/O. Busy publication is an optional cache miss, so
    // do not wait or schedule work for a later invocation.
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let publication = options.open(parent.join("transfer.lock")).map_err(io)?;
    match publication.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(error) => return Err(io(error)),
    }
    // Read the current manifest under the same lock used to replace it, or a
    // concurrent publisher's snapshot could be orphaned by this replacement.
    let old = std::fs::read(parent.join("transfer.json"))
        .ok()
        .filter(|b| b.len() <= 16 * 1024)
        .and_then(|b| serde_json::from_slice::<Cache>(&b).ok());
    let File(copy) = copy;
    // tempfile's randomized names prevent concurrent publishers sharing a file.
    let name = format!(
        "snapshot-{}",
        copy.path().file_name().unwrap().to_string_lossy()
    );
    let dest = parent.join(&name);
    copy.persist(&dest).map_err(io)?;
    let result = (|| {
        use std::io::Write;
        let mut manifest = tempfile::NamedTempFile::new_in(parent).map_err(io)?;
        serde_json::to_writer(
            &mut manifest,
            &Cache {
                snapshot: name,
                etag: etag.into(),
                sha256,
            },
        )
        .map_err(io)?;
        manifest.flush().map_err(io)?;
        manifest.as_file().sync_all().map_err(io)?;
        manifest.persist(parent.join("transfer.json")).map_err(io)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&dest);
    }
    if result.is_ok()
        && let Some(old) = old
        && old.snapshot.starts_with("snapshot-")
        && !old.snapshot.contains(['/', '\\'])
    {
        // Concurrent readers own private copies. A reader racing before copying
        // falls back to a normal GET rather than pairing a different file/ETag.
        let _ = std::fs::remove_file(parent.join(old.snapshot));
    }
    result
}
impl IndexFileStore for S3IndexFiles {
    fn open<'a>(&'a self, cancel: &'a CancelContext) -> PortFuture<'a, Box<dyn IndexFileSession>> {
        Box::pin(async move {
            cancel.check("index-file-open")?;
            cache_dir(&self.cache)?;
            Ok(Box::new(Session {
                store: self.store.clone(),
                location: self.location.clone(),
                cache: self.cache.clone(),
                loaded: false,
                attempted: false,
                baseline: Baseline::Unknown,
            }) as Box<dyn IndexFileSession>)
        })
    }
}
impl IndexFileSession for Session {
    fn load<'a>(
        &'a mut self,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Option<Box<dyn SnapshotFile>>> {
        Box::pin(async move {
            if self.loaded {
                return Err(io("file session already loaded"));
            }
            self.loaded = true;
            let cached = cached(&self.cache, cancel).await;
            let id = self.store.id(&self.location, true)?;
            enum Fetched {
                File(Box<aws_sdk_s3::operation::get_object::GetObjectOutput>),
                NotModified,
                Missing,
            }
            let response = read_request(
                async {
                    match self
                        .store
                        .client
                        .get_object()
                        .bucket(&id.authority)
                        .key(&id.key)
                        .set_if_none_match(cached.as_ref().map(|(_, etag)| etag.clone()))
                        .send()
                        .await
                    {
                        Ok(output) => Ok(Fetched::File(Box::new(output))),
                        Err(e) => match e.raw_response().map(|r| r.status().as_u16()) {
                            Some(304) => Ok(Fetched::NotModified),
                            Some(404) => Ok(Fetched::Missing),
                            _ => Err(sdk_error("index-file-get", &self.location, e, false)),
                        },
                    }
                },
                cancel,
            )
            .await?;
            // Inspect protocol status before reducing the SDK error to a domain
            // error: 304 reuses only the owned immutable snapshot above.
            let mut output = match response {
                Fetched::File(output) => output,
                Fetched::Missing => {
                    self.baseline = Baseline::Missing;
                    return Ok(None);
                }
                Fetched::NotModified => {
                    let (file, etag) =
                        cached.ok_or_else(|| io("304 without a matching immutable snapshot"))?;
                    self.baseline = Baseline::Version(etag);
                    return Ok(Some(Box::new(file) as Box<dyn SnapshotFile>));
                }
            };
            let etag = output
                .e_tag()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| io("index response has no ETag"))?
                .to_owned();
            let length = output
                .content_length()
                .filter(|n| *n >= 0)
                .ok_or_else(|| io("index response has no valid length"))?
                as u64;
            let file = File(tempfile::NamedTempFile::new_in(&self.cache).map_err(io)?);
            let mut target = tokio::fs::File::from_std(file.0.reopen().map_err(io)?);
            read_request(
                async {
                    let mut received = 0u64;
                    while let Some(bytes) =
                        tokio::time::timeout(IDLE_TIMEOUT, output.body.try_next())
                            .await
                            .map_err(|_| io("index download stalled"))?
                            .map_err(io)?
                    {
                        received = received
                            .checked_add(bytes.len() as u64)
                            .ok_or_else(|| io("index length overflow"))?;
                        if received > length {
                            return Err(io("index download exceeds declared length"));
                        }
                        target.write_all(&bytes).await.map_err(io)?;
                    }
                    if received != length {
                        return Err(io("incomplete index download"));
                    }
                    target.sync_all().await.map_err(io)
                },
                cancel,
            )
            .await?;
            publish(&self.cache, &file, &etag, cancel).await?;
            self.baseline = Baseline::Version(etag);
            Ok(Some(Box::new(file) as Box<dyn SnapshotFile>))
        })
    }
    fn save<'a>(
        &'a mut self,
        snapshot: &'a dyn SnapshotFile,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ()> {
        Box::pin(async move {
            if self.attempted {
                return Err(io("conditional save already attempted"));
            }
            self.attempted = true;
            if matches!(self.baseline, Baseline::Unknown) {
                return Err(io("no confirmed index baseline; conditional save refused"));
            }
            let length = tokio::fs::metadata(snapshot.path())
                .await
                .map_err(io)?
                .len();
            if length > 5 * 1024 * 1024 * 1024 {
                return Err(OperationError::new(
                    ErrorKind::Unsupported,
                    "index-file-save",
                    "index exceeds the single-request S3 upload limit",
                ));
            }
            let body = ByteStream::read_from()
                .path(snapshot.path())
                .build()
                .await
                .map_err(io)?;
            let id = self.store.id(&self.location, true)?;
            let mut request = self
                .store
                .client
                .put_object()
                .bucket(&id.authority)
                .key(&id.key)
                .storage_class(StorageClass::Standard)
                .content_length(length as i64)
                .body(body);
            request = match &self.baseline {
                Baseline::Version(etag) => request.if_match(etag),
                Baseline::Missing => request.if_none_match("*"),
                Baseline::Unknown => unreachable!(),
            };
            let output = mutate(
                async {
                    request
                        .send()
                        .await
                        .map_err(|e| sdk_error("index-file-put", &self.location, e, true))
                },
                cancel,
                &self.location,
            )
            .await?;
            let etag = output
                .e_tag()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| io("index saved but response has no ETag; cache not published"))?;
            publish(&self.cache, snapshot, etag, cancel).await
        })
    }
    fn close<'a>(&'a mut self) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.loaded = true;
            self.attempted = true;
            Ok(())
        })
    }
}
