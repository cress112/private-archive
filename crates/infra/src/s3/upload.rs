use super::S3Store;
use super::{API_TIMEOUT, sdk_error};
use archive_domain::*;
use archive_sync::*;
use aws_sdk_s3::types::{
    ChecksumAlgorithm as SdkAlgorithm, ChecksumType, CompletedMultipartUpload, CompletedPart,
    StorageClass,
};
use base64::Engine;
use bytes::BytesMut;
use std::{future::Future, pin::Pin, time::Duration};
use tokio::{
    sync::watch,
    time::{Instant, timeout_at},
};

pub struct S3Saver {
    pub(super) store: S3Store,
    pub(super) class: DataClass,
}

#[derive(Debug, Clone, Copy)]
pub struct UploadPlan {
    pub size: u64,
    pub parts: u32,
}
pub(super) const PART_SIZE: usize = 16 * 1024 * 1024;
static CRC32: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
impl UploadPlan {
    pub fn new(size: u64) -> Result<Self, OperationError> {
        let parts = size.div_ceil(PART_SIZE as u64);
        if parts > 10_000 || size > i64::MAX as u64 {
            return Err(OperationError::new(
                ErrorKind::Unsupported,
                "s3-plan",
                "object exceeds fixed multipart capacity",
            ));
        }
        Ok(Self {
            size,
            parts: if size <= PART_SIZE as u64 {
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
        let offset = u64::from(number - 1) * PART_SIZE as u64;
        Some((offset, (self.size - offset).min(PART_SIZE as u64)))
    }
}

impl Saver for S3Saver {
    fn probe<'a>(
        &'a self,
        destination: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, DestinationObservation> {
        Box::pin(async move {
            match self.store.loader().stat(destination, cancel).await {
                Ok(meta) => Ok(DestinationObservation::Existing(Box::new(meta))),
                Err(e) if e.kind == ErrorKind::NotFound => Ok(DestinationObservation::Missing),
                Err(e) => Err(e),
            }
        })
    }

    fn begin<'a>(
        &'a self,
        destination: &'a Location,
        source: &'a ObjectMetadata,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Box<dyn WriteSession>> {
        Box::pin(async move {
            let id = self.store.id(destination, true)?;
            cancel.check("s3-begin")?;
            if source.kind != EntryKind::File {
                return Err(OperationError::new(
                    ErrorKind::Unsupported,
                    "s3-begin",
                    "source must be an ordinary file",
                ));
            }
            let plan = UploadPlan::new(source.size.ok_or_else(|| {
                OperationError::new(ErrorKind::Input, "s3-begin", "source size is required")
            })?)?;
            let attributes = crate::attributes::encode(source);
            let upload_id = if plan.parts > 0 {
                let response = mutate(async {
                    self.store.client.create_multipart_upload().bucket(&id.authority).key(&id.key)
                        .set_metadata(Some(attributes.clone())).storage_class(storage_class(self.class)).checksum_algorithm(SdkAlgorithm::Crc32).checksum_type(ChecksumType::FullObject)
                        .send().await.map_err(|e| sdk_error("s3-create-upload", destination, e, true))
                }, cancel, destination).await.map_err(|mut e| { if e.outcome == OperationState::Unknown { e.context.causes.push("upload ID unavailable; incomplete upload may remain for Lifecycle cleanup".into()); } e })?;
                Some(
                    response
                        .upload_id()
                        .filter(|s| !s.is_empty() && s.len() <= 4096)
                        .ok_or_else(|| {
                            let mut e = OperationError::new(
                                ErrorKind::Integrity,
                                "s3-create-upload",
                                "response has no usable upload ID; incomplete upload may remain",
                            );
                            e.outcome = OperationState::Unknown;
                            e
                        })?
                        .to_owned(),
                )
            } else {
                None
            };
            let mut session = S3WriteSession {
                store: self.store.clone(),
                class: self.class,
                attributes,
                attribute_notices: crate::attributes::notices(source, destination),
                destination: destination.clone(),
                plan,
                upload_id,
                current: BytesMut::with_capacity(plan.size.min(PART_SIZE as u64) as usize),
                bytes: 0,
                next_part: 1,
                inflight: crate::parts::PartScheduler::new(),
                completed: Vec::with_capacity(plan.parts as usize),
                completed_bytes: 0,
                full_crc: CRC32.digest(),
                source_checksum: source
                    .checksum
                    .as_ref()
                    .and_then(crate::checksum::Checksummer::for_expected),
                expected_checksum: source.checksum.as_ref().map(|s| s.value.clone()),
                failed: watch::channel(None).0,
                state: State::Active,
                cancel: cancel.clone(),
                abort_result: None,
            };
            if let Err(mut error) = cancel.check("s3-begin") {
                if let Err(cleanup) = session.abort().await {
                    error.context.causes.push(format!("cleanup: {cleanup}"));
                }
                return Err(error);
            }
            Ok(Box::new(session) as Box<dyn WriteSession>)
        })
    }
}

pub(super) fn storage_class(class: DataClass) -> StorageClass {
    match class {
        DataClass::Standard => StorageClass::Standard,
        DataClass::Archived => StorageClass::DeepArchive,
    }
}
fn crc(value: u32) -> String {
    base64::engine::general_purpose::STANDARD.encode(value.to_be_bytes())
}

#[derive(Clone)]
struct Failure {
    error: OperationError,
    at: Instant,
}
#[derive(PartialEq, Eq)]
enum State {
    Active,
    Failed,
    Committed,
    Aborted,
}
struct S3WriteSession {
    attribute_notices: Vec<Warning>,
    attributes: std::collections::HashMap<String, String>,
    store: S3Store,
    class: DataClass,
    destination: Location,
    plan: UploadPlan,
    upload_id: Option<String>,
    current: BytesMut,
    bytes: u64,
    next_part: u32,
    inflight: crate::parts::PartScheduler<CompletedPart>,
    completed: Vec<CompletedPart>,
    completed_bytes: usize,
    full_crc: crc::Digest<'static, u32>,
    source_checksum: Option<crate::checksum::Checksummer>,
    expected_checksum: Option<Vec<u8>>,
    failed: watch::Sender<Option<Failure>>,
    state: State,
    cancel: CancelContext,
    abort_result: Option<Result<(), OperationError>>,
}
impl S3WriteSession {
    fn check(&self) -> Result<(), OperationError> {
        if let Some(failed) = self.failed.borrow().as_ref() {
            return Err(failed.error.clone());
        }
        Ok(())
    }
    fn integrity(&self, message: &str) -> OperationError {
        let mut e = OperationError::new(ErrorKind::Integrity, "s3-upload", message);
        e.context.destination_uri = Some(self.destination.display());
        e.outcome = OperationState::Failed;
        e
    }
    fn active(&self) -> Result<(), OperationError> {
        self.check()?;
        if self.state != State::Active {
            return Err(OperationError::new(
                ErrorKind::Input,
                "s3-session",
                "write session is no longer active",
            ));
        }
        Ok(())
    }
    fn completed(
        &mut self,
        result: Result<Result<CompletedPart, OperationError>, tokio::task::JoinError>,
    ) -> Result<(), OperationError> {
        let part =
            result.map_err(|_| self.integrity("part task terminated without a response"))??;
        let bytes = std::mem::size_of::<CompletedPart>()
            + part.e_tag().map_or(0, str::len)
            + part.checksum_crc32().map_or(0, str::len);
        self.completed_bytes += bytes;
        if self.completed_bytes > 4 * 1024 * 1024 {
            return Err(self.integrity("completed part metadata exceeds 4MiB"));
        }
        self.completed.push(part);
        Ok(())
    }
    fn drain_ready(&mut self) -> Result<(), OperationError> {
        while let Some(result) = self.inflight.try_join_next() {
            self.completed(result)?;
        }
        self.check()
    }
    async fn dispatch(&mut self, cancel: &CancelContext) -> Result<(), OperationError> {
        self.drain_ready()?;
        cancel.check("s3-part")?;
        self.check()?;
        cancel.check("s3-part")?;
        let number = self.next_part;
        if self.plan.part(number).map(|(_, n)| n) != Some(self.current.len() as u64) {
            return Err(self.integrity("part length does not match the upload plan"));
        }
        let body = self.current.split().freeze();
        // Four in-flight immutable Bytes plus one assembly allocation. There is
        // no unbounded queue and no retry copy of the request body.
        self.current = BytesMut::with_capacity(
            self.plan
                .part(number + 1)
                .map_or(0, |(_, length)| length as usize),
        );
        let store = self.store.clone();
        let destination = self.destination.clone();
        let upload_id = self.upload_id.clone().expect("multipart session");
        let failed = self.failed.clone();
        let operation = cancel.clone();
        let ready = self
            .inflight
            .schedule(
                async move {
                    if let Some(failure) = failed.borrow().as_ref() {
                        return Err(failure.error.clone());
                    }
                    let checksum = crc(CRC32.checksum(&body));
                    let id = store.id(&destination, true)?;
                    let (body, progress) = super::progress::body(body);
                    let sent = async {
                        let output = store
                            .client
                            .upload_part()
                            .bucket(&id.authority)
                            .key(&id.key)
                            .upload_id(upload_id)
                            .part_number(number as i32)
                            .content_length(body.size_hint().0 as i64)
                            .checksum_crc32(&checksum)
                            .body(body)
                            .customize()
                            .config_override(super::progress::upload_timeouts())
                            .send()
                            .await
                            .map_err(|e| sdk_error("s3-upload-part", &destination, e, true))?;
                        if output.checksum_crc32().is_some_and(|v| v != checksum) {
                            return Err(OperationError::new(
                                ErrorKind::Integrity,
                                "s3-upload-part",
                                "part response checksum mismatch",
                            ));
                        }
                        let etag = output.e_tag().filter(|s| !s.is_empty()).ok_or_else(|| {
                            OperationError::new(
                                ErrorKind::Integrity,
                                "s3-upload-part",
                                "part response has no ETag",
                            )
                        })?;
                        Ok(CompletedPart::builder()
                            .part_number(number as i32)
                            .e_tag(etag)
                            .checksum_crc32(checksum)
                            .build())
                    };
                    let result = tokio::time::timeout(
                        API_TIMEOUT,
                        super::progress::request(sent, progress, &destination),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(unknown(
                            &destination,
                            "s3-upload-part",
                            "part operation timed out",
                        ))
                    });
                    if let Err(error) = &result {
                        operation.record_failure();
                        failed.send_if_modified(|slot| {
                            if slot.is_none() {
                                *slot = Some(Failure {
                                    error: error.clone(),
                                    at: Instant::now(),
                                });
                                true
                            } else {
                                false
                            }
                        });
                    }
                    result
                },
                cancel,
            )
            .await?;
        for result in ready {
            self.completed(result)?;
        }
        self.next_part += 1;
        Ok(())
    }
}

impl WriteSession for S3WriteSession {
    fn destination(&self) -> &Location {
        &self.destination
    }
    fn failure(&self) -> Pin<Box<dyn Future<Output = OperationError> + Send + '_>> {
        let mut receiver = self.failed.subscribe();
        Box::pin(async move {
            loop {
                if let Some(failure) = receiver.borrow_and_update().as_ref() {
                    return failure.error.clone();
                }
                if receiver.changed().await.is_err() {
                    return OperationError::new(
                        ErrorKind::Io,
                        "s3-upload",
                        "part failure channel closed",
                    );
                }
            }
        })
    }
    fn write<'a>(&'a mut self, chunk: &'a [u8], cancel: &'a CancelContext) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.active()?;
            self.state = State::Failed;
            self.cancel = cancel.clone();
            cancel.check("s3-write")?;
            self.drain_ready()?;
            if chunk.len() > LOCAL_BUFFER_BYTES {
                return Err(OperationError::new(
                    ErrorKind::Input,
                    "s3-write",
                    "chunk exceeds the 1MiB I/O limit",
                ));
            }
            let bytes = self
                .bytes
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| self.integrity("byte count overflow"))?;
            if bytes > self.plan.size {
                return Err(self.integrity("body exceeds expected size"));
            }
            self.full_crc.update(chunk);
            if let Some(digest) = &mut self.source_checksum {
                digest.update(chunk);
            }
            self.bytes = bytes;
            let mut remaining = chunk;
            while !remaining.is_empty() {
                let n = remaining.len().min(PART_SIZE - self.current.len());
                self.current.extend_from_slice(&remaining[..n]);
                remaining = &remaining[n..];
                if self.plan.parts > 0 && self.current.len() == PART_SIZE {
                    self.dispatch(cancel).await?;
                }
            }
            self.check()?;
            cancel.check("s3-write")?;
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
            self.cancel = cancel.clone();
            cancel.check("s3-finish")?;
            if !body_complete || self.bytes != self.plan.size {
                return Err(self.integrity("body size or EOF verification failed"));
            }
            if let Some(digest) = self.source_checksum.take()
                && self.expected_checksum.as_deref() != Some(digest.finish().as_slice())
            {
                return Err(self.integrity("source checksum mismatch"));
            }
            let checksum = crc(std::mem::replace(&mut self.full_crc, CRC32.digest()).finalize());
            let id = self.store.id(&self.destination, true)?.clone();
            if self.plan.parts == 0 {
                let (body, progress) = super::progress::body(self.current.split().freeze());
                let output = mutate(
                    super::progress::request(
                        async {
                            self.store
                                .client
                                .put_object()
                                .set_metadata(Some(self.attributes.clone()))
                                .bucket(&id.authority)
                                .key(&id.key)
                                .storage_class(storage_class(self.class))
                                .content_length(self.plan.size as i64)
                                .checksum_crc32(&checksum)
                                .body(body)
                                .customize()
                                .config_override(super::progress::upload_timeouts())
                                .send()
                                .await
                                .map_err(|e| sdk_error("s3-put", &self.destination, e, true))
                        },
                        progress,
                        &self.destination,
                    ),
                    cancel,
                    &self.destination,
                )
                .await?;
                if output.checksum_crc32().is_some_and(|v| v != checksum) {
                    return Err(unknown(
                        &self.destination,
                        "s3-put",
                        "PUT succeeded but response checksum differs",
                    ));
                }
            } else {
                if !self.current.is_empty() {
                    self.dispatch(cancel).await?;
                }
                while !self.inflight.is_empty() {
                    let result = tokio::select! { biased;
                        _ = cancel.cancelled() => { cancel.check("s3-finish")?; unreachable!() },
                        result = self.inflight.join_next() => result.expect("inflight part"),
                    };
                    self.completed(result)?;
                    self.check()?;
                }
                self.inflight
                    .validate_complete(self.plan.parts, self.completed.len())?;
                cancel.check("s3-complete")?;
                self.completed.sort_by_key(|p| p.part_number());
                let parts = CompletedMultipartUpload::builder()
                    .set_parts(Some(std::mem::take(&mut self.completed)))
                    .build();
                let output = mutate(
                    async {
                        self.store
                            .client
                            .complete_multipart_upload()
                            .bucket(&id.authority)
                            .key(&id.key)
                            .upload_id(self.upload_id.as_deref().expect("multipart session"))
                            .multipart_upload(parts)
                            .checksum_type(ChecksumType::FullObject)
                            .checksum_crc32(&checksum)
                            .mpu_object_size(self.plan.size as i64)
                            .send()
                            .await
                            .map_err(|e| sdk_error("s3-complete", &self.destination, e, true))
                    },
                    cancel,
                    &self.destination,
                )
                .await?;
                if output.checksum_crc32().is_some_and(|v| v != checksum) {
                    return Err(unknown(
                        &self.destination,
                        "s3-complete",
                        "Complete succeeded but response checksum differs",
                    ));
                }
            }
            self.state = State::Committed;
            for notice in self.attribute_notices.drain(..) {
                let _ = ancillary(
                    cancel,
                    Duration::from_millis(100),
                    Duration::from_secs(3),
                    warnings.warn(notice),
                )
                .await;
            }
            Ok(SaveOutcome {
                observed: None,
                destination: self.destination.clone(),
                bytes: self.bytes,
                verification: Verification::SizeAndChecksum,
            })
        })
    }
    fn abort<'a>(&'a mut self) -> PortFuture<'a, ()> {
        Box::pin(async move {
            if let Some(result) = &self.abort_result {
                return result.clone();
            }
            if self.state == State::Committed || self.state == State::Aborted {
                return Ok(());
            }
            self.state = State::Failed;
            let stopped = self
                .failed
                .borrow()
                .as_ref()
                .map(|f| f.at)
                .into_iter()
                .chain(self.cancel.stopped_at())
                .min()
                .unwrap_or_else(Instant::now);
            self.current.clear();
            let mut unconfirmed = false;
            while !self.inflight.is_empty() {
                match timeout_at(stopped + Duration::from_secs(30), self.inflight.join_next()).await
                {
                    Ok(_) => {}
                    Err(_) => {
                        unconfirmed = true;
                        self.inflight.abort_all();
                        while self.inflight.join_next().await.is_some() {}
                        break;
                    }
                }
            }
            if let Some(upload_id) = &self.upload_id {
                let id = self.store.id(&self.destination, true)?;
                let deadline = (Instant::now() + Duration::from_secs(15))
                    .min(stopped + Duration::from_secs(45));
                // Cache an unknown result before issuing: even if this future is
                // dropped at its deadline, a second abort must not retry it.
                self.abort_result = Some(Err(unknown(
                    &self.destination,
                    "s3-abort",
                    "Abort response was not confirmed",
                )));
                let result = timeout_at(
                    deadline,
                    self.store
                        .client
                        .abort_multipart_upload()
                        .bucket(&id.authority)
                        .key(&id.key)
                        .upload_id(upload_id)
                        .send(),
                )
                .await
                .map_err(|_| {
                    unknown(
                        &self.destination,
                        "s3-abort",
                        "Abort timed out; upload may remain",
                    )
                })?
                .map(|_| ())
                .map_err(|e| sdk_error("s3-abort", &self.destination, e, true));
                self.abort_result = Some(result.clone());
                result?;
            }
            self.state = State::Aborted;
            if unconfirmed {
                let error = unknown(
                    &self.destination,
                    "s3-abort",
                    "part requests could not be confirmed; Abort was attempted once",
                );
                self.abort_result = Some(Err(error.clone()));
                return Err(error);
            }
            Ok(())
        })
    }
}

pub(super) fn unknown(
    destination: &Location,
    stage: &'static str,
    message: &str,
) -> OperationError {
    let mut error = OperationError::new(ErrorKind::Unavailable, stage, message);
    error.outcome = OperationState::Unknown;
    error.context.destination_uri = Some(destination.display());
    error
}

/// Once issued, a mutation is allowed up to 30s to report a confirmed result
/// after cancellation. Dropping/losing a response never proves no remote change.
pub(super) async fn mutate<T>(
    future: impl Future<Output = Result<T, OperationError>>,
    cancel: &CancelContext,
    destination: &Location,
) -> Result<T, OperationError> {
    cancel.check("s3-mutation")?;
    let request = tokio::time::timeout(API_TIMEOUT, future);
    tokio::pin!(request);
    let result = tokio::select! { biased;
        result = &mut request => result,
        _ = cancel.cancelled() => tokio::time::timeout(Duration::from_secs(30), &mut request).await
            .map_err(|_| unknown(destination, "s3-mutation", "cancelled mutation response is unknown"))?,
    };
    result.unwrap_or_else(|_| {
        Err(unknown(
            destination,
            "s3-mutation",
            "mutation operation timed out",
        ))
    })
}
