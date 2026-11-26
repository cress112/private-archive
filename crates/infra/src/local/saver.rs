use super::{local_path, lstat_path, metadata, unsupported, worker_error};
use crate::checksum::Checksummer;
use archive_domain::{
    EntryKind, ErrorKind, Location, ObjectMetadata, OperationError, OperationState, Warning,
    WarningCategory,
};
use archive_sync::{
    CancelContext, DestinationObservation, LOCAL_BUFFER_BYTES, PortFuture, SaveOutcome, Saver,
    Verification, WarningSink, WriteSession,
};
use tokio::io::AsyncWriteExt;

#[derive(Default)]
pub struct LocalSaver;

impl Saver for LocalSaver {
    fn probe<'a>(
        &'a self,
        destination: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, DestinationObservation> {
        Box::pin(async move {
            cancel.check("probe")?;
            let path = lstat_path(local_path(destination)?);
            let result = match tokio::fs::symlink_metadata(path).await {
                Ok(stat) => {
                    DestinationObservation::Existing(Box::new(metadata(destination.clone(), stat)))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    DestinationObservation::Missing
                }
                Err(e) => return Err(OperationError::io("probe", destination, e)),
            };
            cancel.check("probe")?;
            Ok(result)
        })
    }

    fn begin<'a>(
        &'a self,
        destination: &'a Location,
        source: &'a ObjectMetadata,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Box<dyn WriteSession>> {
        Box::pin(async move {
            cancel.check("begin")?;
            if source.kind != EntryKind::File {
                return Err(unsupported(&source.location));
            }
            let expected_size = source.size.ok_or_else(|| {
                OperationError::new(
                    ErrorKind::Input,
                    "begin",
                    "expected source size is required",
                )
            })?;
            let path = local_path(destination)?;
            if path.file_name().is_none() || path.as_os_str().as_encoded_bytes().ends_with(b"/") {
                return Err(OperationError::new(
                    ErrorKind::Input,
                    "begin",
                    "destination must be a resolved file path",
                )
                .at(destination));
            }
            match self.probe(destination, cancel).await? {
                DestinationObservation::Existing(meta) if meta.kind != EntryKind::File => {
                    return Err(unsupported(destination));
                }
                _ => {}
            }
            let parent = path
                .parent()
                .expect("absolute file has a parent")
                .to_path_buf();
            tokio::fs::create_dir_all(&parent)
                .await
                .map_err(|e| OperationError::io("mkdir", destination, e))?;
            cancel.check("begin")?;
            let target = destination.clone();
            let (file, temp) = tokio::task::spawn_blocking(move || {
                tempfile::Builder::new()
                    .prefix(".private-archive-")
                    .tempfile_in(parent)
                    .map(tempfile::NamedTempFile::into_parts)
                    .map_err(|e| OperationError::io("begin", &target, e))
            })
            .await
            .map_err(worker_error)??;
            let mut file = tokio::fs::File::from_std(file);
            file.set_max_buf_size(LOCAL_BUFFER_BYTES);
            let checksummer = source.checksum.as_ref().and_then(Checksummer::for_expected);
            let mut session = LocalWriteSession {
                destination: destination.clone(),
                attributes: source.clone(),
                file: Some(file),
                temp: Some(temp),
                expected_size,
                expected_checksum: source.checksum.as_ref().map(|v| v.value.clone()),
                checksummer,
                bytes: 0,
                state: State::Active,
            };
            if let Err(error) = cancel.check("begin") {
                // The temporary file may have been created while cancellation
                // arrived. Explicitly close/remove it before returning.
                let _ = session.abort().await;
                return Err(error);
            }
            Ok(Box::new(session) as Box<dyn WriteSession>)
        })
    }
}

#[derive(PartialEq, Eq)]
enum State {
    Active,
    Failed,
    Committed,
    Aborted,
}

struct LocalWriteSession {
    attributes: ObjectMetadata,
    destination: Location,
    file: Option<tokio::fs::File>,
    temp: Option<tempfile::TempPath>,
    expected_size: u64,
    expected_checksum: Option<Vec<u8>>,
    checksummer: Option<Checksummer>,
    bytes: u64,
    state: State,
}

impl LocalWriteSession {
    fn active(&self) -> Result<(), OperationError> {
        if self.state == State::Active {
            Ok(())
        } else {
            Err(OperationError::new(
                ErrorKind::Input,
                "session",
                "write session is no longer active",
            )
            .at(&self.destination))
        }
    }
    fn integrity(&self, message: &str) -> OperationError {
        let mut error =
            OperationError::new(ErrorKind::Integrity, "verify", message).at(&self.destination);
        error.outcome = OperationState::Failed;
        error
    }
}

