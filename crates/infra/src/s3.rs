mod copy;
mod index_file;
mod remove;
mod restore;
pub use remove::S3Remover;
pub use restore::S3Restorer;
mod loader;
mod metadata;
mod progress;
#[cfg(all(test, feature = "test-fixtures"))]
mod tests;
mod upload;
use archive_domain::{DataClass, ErrorKind, Location, ObjectId, OperationError, OperationState};
use archive_sync::CancelContext;
use aws_sdk_s3::{
    Client,
    config::{
        BehaviorVersion, RequestChecksumCalculation, ResponseChecksumValidation,
        retry::RetryConfig, timeout::TimeoutConfig,
    },
};
pub use copy::{CopyPlan, S3RemoteCopy};
pub use loader::S3Loader;
use std::{future::Future, time::Duration};
pub use upload::{S3Saver, UploadPlan};

pub(super) const API_TIMEOUT: Duration = Duration::from_secs(900);
pub(super) const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// SDK clients never cross this adapter boundary. Construct only for an explicit
/// remote operation; local browsing must not construct/resolve cloud credentials.
#[derive(Clone)]
pub struct S3Store {
    client: Client,
    namespace: String,
}

fn timeouts() -> TimeoutConfig {
    TimeoutConfig::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(IDLE_TIMEOUT)
        .disable_operation_timeout()
        .disable_operation_attempt_timeout()
        .build()
}

async fn standard_sdk_config(
    cancel: &CancelContext,
) -> Result<aws_types::SdkConfig, OperationError> {
    cancel.check("s3-config")?;
    let shared = tokio::select! {
        _ = cancel.cancelled() => { cancel.check("s3-config")?; unreachable!() },
        config = aws_config::defaults(BehaviorVersion::latest())
            .stalled_stream_protection(aws_sdk_s3::config::StalledStreamProtectionConfig::disabled())
            .retry_config(RetryConfig::standard().with_max_attempts(1))
            .timeout_config(timeouts())
            .request_checksum_calculation(RequestChecksumCalculation::WhenSupported)
            .response_checksum_validation(ResponseChecksumValidation::WhenSupported)
            .load() => config,
    };
    // Reject configured endpoint overrides before credentials are resolved
    // for an S3 request; the normal build cannot inject a test endpoint.
    let mut custom_endpoint = shared.endpoint_url().is_some();
    if let Some(config) = shared.service_config() {
        for service in ["S3", "STS"] {
            let key = aws_types::service_config::ServiceConfigKey::builder()
                .service_id(service)
                .env("AWS_ENDPOINT_URL")
                .profile("endpoint_url")
                .build()
                .expect("static service key");
            custom_endpoint |= config.load_config(key).is_some();
        }
    }
    if custom_endpoint {
        return Err(OperationError::new(
            ErrorKind::Input,
            "s3-config",
            "custom endpoints require the test-fixtures build",
        ));
    }
    if shared.region().is_none() {
        return Err(OperationError::new(
            ErrorKind::Input,
            "s3-config",
            "configure an AWS region through the standard SDK configuration",
        ));
    }
    Ok(shared)
}

impl S3Store {
    pub fn index_files(
        &self,
        location: &Location,
        cache: std::path::PathBuf,
    ) -> Result<Box<dyn archive_sync::IndexFileStore>, OperationError> {
        self.id(location, true)?;
        Ok(Box::new(index_file::S3IndexFiles::new(
            self.clone(),
            location.clone(),
            cache,
        )))
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub async fn from_env(cancel: &CancelContext) -> Result<Self, OperationError> {
        let shared = standard_sdk_config(cancel).await?;
        let mut builder = aws_sdk_s3::config::Builder::from(&shared);
        builder.set_endpoint_url(None);
        let client = Client::from_conf(
            builder
                .retry_config(RetryConfig::standard().with_max_attempts(1))
                .timeout_config(timeouts())
                .stalled_stream_protection(
                    aws_sdk_s3::config::StalledStreamProtectionConfig::disabled(),
                )
                .request_checksum_calculation(RequestChecksumCalculation::WhenSupported)
                .response_checksum_validation(ResponseChecksumValidation::WhenSupported)
                .build(),
        );
        Ok(Self {
            client,
            namespace: "aws".into(),
        })
    }

    pub fn loader(&self) -> S3Loader {
        S3Loader {
            store: self.clone(),
        }
    }
    pub fn restorer(&self) -> S3Restorer {
        S3Restorer {
            store: self.clone(),
        }
    }
    pub fn remover(&self) -> S3Remover {
        S3Remover {
            store: self.clone(),
        }
    }
    pub fn saver(&self, class: DataClass) -> S3Saver {
        S3Saver {
            store: self.clone(),
            class,
        }
    }
    pub fn copier(&self, class: DataClass) -> S3RemoteCopy {
        S3RemoteCopy {
            store: self.clone(),
            class,
        }
    }
    pub fn object(&self, bucket: &str, key: &str) -> Result<Location, OperationError> {
        let mut id = ObjectId::parse_s3(&format!("s3://{bucket}/{key}"))
            .map_err(|e| OperationError::new(ErrorKind::Input, "s3-location", e))?;
        id.namespace = self.namespace.clone();
        Ok(Location::Remote(id))
    }

    /// No env/profile reads: fixed dummy credentials, loopback HTTP endpoint,
    /// path-style access and a namespace isolated from real AWS observations.
    #[cfg(feature = "test-fixtures")]
    pub fn fixture_namespace(endpoint: &str) -> Result<String, OperationError> {
        let authority = endpoint.strip_prefix("http://").ok_or_else(|| {
            OperationError::new(
                ErrorKind::Input,
                "s3-fixture",
                "expected a loopback HTTP endpoint",
            )
        })?;
        let address: std::net::SocketAddr = authority.parse().map_err(|_| {
            OperationError::new(
                ErrorKind::Input,
                "s3-fixture",
                "expected an IP address and port",
            )
        })?;
        if !address.ip().is_loopback() {
            return Err(OperationError::new(
                ErrorKind::Input,
                "s3-fixture",
                "fixture endpoint must be loopback",
            ));
        }
        Ok(format!("test:{endpoint}"))
    }

    #[cfg(feature = "test-fixtures")]
    pub fn for_test(endpoint: &str) -> Result<Self, OperationError> {
        let namespace = Self::fixture_namespace(endpoint)?;
        let config =
            aws_sdk_s3::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(aws_sdk_s3::config::Region::new("us-east-1"))
                .credentials_provider(aws_sdk_s3::config::Credentials::new(
                    "private-archive-test",
                    "private-archive-test-secret",
                    None,
                    None,
                    "fixture",
                ))
                .endpoint_url(endpoint)
                .force_path_style(true)
                .retry_config(RetryConfig::standard().with_max_attempts(1))
                .timeout_config(timeouts())
                .stalled_stream_protection(
                    aws_sdk_s3::config::StalledStreamProtectionConfig::disabled(),
                )
                .request_checksum_calculation(RequestChecksumCalculation::WhenSupported)
                .response_checksum_validation(ResponseChecksumValidation::WhenSupported)
                .build();
        Ok(Self {
            client: Client::from_conf(config),
            namespace,
        })
    }

    fn id<'a>(&self, location: &'a Location, object: bool) -> Result<&'a ObjectId, OperationError> {
        match location {
            Location::Remote(id)
                if id.namespace == self.namespace
                    && id.scheme == "s3"
                    && (!object || !id.key.is_empty()) =>
            {
                Ok(id)
            }
            _ => Err(OperationError::new(
                ErrorKind::Input,
                "s3-location",
                "wrong store namespace or missing object key",
            )
            .at(location)),
        }
    }
}

