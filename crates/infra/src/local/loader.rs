use super::{local_path, lstat_path, metadata, unsupported, worker_error};
use archive_domain::{
    EntryKind, ErrorKind, Location, ObjectMetadata, OperationError, Warning, WarningCategory,
};
use archive_sync::{
    CancelContext, EntryCursor, EnumerationItem, IgnoreFilter, LOCAL_BUFFER_BYTES, Loader,
    OpenedSource, PortFuture, Selection, SelectionMode,
};

#[derive(Default)]
pub struct LocalLoader;

impl Loader for LocalLoader {
    fn stat<'a>(
        &'a self,
        location: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ObjectMetadata> {
        Box::pin(async move {
            cancel.check("stat")?;
            let path = lstat_path(local_path(location)?);
            let stat = tokio::fs::symlink_metadata(path)
                .await
                .map_err(|e| OperationError::io("stat", location, e))?;
            cancel.check("stat")?;
            Ok(metadata(location.clone(), stat))
        })
    }

    fn enumerate<'a>(
        &'a self,
        selection: &'a Selection,
        ignore: &'a IgnoreFilter,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Box<dyn EntryCursor>> {
        Box::pin(async move {
            let root = self.stat(&selection.root, cancel).await?;
            let mut cursor = LocalCursor {
                first: None,
                stack: vec![],
                pending: None,
                max_depth: selection.max_depth.map(|n| n.get()),
                ignore: ignore.clone(),
                failed: false,
            };
            if ignore.is_ignored(&root.location, root.kind == EntryKind::Directory)? {
                cursor.first = Some(skip(&root.location));
            } else if selection.mode == SelectionMode::Exact {
                cursor.first = Some(EnumerationItem::Entry(Box::new(root)));
            } else if root.kind != EntryKind::Directory {
                return Err(
                    if matches!(root.kind, EntryKind::Symlink | EntryKind::Special) {
                        unsupported(&root.location)
                    } else {
                        OperationError::new(
                            ErrorKind::Input,
                            "enumerate",
                            "recursive local source must be a directory",
                        )
                        .at(&root.location)
                    },
                );
            } else {
                cursor.pending = Some((local_path(&root.location)?.to_path_buf(), 0));
            }
            Ok(Box::new(cursor) as Box<dyn EntryCursor>)
        })
    }

    fn open<'a>(
        &'a self,
        location: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, OpenedSource> {
        Box::pin(async move {
            cancel.check("open")?;
            let path = lstat_path(local_path(location)?);
            let source = location.clone();
            // O_NOFOLLOW prevents symlink replacement; O_NONBLOCK prevents a
            // replaced FIFO from hanging before the descriptor can be checked.
            let (file, stat) = tokio::task::spawn_blocking(move || {
                use std::os::unix::fs::OpenOptionsExt;
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(path)
                    .map_err(|e| OperationError::io("open", &source, e))?;
                let stat = file
                    .metadata()
                    .map_err(|e| OperationError::io("open", &source, e))?;
                if !stat.is_file() {
                    return Err(unsupported(&source));
                }
                Ok((file, stat))
            })
            .await
            .map_err(worker_error)??;
            cancel.check("open")?;
            let metadata = metadata(location.clone(), stat);
            let mut file = tokio::fs::File::from_std(file);
            file.set_max_buf_size(LOCAL_BUFFER_BYTES);
            Ok(OpenedSource {
                metadata,
                body: Box::pin(file),
            })
        })
    }
}

struct Frame {
    directory: tokio::fs::ReadDir,
    location: Location,
    depth: u32,
}
struct LocalCursor {
    first: Option<EnumerationItem>,
    stack: Vec<Frame>,
    pending: Option<(std::path::PathBuf, u32)>,
    max_depth: Option<u32>,
    ignore: IgnoreFilter,
    failed: bool,
}

impl LocalCursor {
    async fn advance(
        &mut self,
        cancel: &CancelContext,
    ) -> Result<Option<EnumerationItem>, OperationError> {
        cancel.check("enumerate")?;
        if let Some(item) = self.first.take() {
            return Ok(Some(item));
        }
        loop {
            cancel.check("enumerate")?;
            if let Some((path, depth)) = self.pending.take() {
                let location = Location::Local(path.clone());
                let stat = tokio::fs::symlink_metadata(lstat_path(&path))
                    .await
                    .map_err(|e| OperationError::io("enumerate", &location, e))?;
                if !stat.is_dir() || stat.is_symlink() {
                    return Err(unsupported(&location));
                }
                let directory = tokio::fs::read_dir(path)
                    .await
                    .map_err(|e| OperationError::io("enumerate", &location, e))?;
                self.stack.push(Frame {
                    directory,
                    location,
                    depth,
                });
            }
            let Some(frame) = self.stack.last_mut() else {
                return Ok(None);
            };
            let Some(entry) = frame
                .directory
                .next_entry()
                .await
                .map_err(|e| OperationError::io("enumerate", &frame.location, e))?
            else {
                self.stack.pop();
                continue;
            };
            let depth = frame.depth.checked_add(1).ok_or_else(|| {
                OperationError::new(
                    ErrorKind::Unsupported,
                    "enumerate",
                    "directory depth overflow",
                )
            })?;
            let location = Location::Local(entry.path());
            // symlink_metadata never follows an ignored or unsupported entry.
            let stat = tokio::fs::symlink_metadata(entry.path())
                .await
                .map_err(|e| OperationError::io("enumerate", &location, e))?;
            let observed = metadata(location.clone(), stat);
            if self
                .ignore
                .is_ignored(&location, observed.kind == EntryKind::Directory)?
            {
                return Ok(Some(skip(&location)));
            }
            if observed.kind == EntryKind::Directory {
                if self.max_depth.is_none_or(|max| depth < max) {
                    self.pending = Some((entry.path(), depth));
                }
                continue;
            }
            return Ok(Some(EnumerationItem::Entry(Box::new(observed))));
        }
    }
}

impl EntryCursor for LocalCursor {
    fn next<'a>(
        &'a mut self,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Option<EnumerationItem>> {
        Box::pin(async move {
            if self.failed {
                return Ok(None);
            }
            let result = self.advance(cancel).await;
            if result.is_err() {
                self.failed = true;
                self.stack.clear();
                self.pending = None;
            }
            result
        })
    }
}

fn skip(location: &Location) -> EnumerationItem {
    EnumerationItem::Skipped(Warning {
        category: WarningCategory::Ignored,
        stage: "enumerate",
        uri: location.display(),
        code: None,
        message: "skipped by ignore pattern".into(),
    })
}
