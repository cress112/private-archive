use super::{
    S3Store, sdk_error,
    upload::{mutate, storage_class, unknown},
};
use archive_domain::*;
use archive_sync::*;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

const COPY_PART: u64 = 128 * 1024 * 1024;
#[derive(Debug, Clone, Copy)]
pub struct CopyPlan {
    pub size: u64,
    pub parts: u32,
}
impl CopyPlan {
    pub fn new(size: u64) -> Result<Self, OperationError> {
        let parts = size.div_ceil(COPY_PART);
        if parts > 10_000 || size > i64::MAX as u64 {
            return Err(OperationError::new(
                ErrorKind::Unsupported,
                "s3-copy-plan",
                "object exceeds fixed multipart copy capacity",
            ));
        }
        Ok(Self {
            size,
            parts: if size <= 16 * 1024 * 1024 {
                0
            } else {
                parts as u32
            },
        })
    }
    pub fn part(&self, number: u32) -> Option<(u64, u64)> {
        if number == 0 || number > self.parts {
            return None;
        }
        let start = u64::from(number - 1) * COPY_PART;
        Some((start, (self.size - start).min(COPY_PART)))
    }
}

pub struct S3RemoteCopy {
    pub(super) store: S3Store,
    pub(super) class: DataClass,
}

// CopySource is a header, not the request URI. Encode once, including literal %,
// while preserving key separators and dot segments verbatim.
fn copy_source(id: &ObjectId) -> String {
    let mut result = String::new();
    for byte in format!("{}/{}", id.authority, id.key).bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~/".contains(&byte) {
            result.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(result, "%{byte:02X}").unwrap();
        }
    }
    result
}

impl RemoteCopy for S3RemoteCopy {
    fn copy<'a>(
        &'a self,
        source: &'a ObjectMetadata,
        destination: &'a Location,
        warnings: &'a dyn WarningSink,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, SaveOutcome> {
        Box::pin(async move {
            let source_id = self.store.id(&source.location, true)?;
            let dest = self.store.id(destination, true)?;
            cancel.check("s3-copy")?;
            if source.kind != EntryKind::File {
                return Err(OperationError::new(
                    ErrorKind::Input,
                    "s3-copy",
                    "copy source must be a file",
                ));
            }
            let plan = CopyPlan::new(source.size.ok_or_else(|| {
                OperationError::new(ErrorKind::Input, "s3-copy", "source size is required")
            })?)?;
            let encoded = copy_source(source_id);
            let result = if plan.parts == 0 {
                mutate(
                    async {
                        let output = self
                            .store
                            .client
                            .copy_object()
                            .bucket(&dest.authority)
                            .key(&dest.key)
                            .metadata_directive(aws_sdk_s3::types::MetadataDirective::Copy)
                            .copy_source(&encoded)
                            .storage_class(storage_class(self.class))
                            .send()
                            .await
                            .map_err(|e| sdk_error("s3-copy", destination, e, true))?;
                        if output
                            .copy_object_result()
                            .and_then(|r| r.e_tag())
                            .is_none_or(str::is_empty)
                        {
                            return Err(unknown(
                                destination,
                                "s3-copy",
                                "copy response has no usable result",
                            ));
                        }
                        Ok(())
                    },
                    cancel,
                    destination,
                )
                .await
            } else {
                self.multipart(plan, &encoded, source, destination, warnings, cancel)
                    .await
            };
            result.map_err(|mut e| {
                e.context.source_uri = Some(source.location.display());
                e
            })?;
            // A confirmed mutation remains confirmed if cancellation arrives now.
            // Verification and diagnostic delivery after commit are bounded.
            let verify = CancelContext::new().with_deadline(
                cancel.ancillary_deadline(Duration::from_secs(5), Duration::from_secs(3)),
            );
            let observed = self
                .store
                .loader()
                .stat(destination, &verify)
                .await
                .map_err(|mut e| {
                    e.outcome = OperationState::Unknown;
                    e.context.source_uri = Some(source.location.display());
                    e.context.destination_uri = Some(destination.display());
                    e
                })?;
            if observed.size != Some(plan.size) {
                let mut error = OperationError::new(
                    ErrorKind::Integrity,
                    "s3-copy-verify",
                    "destination size differs from source",
                );
                error.outcome = OperationState::Failed;
                error.context.source_uri = Some(source.location.display());
                error.context.destination_uri = Some(destination.display());
                return Err(error);
            }
            let comparable = source
                .checksum
                .as_ref()
                .zip(observed.checksum.as_ref())
                .filter(|(a, b)| {
                    a.scope == ChecksumScope::FullObject
                        && b.scope == ChecksumScope::FullObject
                        && a.algorithm == b.algorithm
                });
            let verification = if let Some((expected, actual)) = comparable {
                if expected.value != actual.value {
                    let mut error = OperationError::new(
                        ErrorKind::Integrity,
                        "s3-copy-verify",
                        "destination checksum differs from source",
                    );
                    error.outcome = OperationState::Failed;
                    error.context.source_uri = Some(source.location.display());
                    error.context.destination_uri = Some(destination.display());
                    return Err(error);
                }
                Verification::SizeAndChecksum
            } else {
                let _ = ancillary(
                    cancel,
                    Duration::from_millis(100),
                    Duration::from_secs(3),
                    warnings.warn(Warning {
                        category: WarningCategory::Verification,
                        stage: "s3-copy-verify",
                        uri: destination.display(),
                        code: None,
                        message: "size verified; comparable FULL_OBJECT checksum is unavailable"
                            .into(),
                    }),
                )
                .await;
                Verification::SizeOnly
            };
            for notice in crate::attributes::notices(source, destination) {
                let _ = ancillary(
                    cancel,
                    Duration::from_millis(100),
                    Duration::from_secs(3),
                    warnings.warn(notice),
                )
                .await;
            }
            Ok(SaveOutcome {
                observed: Some(Box::new(observed)),
                destination: destination.clone(),
                bytes: plan.size,
                verification,
            })
        })
    }
}

