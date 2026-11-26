use crate::*;
use archive_domain::*;
use archive_index::{IndexWrite, MetadataPatch};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::time::Instant;

pub struct CopyRequest {
    pub protected: Option<ObjectId>,
    pub source: Selection,
    pub destination: Location,
    pub cwd: PathBuf,
    pub ignore: Vec<String>,
}
/// Frontends assemble these ports; this use case never resolves credentials or
/// imports a store SDK/database implementation.
pub struct CopyEngine<'a> {
    pub local_loader: &'a dyn Loader,
    pub local_saver: &'a dyn Saver,
    pub remote_loader: &'a dyn Loader,
    pub remote_saver: &'a dyn Saver,
    pub remote_copy: &'a dyn RemoteCopy,
    pub index: Result<&'a dyn IndexWrite, OperationError>,
}
struct Diagnostics<'a> {
    observer: &'a dyn OperationObserver,
    count: AtomicU64,
    ignored: AtomicU64,
    direct: bool,
    roots: [String; 2],
}
impl WarningSink for Diagnostics<'_> {
    fn warn<'a>(&'a self, warning: Warning) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.count.fetch_add(1, Ordering::Relaxed);
            if warning.category == WarningCategory::Ignored {
                self.ignored.fetch_add(1, Ordering::Relaxed);
                let direct = self.direct || self.roots.contains(&warning.uri);
                let _ = self.observer.ignored(warning, direct).await;
            } else {
                let _ = self.observer.warn(warning).await;
            }
            Ok(())
        })
    }
}
struct AttachedIndex<'a> {
    port: Option<&'a dyn IndexWrite>,
}
impl AttachedIndex<'_> {
    async fn failed(
        &mut self,
        error: OperationError,
        location: &Location,
        warnings: &dyn WarningSink,
        cancel: &CancelContext,
    ) {
        self.port = None;
        let _ = ancillary(
            cancel,
            Duration::from_secs(2),
            Duration::from_secs(3),
            warnings.warn(Warning {
                category: WarningCategory::Index,
                stage: "cp-index",
                uri: location.display(),
                code: error.code.clone(),
                message: format!("index disabled for this copy: {error}"),
            }),
        )
        .await;
    }
    async fn observed(
        &mut self,
        meta: ObjectMetadata,
        warnings: &dyn WarningSink,
        cancel: &CancelContext,
    ) {
        if !matches!(meta.location, Location::Remote(_)) {
            return;
        }
        let location = meta.location.clone();
        if let Some(port) = self.port {
            let result = async { port.apply(MetadataPatch::full(meta)?).await };
            let result = ancillary(
                cancel,
                Duration::from_secs(3),
                Duration::from_secs(3),
                result,
            )
            .await
            .unwrap_or_else(|| {
                Err(OperationError::new(
                    ErrorKind::Unavailable,
                    "cp-index",
                    "index request exceeded three seconds",
                ))
            });
            if let Err(e) = result {
                self.failed(e, &location, warnings, cancel).await;
            }
        }
    }
    async fn missing(
        &mut self,
        location: &Location,
        warnings: &dyn WarningSink,
        cancel: &CancelContext,
    ) {
        if let (Some(port), Location::Remote(id)) = (self.port, location) {
            let result = ancillary(
                cancel,
                Duration::from_secs(3),
                Duration::from_secs(3),
                port.remove(id),
            )
            .await
            .unwrap_or_else(|| {
                Err(OperationError::new(
                    ErrorKind::Unavailable,
                    "cp-index",
                    "index request exceeded three seconds",
                ))
            });
            if let Err(e) = result {
                self.failed(e, location, warnings, cancel).await;
            }
        }
    }
}
impl CopyEngine<'_> {
    fn loader(&self, location: &Location) -> &dyn Loader {
        match location {
            Location::Local(_) => self.local_loader,
            Location::Remote(_) => self.remote_loader,
        }
    }
    fn saver(&self, location: &Location) -> &dyn Saver {
        match location {
            Location::Local(_) => self.local_saver,
            Location::Remote(_) => self.remote_saver,
        }
    }
    pub async fn copy(
        &self,
        request: &CopyRequest,
        plan: &mut dyn PlanStore,
        observer: &dyn OperationObserver,
        cancel: &CancelContext,
    ) -> Result<CopySummary, OperationError> {
        let diagnostics = Diagnostics {
            observer,
            count: AtomicU64::new(0),
            ignored: AtomicU64::new(0),
            direct: request.source.mode == SelectionMode::Exact,
            roots: [request.source.root.display(), request.destination.display()],
        };
        let mut index = AttachedIndex {
            port: self.index.as_ref().ok().copied(),
        };
        if let Err(error) = &self.index {
            index
                .failed(error.clone(), &request.source.root, &diagnostics, cancel)
                .await;
        }
        let mut result = self
            .execute(request, plan, observer, &diagnostics, &mut index, cancel)
            .await;
        // The manifest is operation-local. Its removal never rolls back results.
        if result.is_err() {
            cancel.record_failure();
        }
        let ignored = diagnostics.ignored.load(Ordering::Relaxed);
        let _ = ancillary(
            cancel,
            Duration::from_millis(100),
            Duration::from_secs(3),
            observer.ignored_summary(ignored),
        )
        .await;
        let deadline = cancel
            .cleanup_deadline()
            .min(Instant::now() + Duration::from_secs(5));
        plan.set_cleanup_deadline(deadline);
        let cleanup = tokio::time::timeout_at(deadline, plan.discard()).await;
        if !matches!(cleanup, Ok(Ok(()))) {
            let message = match cleanup {
                Ok(Err(e)) => e.to_string(),
                _ => "manifest cleanup exceeded five seconds".into(),
            };
            if let Err(error) = &mut result {
                error
                    .context
                    .causes
                    .push(format!("manifest cleanup: {message}"));
            }
            let _ = ancillary(
                cancel,
                Duration::from_millis(100),
                Duration::ZERO,
                diagnostics.warn(Warning {
                    category: WarningCategory::Cleanup,
                    stage: "cp-manifest",
                    uri: request.source.root.display(),
                    code: None,
                    message,
                }),
            )
            .await;
        }
        result.map(|mut summary| {
            summary.warnings = diagnostics.count.load(Ordering::Relaxed);
            summary.ignored = ignored;
            summary
        })
    }
    async fn execute(
        &self,
        request: &CopyRequest,
        plan: &mut dyn PlanStore,
        observer: &dyn OperationObserver,
        warnings: &dyn WarningSink,
        index: &mut AttachedIndex<'_>,
        cancel: &CancelContext,
    ) -> Result<CopySummary, OperationError> {
        cancel.check("cp-plan")?;
        let destination_is_directory = if matches!(request.destination, Location::Local(_))
            && request.source.mode == SelectionMode::Exact
        {
            match self.local_saver.probe(&request.destination, cancel).await? {
                DestinationObservation::Existing(m) => m.kind == EntryKind::Directory,
                DestinationObservation::Missing => false,
            }
        } else {
            false
        };
        let layout = CopyLayout::new(
            request.source.clone(),
            request.destination.clone(),
            destination_is_directory,
            &request.cwd,
            &request.ignore,
        )?;
        let loader = self.loader(&layout.source.root);
        let mut cursor = loader
            .enumerate(&layout.source, layout.source_filter(), cancel)
            .await?;
        let mut planned = 0u64;
        while let Some(item) = cursor.next(cancel).await? {
            cancel.check("cp-plan")?;
            let entry = match item {
                EnumerationItem::Skipped(w) => {
                    let _ = ancillary(
                        cancel,
                        Duration::from_millis(100),
                        Duration::from_secs(3),
                        warnings.warn(w),
                    )
                    .await;
                    continue;
                }
                EnumerationItem::Entry(m) => m,
            };
            let route = match layout.resolve(&entry.location, entry.kind)? {
                Candidate::Skip(w) => {
                    let _ = ancillary(
                        cancel,
                        Duration::from_millis(100),
                        Duration::from_secs(3),
                        warnings.warn(w),
                    )
                    .await;
                    continue;
                }
                Candidate::OutsideSelection => continue,
                Candidate::Copy(route) => route,
            };
            let source = route.source();
            let destination = route.destination();
            check_protected(&source, request.protected.as_ref())
                .and_then(|_| check_protected(&destination, request.protected.as_ref()))
                .map_err(|e| route_error(e, &source, &destination))?;
            let metadata = loader
                .stat(&source, cancel)
                .await
                .map_err(|e| route_error(e, &source, &destination))?;
            if metadata.kind != EntryKind::File || metadata.size.is_none() {
                return Err(OperationError::new(
                    ErrorKind::Unsupported,
                    "cp-plan",
                    "source is not an ordinary file with a known size",
                )
                .at(&source));
            }
            if matches!(
                metadata.restore_state,
                RestoreState::Archived | RestoreState::Restoring
            ) {
                return Err(OperationError::new(
                    ErrorKind::Unavailable,
                    "cp-plan",
                    "source is archived or restoring",
                )
                .at(&source));
            }
            index.observed(metadata.clone(), warnings, cancel).await;
            let observation = self
                .saver(&destination)
                .probe(&destination, cancel)
                .await
                .map_err(|e| route_error(e, &source, &destination))?;
            if let DestinationObservation::Existing(meta) = &observation
                && meta.kind != EntryKind::File
            {
                return Err(OperationError::new(
                    ErrorKind::Unsupported,
                    "cp-plan",
                    "resolved destination is not an ordinary file",
                )
                .at(&destination));
            }
            // Reflect each completed observation, using the store results
            // directly even if the optional index was disabled.
            match &observation {
                DestinationObservation::Existing(m) => {
                    index.observed(*m.clone(), warnings, cancel).await
                }
                DestinationObservation::Missing => {
                    index.missing(&destination, warnings, cancel).await
                }
            }
            plan.append(
                ResolvedCopy {
                    route,
                    source_metadata: metadata,
                    destination_observation: observation,
                },
                cancel,
            )
            .await?;
            planned = planned.checked_add(1).ok_or_else(|| {
                OperationError::new(ErrorKind::Unsupported, "cp-plan", "target count overflow")
            })?;
        }
        if planned == 0 {
            return Err(OperationError::new(
                ErrorKind::NotFound,
                "cp-plan",
                "no targets after selection and ignore; copy could not be performed",
            ));
        }
        plan.seal(cancel).await?;
        let mut summary = CopySummary::default();
        while let Some(entry) = plan.next(cancel).await? {
            cancel.check("cp-transfer")?;
            let source = entry.route.source();
            let destination = entry.route.destination();
            let saved = async {
                Ok::<_, OperationError>(match entry.route {
                    CopyRoute::Remote { .. } => {
                        self.remote_copy
                            .copy(&entry.source_metadata, &destination, warnings, cancel)
                            .await?
                    }
                    _ => {
                        let mut opened = self.loader(&source).open(&source, cancel).await?;
                        index
                            .observed(opened.metadata.clone(), warnings, cancel)
                            .await;
                        let mut session = self
                            .saver(&destination)
                            .begin(&destination, &opened.metadata, cancel)
                            .await?;
                        stream_to_session(&mut opened.body, session.as_mut(), warnings, cancel)
                            .await?
                    }
                })
            }
            .await
            .map_err(|e| route_error(e, &source, &destination))?;
            // Publish confirmed success to the index before output. Failures in
            // either ancillary index request must not conceal a completed copy.
            if matches!(destination, Location::Remote(_)) && index.port.is_some() {
                if let Some(observed) = saved.observed {
                    index.observed(*observed, warnings, cancel).await;
                } else {
                    let bounded = CancelContext::new().with_deadline(
                        cancel.ancillary_deadline(Duration::from_secs(3), Duration::from_secs(3)),
                    );
                    match self.remote_loader.stat(&destination, &bounded).await {
                        Ok(meta) => index.observed(meta, warnings, cancel).await,
                        Err(e) => index.failed(e, &destination, warnings, cancel).await,
                    }
                }
            }
            summary.files = summary.files.checked_add(1).ok_or_else(|| {
                OperationError::new(ErrorKind::Unsupported, "cp-summary", "file count overflow")
            })?;
            summary.bytes = summary.bytes.checked_add(saved.bytes).ok_or_else(|| {
                OperationError::new(ErrorKind::Unsupported, "cp-summary", "byte count overflow")
            })?;
            let outcome = CopyOutcome {
                source,
                destination,
                bytes: saved.bytes,
                verification: saved.verification,
            };
            // Deliver a confirmed success even when cancellation arrived just
            // after the store committed it. Stop before starting the next file.
            ancillary(
                cancel,
                Duration::from_secs(5),
                Duration::from_secs(3),
                observer.copied(outcome),
            )
            .await
            .ok_or_else(|| {
                OperationError::new(
                    if cancel.is_cancelled() {
                        ErrorKind::Cancelled
                    } else {
                        ErrorKind::Io
                    },
                    "cp-observer",
                    "result delivery exceeded five seconds",
                )
            })??;
        }
        Ok(summary)
    }
}

fn route_error(
    mut error: OperationError,
    source: &Location,
    destination: &Location,
) -> OperationError {
    error.context.source_uri = Some(source.display());
    error.context.destination_uri = Some(destination.display());
    error
}
