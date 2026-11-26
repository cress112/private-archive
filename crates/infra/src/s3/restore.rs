use super::{S3Store, sdk_error, upload::mutate};
use archive_domain::*;
use archive_sync::*;
use aws_sdk_s3::types::{GlacierJobParameters, RestoreRequest as SdkRequest, Tier};
use aws_smithy_runtime_api::client::{
    interceptors::{Intercept, context::BeforeDeserializationInterceptorContextRef},
    runtime_components::RuntimeComponents,
};
use aws_smithy_types::config_bag::ConfigBag;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::SystemTime,
};

pub struct S3Restorer {
    pub(super) store: S3Store,
}
#[derive(Debug)]
struct Status(Arc<AtomicU16>);
impl Intercept for Status {
    fn name(&self) -> &'static str {
        "RestoreResponseStatus"
    }
    fn read_before_deserialization(
        &self,
        ctx: &BeforeDeserializationInterceptorContextRef<'_>,
        _: &RuntimeComponents,
        _: &mut ConfigBag,
    ) -> Result<(), aws_smithy_runtime_api::box_error::BoxError> {
        self.0
            .store(ctx.response().status().as_u16(), Ordering::Release);
        Ok(())
    }
}
impl RemoteRestore for S3Restorer {
    fn restore<'a>(
        &'a self,
        object: &'a ObjectMetadata,
        days: RestoreDays,
        tier: RestoreTier,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, RestoreAccepted> {
        Box::pin(async move {
            let location = &object.location;
            let id = self.store.id(location, true)?;
            if object.kind != EntryKind::File {
                return Err(
                    OperationError::new(ErrorKind::Input, "s3-restore", "file required")
                        .at(location),
                );
            }
            let tier = match tier {
                RestoreTier::Bulk => Tier::Bulk,
                RestoreTier::Standard => Tier::Standard,
            };
            let job = GlacierJobParameters::builder()
                .tier(tier)
                .build()
                .map_err(|e| OperationError::new(ErrorKind::Input, "s3-restore", e.to_string()))?;
            let request = SdkRequest::builder()
                .days(days.get() as i32)
                .glacier_job_parameters(job)
                .build();
            let status = Arc::new(AtomicU16::new(0));
            mutate(
                async {
                    self.store
                        .client
                        .restore_object()
                        .bucket(&id.authority)
                        .key(&id.key)
                        .restore_request(request)
                        .customize()
                        .interceptor(Status(status.clone()))
                        .send()
                        .await
                        .map_err(|e| sdk_error("s3-restore", location, e, true))
                },
                cancel,
                location,
            )
            .await?;
            let status = status.load(Ordering::Acquire);
            if status != 202 {
                let mut error = OperationError::new(
                    if status == 200 {
                        ErrorKind::Unsupported
                    } else {
                        ErrorKind::Integrity
                    },
                    "s3-restore",
                    if status == 200 {
                        "object was already restored when the request completed"
                    } else {
                        "restore acceptance could not be confirmed"
                    },
                );
                error.outcome = if status == 200 {
                    OperationState::Failed
                } else {
                    OperationState::Unknown
                };
                error.context.destination_uri = Some(location.display());
                error.context.causes.push(format!("HTTP {status}"));
                if status == 200 {
                    error.context.causes.push("S3 may have updated the existing temporary copy lifetime; no further request was issued".into());
                }
                return Err(error);
            }
            Ok(RestoreAccepted {
                queried_at: SystemTime::now(),
            })
        })
    }
}