impl S3RemoteCopy {
    async fn multipart(
        &self,
        plan: CopyPlan,
        encoded: &str,
        source: &ObjectMetadata,
        destination: &Location,
        warnings: &dyn WarningSink,
        cancel: &CancelContext,
    ) -> Result<(), OperationError> {
        let dest = self.store.id(destination, true)?;
        let created = mutate(
            async {
                self.store
                    .client
                    .create_multipart_upload()
                    .set_metadata(Some(crate::attributes::encode(source)))
                    .bucket(&dest.authority)
                    .key(&dest.key)
                    .storage_class(storage_class(self.class))
                    .send()
                    .await
                    .map_err(|e| sdk_error("s3-copy-create", destination, e, true))
            },
            cancel,
            destination,
        )
        .await?;
        let upload = created
            .upload_id()
            .filter(|s| !s.is_empty() && s.len() <= 4096)
            .ok_or_else(|| {
                unknown(
                    destination,
                    "s3-copy-create",
                    "upload ID unavailable; incomplete upload may remain for Lifecycle cleanup",
                )
            })?;
        let mut inflight = crate::parts::PartScheduler::new();
        let failed = std::sync::Arc::new(std::sync::Mutex::new(None::<Instant>));
        let result = async {
            let mut next = 1;
            let mut completed = Vec::with_capacity(plan.parts as usize);
            let mut metadata_bytes = 0;
            loop {
                cancel.check("s3-copy-part")?;
                // Drain completions before scheduling replacements. The shared
                // failure flag prevents a completed failure being overtaken.
                while next <= plan.parts && inflight.len() < 4 && failed.lock().unwrap().is_none() {
                    cancel.check("s3-copy-part")?;
                    let (offset, len) = plan.part(next).unwrap();
                    let number = next;
                    let store = self.store.clone();
                    let part_destination = destination.clone();
                    let encoded = encoded.to_owned();
                    let upload = upload.to_owned();
                    let failed = failed.clone();
                    let operation = cancel.clone();
                    let ready = inflight
                        .schedule(
                            async move {
                                let dest = store.id(&part_destination, true)?;
                                let request = async {
                                    let output = store
                                        .client
                                        .upload_part_copy()
                                        .bucket(&dest.authority)
                                        .key(&dest.key)
                                        .upload_id(upload)
                                        .part_number(number as i32)
                                        .copy_source(encoded)
                                        .copy_source_range(format!(
                                            "bytes={offset}-{}",
                                            offset + len - 1
                                        ))
                                        .send()
                                        .await
                                        .map_err(|e| {
                                            sdk_error("s3-copy-part", &part_destination, e, true)
                                        })?;
                                    let part = output.copy_part_result().ok_or_else(|| {
                                        unknown(
                                            &part_destination,
                                            "s3-copy-part",
                                            "copy part response has no result",
                                        )
                                    })?;
                                    let etag = part.e_tag().filter(|s| !s.is_empty()).ok_or_else(
                                        || {
                                            unknown(
                                                &part_destination,
                                                "s3-copy-part",
                                                "copy part response has no ETag",
                                            )
                                        },
                                    )?;
                                    Ok(CompletedPart::builder()
                                        .part_number(number as i32)
                                        .e_tag(etag)
                                        .build())
                                };
                                let result = tokio::time::timeout(super::API_TIMEOUT, request)
                                    .await
                                    .unwrap_or_else(|_| {
                                        Err(unknown(
                                            &part_destination,
                                            "s3-copy-part",
                                            "copy part timed out",
                                        ))
                                    });
                                if result.is_err() {
                                    operation.record_failure();
                                    failed.lock().unwrap().get_or_insert_with(Instant::now);
                                }
                                result
                            },
                            cancel,
                        )
                        .await?;
                    for result in ready {
                        let part = result.map_err(|_| {
                            unknown(destination, "s3-copy-part", "part task terminated")
                        })??;
                        metadata_bytes +=
                            std::mem::size_of::<CompletedPart>() + part.e_tag().map_or(0, str::len);
                        if metadata_bytes > 4 * 1024 * 1024 {
                            return Err(OperationError::new(
                                ErrorKind::Integrity,
                                "s3-copy-part",
                                "completed metadata exceeds 4MiB",
                            ));
                        }
                        completed.push(part);
                    }
                    next += 1;
                }
                if inflight.is_empty() {
                    break;
                }
                let result = tokio::select! { biased;
                    _ = cancel.cancelled() => { cancel.check("s3-copy-part")?; unreachable!() },
                    result = inflight.join_next() => result.unwrap(),
                };
                let part = result.map_err(|_| {
                    unknown(
                        destination,
                        "s3-copy-part",
                        "copy task terminated without a response",
                    )
                })??;
                metadata_bytes +=
                    std::mem::size_of::<CompletedPart>() + part.e_tag().map_or(0, str::len);
                if metadata_bytes > 4 * 1024 * 1024 {
                    return Err(OperationError::new(
                        ErrorKind::Integrity,
                        "s3-copy-part",
                        "completed metadata exceeds 4MiB",
                    ));
                }
                completed.push(part);
            }
            cancel.check("s3-copy-complete")?;
            inflight.validate_complete(plan.parts, completed.len())?;
            if completed.len() != plan.parts as usize {
                return Err(unknown(
                    destination,
                    "s3-copy-part",
                    "not all copy parts completed",
                ));
            }
            completed.sort_by_key(|p| p.part_number());
            mutate(
                async {
                    let output = self
                        .store
                        .client
                        .complete_multipart_upload()
                        .bucket(&dest.authority)
                        .key(&dest.key)
                        .upload_id(upload)
                        .mpu_object_size(plan.size as i64)
                        .multipart_upload(
                            CompletedMultipartUpload::builder()
                                .set_parts(Some(completed))
                                .build(),
                        )
                        .send()
                        .await
                        .map_err(|e| sdk_error("s3-copy-complete", destination, e, true))?;
                    if output.e_tag().is_none_or(str::is_empty) {
                        return Err(unknown(
                            destination,
                            "s3-copy-complete",
                            "Complete response has no ETag",
                        ));
                    }
                    Ok(())
                },
                cancel,
                destination,
            )
            .await
        }
        .await;
        if result.is_err() {
            cancel.record_failure();
            let stopped = failed
                .lock()
                .unwrap()
                .iter()
                .copied()
                .chain(cancel.stopped_at())
                .min()
                .unwrap_or_else(Instant::now);
            let mut unconfirmed = false;
            while !inflight.is_empty() {
                if timeout_at(stopped + Duration::from_secs(30), inflight.join_next())
                    .await
                    .is_err()
                {
                    unconfirmed = true;
                    inflight.abort_all();
                    while inflight.join_next().await.is_some() {}
                    break;
                }
            }
            let cleanup = timeout_at(
                (Instant::now() + Duration::from_secs(15)).min(stopped + Duration::from_secs(45)),
                self.store
                    .client
                    .abort_multipart_upload()
                    .bucket(&dest.authority)
                    .key(&dest.key)
                    .upload_id(upload)
                    .send(),
            )
            .await;
            if unconfirmed || !matches!(cleanup, Ok(Ok(_))) {
                let message = match cleanup {
                    Ok(Err(e)) => sdk_error("s3-copy-abort", destination, e, true).to_string(),
                    _ => "part or Abort response could not be confirmed".into(),
                };
                let _ = timeout_at(
                    (Instant::now() + Duration::from_millis(100))
                        .min(stopped + Duration::from_secs(50)),
                    warnings.warn(Warning {
                        category: WarningCategory::Cleanup,
                        stage: "s3-copy-abort",
                        uri: destination.display(),
                        code: None,
                        message,
                    }),
                )
                .await;
            }
        }
        result
    }
}
