use crate::{IgnoreFilter, Selection};
use archive_domain::{ErrorKind, Location, ObjectMetadata, OperationError, Warning};
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::{io::AsyncRead, sync::watch, time::Instant};

pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, OperationError>> + Send + 'a>>;
pub type BodyReader = Pin<Box<dyn AsyncRead + Send>>;
pub const LOCAL_BUFFER_BYTES: usize = 1024 * 1024;

/// Shared cancellation with an optional deadline. No per-file tasks are spawned.
#[derive(Debug, Clone)]
pub struct CancelContext {
    signal: Arc<watch::Sender<Option<Instant>>>,
    deadline: Option<Instant>,
    failure_time: Arc<std::sync::Mutex<Option<Instant>>>,
}
impl Default for CancelContext {
    fn default() -> Self {
        Self::new()
    }
}
impl CancelContext {
    pub fn new() -> Self {
        Self {
            signal: Arc::new(watch::channel(None).0),
            deadline: None,
            failure_time: Arc::new(std::sync::Mutex::new(None)),
        }
    }
    pub fn with_deadline(&self, deadline: Instant) -> Self {
        Self {
            signal: self.signal.clone(),
            failure_time: self.failure_time.clone(),
            deadline: Some(self.deadline.map_or(deadline, |old| old.min(deadline))),
        }
    }
    pub fn record_failure(&self) {
        self.failure_time
            .lock()
            .unwrap()
            .get_or_insert_with(Instant::now);
    }
    pub fn stopped_at(&self) -> Option<Instant> {
        self.cancelled_at()
            .into_iter()
            .chain(*self.failure_time.lock().unwrap())
            .min()
    }
    pub fn cleanup_deadline(&self) -> Instant {
        self.stopped_at().unwrap_or_else(Instant::now) + std::time::Duration::from_secs(50)
    }
    pub fn ancillary_deadline(
        &self,
        maximum: std::time::Duration,
        reserve: std::time::Duration,
    ) -> Instant {
        let normal = Instant::now() + maximum;
        self.stopped_at().map_or(normal, |stopped| {
            normal.min(stopped + std::time::Duration::from_secs(50) - reserve)
        })
    }
    pub fn cancel(&self) {
        self.signal.send_if_modified(|time| {
            if time.is_none() {
                *time = Some(Instant::now());
                true
            } else {
                false
            }
        });
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled_at().is_some()
    }
    pub fn cancelled_at(&self) -> Option<Instant> {
        let requested = *self.signal.borrow();
        let expired = self.deadline.filter(|d| Instant::now() >= *d);
        match (requested, expired) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
    pub fn check(&self, stage: &'static str) -> Result<(), OperationError> {
        if self.is_cancelled() {
            Err(OperationError::new(
                ErrorKind::Cancelled,
                stage,
                "operation cancelled or deadline reached",
            ))
        } else {
            Ok(())
        }
    }
    pub async fn cancelled(&self) {
        let mut receiver = self.signal.subscribe();
        let wait_signal = async {
            loop {
                if receiver.borrow_and_update().is_some() {
                    return;
                }
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        };
        if let Some(deadline) = self.deadline {
            tokio::select! { _ = wait_signal => {}, _ = tokio::time::sleep_until(deadline) => {} }
        } else {
            wait_signal.await;
        }
    }
}

/// Ancillary work shares the stop budget and leaves time for manifest cleanup.
pub async fn ancillary<T>(
    cancel: &CancelContext,
    maximum: std::time::Duration,
    reserve: std::time::Duration,
    future: impl Future<Output = T>,
) -> Option<T> {
    let deadline = cancel.ancillary_deadline(maximum, reserve);
    if Instant::now() >= deadline {
        return None;
    }
    tokio::pin!(future);
    tokio::select! {biased;
        result=tokio::time::timeout_at(deadline,&mut future)=>result.ok(),
        _=cancel.cancelled()=>tokio::time::timeout_at(deadline.min(cancel.cleanup_deadline()-reserve),&mut future).await.ok(),
    }
}

pub enum EnumerationItem {
    Entry(Box<ObjectMetadata>),
    Skipped(Warning),
}
pub trait EntryCursor: Send + Sync {
    fn next<'a>(&'a mut self, cancel: &'a CancelContext)
    -> PortFuture<'a, Option<EnumerationItem>>;
}

pub struct OpenedSource {
    pub metadata: ObjectMetadata,
    pub body: BodyReader,
}

pub trait Loader: Send + Sync {
    fn stat<'a>(
        &'a self,
        location: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ObjectMetadata>;
    fn enumerate<'a>(
        &'a self,
        selection: &'a Selection,
        ignore: &'a IgnoreFilter,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Box<dyn EntryCursor>>;
    fn open<'a>(
        &'a self,
        location: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, OpenedSource>;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum DestinationObservation {
    Missing,
    Existing(Box<ObjectMetadata>),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    SizeAndChecksum,
    SizeOnly,
}
pub struct SaveOutcome {
    pub destination: Location,
    pub bytes: u64,
    pub verification: Verification,
    /// A real destination observation already obtained by the adapter.
    pub observed: Option<Box<ObjectMetadata>>,
}

/// Server-side copy; implementations must never fall back to downloading a body.
pub trait RemoteCopy: Send + Sync {
    fn copy<'a>(
        &'a self,
        source: &'a ObjectMetadata,
        destination: &'a Location,
        warnings: &'a dyn WarningSink,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, SaveOutcome>;
}

pub trait WarningSink: Send + Sync {
    fn warn<'a>(&'a self, warning: Warning) -> PortFuture<'a, ()>;
}

pub trait Saver: Send + Sync {
    fn probe<'a>(
        &'a self,
        destination: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, DestinationObservation>;
    fn begin<'a>(
        &'a self,
        destination: &'a Location,
        source: &'a ObjectMetadata,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Box<dyn WriteSession>>;
}

/// A failed write/finish is terminal. Call abort explicitly to observe cleanup
/// failures. Dropping the session is only a best-effort fallback.
pub trait WriteSession: Send + Sync {
    fn destination(&self) -> &Location;
    /// Wake the producer when an asynchronous write fails while it is waiting
    /// for input. Local sessions have no background requests.
    fn failure(&self) -> Pin<Box<dyn Future<Output = OperationError> + Send + '_>> {
        Box::pin(std::future::pending())
    }
    fn write<'a>(&'a mut self, chunk: &'a [u8], cancel: &'a CancelContext) -> PortFuture<'a, ()>;
    fn finish<'a>(
        &'a mut self,
        body_complete: bool,
        warnings: &'a dyn WarningSink,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, SaveOutcome>;
    /// Cleanup uses its own deadline; it must remain callable after cancellation.
    fn abort<'a>(&'a mut self) -> PortFuture<'a, ()>;
}