pub(super) async fn read_request<T>(
    future: impl Future<Output = Result<T, OperationError>>,
    cancel: &CancelContext,
) -> Result<T, OperationError> {
    cancel.check("s3-read")?;
    tokio::select! {
        biased;
        _ = cancel.cancelled() => { cancel.check("s3-read")?; unreachable!() },
        result = tokio::time::timeout(API_TIMEOUT, future) => result.unwrap_or_else(|_| Err(OperationError::new(ErrorKind::Unavailable, "s3-read", "API operation timed out"))),
    }
}

pub(super) fn sdk_error<E>(
    stage: &'static str,
    location: &Location,
    error: aws_sdk_s3::error::SdkError<E>,
    mutation: bool,
) -> OperationError
where
    E: aws_sdk_s3::error::ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    use aws_sdk_s3::error::SdkError;
    let status = error.raw_response().map(|r| r.status().as_u16());
    let service = error.as_service_error();
    let code = service.and_then(|e| e.code()).map(str::to_owned);
    let kind = match status {
        Some(404) => ErrorKind::NotFound,
        Some(401 | 403) => ErrorKind::Permission,
        Some(500..=599) => ErrorKind::Unavailable,
        _ if matches!(code.as_deref(), Some("BadDigest" | "InvalidDigest")) => ErrorKind::Integrity,
        _ if matches!(error, SdkError::ConstructionFailure(_)) => ErrorKind::Input,
        _ => ErrorKind::Unavailable,
    };
    let mut result = OperationError::new(
        kind,
        stage,
        match service.and_then(|e| e.message()) {
            Some(message) => safe_message(message),
            None => match &error {
                SdkError::TimeoutError(_) => "SDK request timed out".into(),
                SdkError::DispatchFailure(_) => "SDK connection or transport failed".into(),
                SdkError::ConstructionFailure(_) => "SDK request could not be constructed".into(),
                _ => "SDK response could not be validated or parsed".into(),
            },
        },
    );
    if mutation {
        result.context.destination_uri = Some(location.display());
    } else {
        result.context.source_uri = Some(location.display());
    }
    result.outcome = if matches!(error, SdkError::ConstructionFailure(_)) {
        OperationState::NotStarted
    } else if mutation && (service.is_none() || status.is_some_and(|s| s >= 500)) {
        OperationState::Unknown
    } else {
        OperationState::Failed
    };
    result.code = code;
    result.context.request_id = error
        .raw_response()
        .and_then(|r| r.headers().get("x-amz-request-id"))
        .map(str::to_owned);
    if let Some(status) = status {
        result.context.causes.push(format!("HTTP {status}"));
    }
    let mut cause = std::error::Error::source(&error);
    for _ in 0..4 {
        let Some(source) = cause else {
            break;
        };
        result.context.causes.push(
            safe_message(&source.to_string())
                .chars()
                .take(512)
                .collect(),
        );
        cause = source.source();
    }
    // Do not format SDK Debug/raw requests: they may contain signed URLs or
    // Authorization headers. The service code/status/request ID remain available.
    result
}

fn safe_message(message: &str) -> String {
    if message.contains("http://")
        || message.contains("https://")
        || message.to_ascii_lowercase().contains("authorization")
        || message.contains("X-Amz-")
    {
        "service returned a diagnostic containing request credentials or URLs (redacted)".into()
    } else {
        message.chars().take(2048).collect()
    }
}