impl WriteSession for LocalWriteSession {
    fn destination(&self) -> &Location {
        &self.destination
    }
    fn write<'a>(&'a mut self, chunk: &'a [u8], cancel: &'a CancelContext) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.active()?;
            // Any failure is terminal, including cancellation during a write.
            self.state = State::Failed;
            cancel.check("write")?;
            if chunk.len() > LOCAL_BUFFER_BYTES {
                return Err(OperationError::new(
                    ErrorKind::Input,
                    "write",
                    "local chunk exceeds the 1MiB I/O limit",
                ));
            }
            let bytes = self
                .bytes
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| self.integrity("byte count overflow"))?;
            if bytes > self.expected_size {
                return Err(self.integrity("body exceeds the expected size"));
            }
            self.file
                .as_mut()
                .expect("active session has a file")
                .write_all(chunk)
                .await
                .map_err(|e| OperationError::io("write", &self.destination, e))?;
            if let Some(checksummer) = &mut self.checksummer {
                checksummer.update(chunk);
            }
            self.bytes = bytes;
            cancel.check("write")?;
            self.state = State::Active;
            Ok(())
        })
    }

    fn finish<'a>(
        &'a mut self,
        body_complete: bool,
        warnings: &'a dyn WarningSink,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, SaveOutcome> {
        Box::pin(async move {
            self.active()?;
            self.state = State::Failed;
            cancel.check("finish")?;
            if !body_complete {
                return Err(self.integrity("body EOF has not been observed"));
            }
            if self.bytes != self.expected_size {
                return Err(self.integrity("body is shorter than the expected size"));
            }
            let verification = if let Some(checksummer) = self.checksummer.take() {
                if self.expected_checksum.as_deref() != Some(checksummer.finish().as_slice()) {
                    return Err(self.integrity("body checksum mismatch"));
                }
                Verification::SizeAndChecksum
            } else {
                let message = if self.expected_checksum.is_some() {
                    "checksum algorithm or scope cannot be verified; size and EOF verified"
                } else {
                    "source has no checksum; size and EOF verified"
                };
                let warning = Warning {
                    category: WarningCategory::Verification,
                    stage: "verify",
                    uri: self.destination.display(),
                    code: None,
                    message: message.into(),
                };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(OperationError::new(ErrorKind::Cancelled, "verify", "operation cancelled")),
                    result = warnings.warn(warning) => result?,
                }
                Verification::SizeOnly
            };
            let file = self.file.as_mut().expect("active session has a file");
            file.flush()
                .await
                .map_err(|e| OperationError::io("flush", &self.destination, e))?;
            cancel.check("finish")?;
            // Drain Tokio's pending blocking I/O and close the handle before
            // publishing. No unverified data ever touches the destination file.
            let file = self.file.take().unwrap().into_std().await;
            let attributes = self.attributes.clone();
            cancel.check("finish")?;
            let temp = self.temp.take().expect("active session has a temp path");
            let destination = self.destination.clone();
            let commit_cancel = cancel.clone();
            let (result, remaining, notices) = tokio::task::spawn_blocking(move || {
                let notices = crate::attributes::restore(&file, &attributes, &destination);
                drop(file);
                let commit = || -> Result<(), OperationError> {
                    commit_cancel.check("commit")?;
                    let path = local_path(&destination)?;
                    match std::fs::symlink_metadata(path) {
                        Ok(stat) if !stat.is_file() || stat.is_symlink() => {
                            Err(unsupported(&destination))
                        }
                        Ok(_) => Ok(()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(e) => Err(OperationError::io("commit", &destination, e)),
                    }
                };
                if let Err(error) = commit() {
                    return (Err(error), Some(temp), notices);
                }
                match temp.persist(local_path(&destination).unwrap()) {
                    Ok(()) => (Ok(()), None, notices),
                    Err(error) => (
                        Err(OperationError::io("commit", &destination, error.error)),
                        Some(error.path),
                        notices,
                    ),
                }
            })
            .await
            .map_err(worker_error)?;
            self.temp = remaining;
            result?;
            self.state = State::Committed;
            for notice in notices {
                let _ = archive_sync::ancillary(
                    cancel,
                    std::time::Duration::from_millis(100),
                    std::time::Duration::from_secs(3),
                    warnings.warn(notice),
                )
                .await;
            }
            // Cancellation after a confirmed rename cannot undo that success.
            Ok(SaveOutcome {
                observed: None,
                destination: self.destination.clone(),
                bytes: self.bytes,
                verification,
            })
        })
    }

    fn abort<'a>(&'a mut self) -> PortFuture<'a, ()> {
        Box::pin(async move {
            if self.state == State::Committed || self.state == State::Aborted {
                return Ok(());
            }
            self.state = State::Failed;
            // flush/into_std wait for any outstanding writes before removing the
            // temp path. Cleanup runs even when the operation was cancelled.
            let flush_error = if let Some(mut file) = self.file.take() {
                let error = file.flush().await.err();
                drop(file.into_std().await);
                error
            } else {
                None
            };
            let remove_result = if let Some(temp) = self.temp.take() {
                let destination = self.destination.clone();
                tokio::task::spawn_blocking(move || {
                    temp.close()
                        .map_err(|e| OperationError::io("abort", &destination, e))
                })
                .await
                .map_err(worker_error)?
            } else {
                Ok(())
            };
            self.state = State::Aborted;
            remove_result?;
            if let Some(error) = flush_error {
                return Err(OperationError::io("abort", &self.destination, error));
            }
            Ok(())
        })
    }
}