/// Shared bounded pump. EOF must be observed even when Content-Length is known;
/// bytes beyond the expected size and errors after the last chunk remain errors.
/// This does not resolve targets or implement the full cp use case (P1-08).
pub async fn stream_to_session(
    body: &mut BodyReader,
    session: &mut dyn WriteSession,
    warnings: &dyn WarningSink,
    cancel: &CancelContext,
) -> Result<SaveOutcome, OperationError> {
    use tokio::io::AsyncReadExt;
    let transfer = async {
        let mut buffer = vec![0; LOCAL_BUFFER_BYTES];
        loop {
            cancel.check("read")?;
            let count = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(OperationError::new(ErrorKind::Cancelled, "read", "operation cancelled")),
                error = session.failure() => return Err(error),
                result = body.read(&mut buffer) => result.map_err(|e| OperationError::new(ErrorKind::Io, "read", e.to_string()))?,
            };
            if count == 0 { break; }
            session.write(&buffer[..count], cancel).await?;
        }
        session.finish(true, warnings, cancel).await
    }.await;
    if transfer.is_err() {
        cancel.record_failure();
        // Leave five seconds of the shared 50-second cleanup budget for
        // diagnostics; a future multipart adapter can use 30s + 15s to abort.
        match tokio::time::timeout_at(
            cancel.cleanup_deadline() - std::time::Duration::from_secs(5),
            session.abort(),
        )
        .await
        {
            Ok(Ok(())) => {}
            failure => {
                let message = match failure {
                    Ok(Err(e)) => e.to_string(),
                    _ => "cleanup exceeded 45 seconds".into(),
                };
                let warning = Warning {
                    category: archive_domain::WarningCategory::Cleanup,
                    stage: "abort",
                    uri: session.destination().display(),
                    code: None,
                    message,
                };
                // Preserve the original failure even if diagnostic delivery fails.
                let _ = tokio::time::timeout_at(
                    (Instant::now() + std::time::Duration::from_millis(100))
                        .min(cancel.cleanup_deadline()),
                    warnings.warn(warning),
                )
                .await;
            }
        }
    }
    transfer
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedCopy {
    pub route: crate::CopyRoute,
    pub source_metadata: ObjectMetadata,
    pub destination_observation: DestinationObservation,
}
pub trait PlanStore: Send + Sync {
    fn set_cleanup_deadline(&mut self, _deadline: Instant) {}
    fn append<'a>(
        &'a mut self,
        entry: ResolvedCopy,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ()>;
    fn seal<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, ()>;
    fn next<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, Option<ResolvedCopy>>;
    fn discard<'a>(&'a mut self) -> PortFuture<'a, ()>;
}

pub struct CopyOutcome {
    pub source: Location,
    pub destination: Location,
    pub bytes: u64,
    pub verification: Verification,
}
pub trait OperationObserver: WarningSink {
    fn copied<'a>(&'a self, outcome: CopyOutcome) -> PortFuture<'a, ()>;
    fn ignored<'a>(&'a self, warning: Warning, _direct: bool) -> PortFuture<'a, ()> {
        self.warn(warning)
    }
    fn ignored_summary<'a>(&'a self, _observed: u64) -> PortFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}
#[derive(Debug, Default)]
pub struct CopySummary {
    pub files: u64,
    pub bytes: u64,
    pub warnings: u64,
    pub ignored: u64,
}
