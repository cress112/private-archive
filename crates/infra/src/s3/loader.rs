use super::{IDLE_TIMEOUT, S3Store, metadata, read_request, sdk_error};
use archive_domain::*;
use archive_sync::*;
use aws_sdk_s3::types::{ChecksumMode, OptionalObjectAttributes};
use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, ReadBuf};

pub struct S3Loader {
    pub(super) store: S3Store,
}
impl Loader for S3Loader {
    fn stat<'a>(
        &'a self,
        location: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ObjectMetadata> {
        Box::pin(async move {
            let id = self.store.id(location, true)?;
            let output = read_request(
                async {
                    self.store
                        .client
                        .head_object()
                        .bucket(&id.authority)
                        .key(&id.key)
                        .checksum_mode(ChecksumMode::Enabled)
                        .send()
                        .await
                        .map_err(|e| sdk_error("s3-head", location, e, false))
                },
                cancel,
            )
            .await?;
            metadata::head(location, &output)
        })
    }

    fn open<'a>(
        &'a self,
        location: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, OpenedSource> {
        Box::pin(async move {
            let id = self.store.id(location, true)?;
            let output = read_request(
                async {
                    self.store
                        .client
                        .get_object()
                        .bucket(&id.authority)
                        .key(&id.key)
                        .checksum_mode(ChecksumMode::Enabled)
                        .send()
                        .await
                        .map_err(|e| sdk_error("s3-get", location, e, false))
                },
                cancel,
            )
            .await?;
            let metadata = metadata::get(location, &output)?;
            let body = IdleReader {
                inner: Box::pin(output.body.into_async_read()),
                timer: Box::pin(tokio::time::sleep(IDLE_TIMEOUT)),
                cancelled: Box::pin({
                    let cancel = cancel.clone();
                    async move { cancel.cancelled().await }
                }),
            };
            Ok(OpenedSource {
                metadata,
                body: Box::pin(body),
            })
        })
    }

    fn enumerate<'a>(
        &'a self,
        selection: &'a Selection,
        ignore: &'a IgnoreFilter,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Box<dyn EntryCursor>> {
        Box::pin(async move {
            self.store
                .id(&selection.root, selection.mode == SelectionMode::Exact)?;
            cancel.check("s3-list")?;
            Ok(Box::new(S3Cursor {
                store: self.store.clone(),
                selection: selection.clone(),
                ignore: ignore.clone(),
                page: VecDeque::new(),
                page_queried_at: None,
                continuation: None,
                done: false,
                failed: false,
            }) as Box<dyn EntryCursor>)
        })
    }
}

struct S3Cursor {
    store: S3Store,
    selection: Selection,
    ignore: IgnoreFilter,
    page: VecDeque<aws_sdk_s3::types::Object>,
    page_queried_at: Option<std::time::SystemTime>,
    continuation: Option<String>,
    done: bool,
    failed: bool,
}
impl S3Cursor {
    async fn advance(
        &mut self,
        cancel: &CancelContext,
    ) -> Result<Option<EnumerationItem>, OperationError> {
        cancel.check("s3-list")?;
        if self.selection.mode == SelectionMode::Exact {
            if self.done {
                return Ok(None);
            }
            self.done = true;
            if self.ignore.is_ignored(&self.selection.root, false)? {
                return Ok(Some(skipped(&self.selection.root)));
            }
            return self
                .store
                .loader()
                .stat(&self.selection.root, cancel)
                .await
                .map(|m| Some(EnumerationItem::Entry(Box::new(m))));
        }
        loop {
            cancel.check("s3-list")?;
            if let Some(object) = self.page.pop_front() {
                let key = object.key().ok_or_else(|| {
                    OperationError::new(ErrorKind::Integrity, "s3-list", "LIST object has no key")
                })?;
                let Location::Remote(root) = &self.selection.root else {
                    unreachable!()
                };
                let location = Location::Remote(root.with_key(key.to_owned()));
                if !self.selection.contains(&location) {
                    continue;
                }
                if self.ignore.is_ignored(&location, false)? {
                    return Ok(Some(skipped(&location)));
                }
                let mut meta = ObjectMetadata::file(
                    location.clone(),
                    metadata::size(object.size(), &location)?,
                );
                metadata::decorate(
                    &mut meta,
                    object.storage_class().map(|v| v.as_str()),
                    None,
                    object.last_modified(),
                    object.e_tag(),
                );
                if object.storage_class().is_none() {
                    meta.data_class = None;
                    meta.restore_state = RestoreState::Unknown;
                }
                if let Some(status) = object.restore_status() {
                    if status.is_restore_in_progress() == Some(true) {
                        meta.restore_state = RestoreState::Restoring;
                    } else if status.is_restore_in_progress() == Some(false)
                        || status.restore_expiry_date().is_some()
                    {
                        meta.restore_state = RestoreState::Restored {
                            until: metadata::timestamp(status.restore_expiry_date()),
                        };
                    } else {
                        meta.restore_state = RestoreState::Unknown;
                    }
                }
                meta.queried_at = self.page_queried_at;
                return Ok(Some(EnumerationItem::Entry(Box::new(meta))));
            }
            if self.done {
                return Ok(None);
            }
            let id = self.store.id(&self.selection.root, false)?;
            let output = read_request(
                async {
                    self.store
                        .client
                        .list_objects_v2()
                        .bucket(&id.authority)
                        .prefix(&id.key)
                        .max_keys(1000)
                        .optional_object_attributes(OptionalObjectAttributes::RestoreStatus)
                        .set_continuation_token(self.continuation.clone())
                        .send()
                        .await
                        .map_err(|e| sdk_error("s3-list", &self.selection.root, e, false))
                },
                cancel,
            )
            .await?;
            self.done = output.is_truncated() != Some(true);
            self.page_queried_at = Some(std::time::SystemTime::now());
            let next = output.next_continuation_token().map(str::to_owned);
            if !self.done && (next.is_none() || next == self.continuation) {
                return Err(OperationError::new(
                    ErrorKind::Integrity,
                    "s3-list",
                    "truncated LIST has no advancing continuation token",
                ));
            }
            self.continuation = next;
            self.page = output.contents.unwrap_or_default().into();
            if self.page.len() > 1000 {
                return Err(OperationError::new(
                    ErrorKind::Integrity,
                    "s3-list",
                    "LIST exceeded requested page size",
                ));
            }
        }
    }
}
impl EntryCursor for S3Cursor {
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
                self.page.clear();
            }
            result
        })
    }
}
fn skipped(location: &Location) -> EnumerationItem {
    EnumerationItem::Skipped(Warning {
        category: WarningCategory::Ignored,
        stage: "s3-list",
        uri: location.display(),
        code: None,
        message: "skipped by ignore pattern".into(),
    })
}

struct IdleReader {
    inner: BodyReader,
    timer: Pin<Box<tokio::time::Sleep>>,
    cancelled: Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
}
impl AsyncRead for IdleReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "S3 read cancelled",
            )));
        }
        let before = output.filled().len();
        let result = this.inner.as_mut().poll_read(cx, output);
        if output.filled().len() > before {
            this.timer
                .as_mut()
                .reset(tokio::time::Instant::now() + IDLE_TIMEOUT);
        }
        if result.is_pending() && this.timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "S3 body made no progress for 120 seconds",
            )));
        }
        result
    }
}
