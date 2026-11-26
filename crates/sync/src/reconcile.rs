use crate::*;
use archive_domain::*;
use archive_index::{IndexScope, IndexWrite, MetadataPatch, Reconcile};
use std::time::Duration;

pub struct ReconcileRequest {
    pub selection: Selection,
    pub ignore: Vec<String>,
}
#[derive(Debug, Clone, Copy, Default)]
pub struct ReconcileSummary {
    pub observed: u64,
    pub removed: u64,
    pub ignored: u64,
}
pub trait ReconcileObserver: Send + Sync {
    fn ignored<'a>(&'a self, _warning: Warning, _direct: bool) -> PortFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn progress<'a>(
        &'a self,
        _current: Location,
        _summary: ReconcileSummary,
    ) -> PortFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    /// removed is confirmed only when succeeded is true.
    fn finished<'a>(&'a self, _summary: ReconcileSummary, _succeeded: bool) -> PortFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}
impl ReconcileObserver for () {}

/// Targets come from this execution's store cursor. IndexRead is not a dependency.
pub struct ReconcileEngine<'a> {
    pub loader: &'a dyn Loader,
    pub index: &'a dyn IndexWrite,
}
impl ReconcileEngine<'_> {
    pub async fn reconcile(
        &self,
        request: &ReconcileRequest,
        observer: &dyn ReconcileObserver,
        cancel: &CancelContext,
    ) -> Result<ReconcileSummary, OperationError> {
        cancel.check("reconcile")?;
        let Location::Remote(root) = &request.selection.root else {
            return Err(OperationError::new(
                ErrorKind::Input,
                "reconcile",
                "a store URI is required",
            ));
        };
        if request.selection.mode == SelectionMode::Exact && root.key.is_empty() {
            return Err(OperationError::new(
                ErrorKind::Input,
                "reconcile",
                "non-recursive reconciliation requires an object key",
            ));
        }
        let ignore = IgnoreFilter::new(request.selection.ignore_base.clone(), &request.ignore)?;
        let scope = IndexScope {
            root: root.clone(),
            recursive: request.selection.mode == SelectionMode::Descendants,
            max_depth: request.selection.max_depth,
            ignore: request.ignore.clone(),
        };
        let mut session = self.index.begin_reconcile(scope).await?;
        let control = session.control();
        let signal = cancel.clone();
        let bridge = tokio::spawn(async move {
            signal.cancelled().await;
            control.cancel();
        });
        let mut summary = ReconcileSummary::default();
        let result = self
            .scan(
                request,
                &ignore,
                &mut *session,
                observer,
                cancel,
                &mut summary,
            )
            .await;
        let result = match result {
            Ok(()) => {
                if summary.observed == 0 && summary.ignored > 0 {
                    Err(OperationError::new(
                        ErrorKind::NotFound,
                        "reconcile",
                        "no targets after ignore; reconciliation could not be performed",
                    ))
                } else {
                    match cancel.check("reconcile") {
                        Err(e) => Err(e),
                        Ok(()) => match index_work(cancel, session.finish()).await {
                            Ok(removed) => {
                                summary.removed = removed;
                                Ok(summary)
                            }
                            Err(e) => Err(e),
                        },
                    }
                }
            }
            Err(e) => Err(e),
        };
        let result = match result {
            Ok(summary) => Ok(summary),
            Err(mut error) => {
                cancel.record_failure();
                if let Some(Err(cleanup)) = ancillary(
                    cancel,
                    Duration::from_secs(3),
                    Duration::ZERO,
                    session.cancel(),
                )
                .await
                {
                    error
                        .context
                        .causes
                        .push(format!("reconcile cleanup: {cleanup}"));
                }
                error.context.causes.push(format!(
                    "observed={}, ignored={}; reconciliation/deletion not completed",
                    summary.observed, summary.ignored
                ));
                Err(error)
            }
        };
        let _ = ancillary(
            cancel,
            Duration::from_millis(100),
            Duration::ZERO,
            observer.finished(summary, result.is_ok()),
        )
        .await;
        bridge.abort();
        let _ = bridge.await;
        result
    }
    async fn scan(
        &self,
        request: &ReconcileRequest,
        ignore: &IgnoreFilter,
        session: &mut dyn Reconcile,
        observer: &dyn ReconcileObserver,
        cancel: &CancelContext,
        summary: &mut ReconcileSummary,
    ) -> Result<(), OperationError> {
        let direct = request.selection.mode == SelectionMode::Exact;
        if direct && ignore.is_ignored(&request.selection.root, false)? {
            summary.ignored = 1;
            let warning = Warning {
                category: WarningCategory::Ignored,
                stage: "reconcile",
                uri: request.selection.root.display(),
                code: None,
                message: "skipped by ignore pattern".into(),
            };
            let _ = ancillary(
                cancel,
                Duration::from_millis(100),
                Duration::from_secs(3),
                observer.ignored(warning, true),
            )
            .await;
            return Ok(());
        }
        let mut cursor = self
            .loader
            .enumerate(&request.selection, ignore, cancel)
            .await?;
        loop {
            cancel.check("reconcile")?;
            let item = match cursor.next(cancel).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(e)
                    if direct
                        && summary.observed == 0
                        && summary.ignored == 0
                        && e.kind == ErrorKind::NotFound =>
                {
                    break;
                }
                Err(e) => return Err(e),
            };
            let (meta, excluded) = match item {
                EnumerationItem::Skipped(w) if w.category == WarningCategory::Ignored => {
                    (None, Some(w))
                }
                EnumerationItem::Skipped(_) => {
                    return Err(OperationError::new(
                        ErrorKind::Integrity,
                        "reconcile",
                        "store skipped an item without a confirmed ignore decision",
                    ));
                }
                EnumerationItem::Entry(m) => {
                    if !request.selection.contains(&m.location) || m.kind != EntryKind::File {
                        return Err(OperationError::new(
                            ErrorKind::Integrity,
                            "reconcile",
                            "store returned an unsupported object or an observation outside scope",
                        ));
                    }
                    if ignore.is_ignored(&m.location, false)? {
                        (
                            None,
                            Some(Warning {
                                category: WarningCategory::Ignored,
                                stage: "reconcile",
                                uri: m.location.display(),
                                code: None,
                                message: "skipped by ignore pattern".into(),
                            }),
                        )
                    } else {
                        (Some(m), None)
                    }
                }
            };
            if let Some(warning) = excluded {
                summary.ignored += 1;
                let _ = ancillary(
                    cancel,
                    Duration::from_millis(100),
                    Duration::from_secs(3),
                    observer.ignored(warning, direct),
                )
                .await;
            }
            if let Some(meta) = meta {
                if direct && summary.observed > 0 {
                    return Err(OperationError::new(
                        ErrorKind::Integrity,
                        "reconcile",
                        "exact lookup returned more than one object",
                    ));
                }
                let location = meta.location.clone();
                let patch = if direct {
                    MetadataPatch::full(*meta)
                } else {
                    MetadataPatch::listing(*meta)
                }
                .map_err(|mut e| {
                    e.kind = ErrorKind::Integrity;
                    e
                })?;
                summary.observed += 1;
                index_work(cancel, session.observe(patch)).await?;
                let _ = ancillary(
                    cancel,
                    Duration::from_millis(100),
                    Duration::from_secs(3),
                    observer.progress(location, *summary),
                )
                .await;
            }
        }
        Ok(())
    }
}
async fn index_work<T>(
    cancel: &CancelContext,
    future: impl std::future::Future<Output = Result<T, OperationError>>,
) -> Result<T, OperationError> {
    tokio::pin!(future);
    tokio::select! {biased;
        result=&mut future=>result,
        _=cancel.cancelled()=>tokio::time::timeout_at(cancel.cleanup_deadline()-Duration::from_secs(3),&mut future).await.unwrap_or_else(|_|Err(OperationError::new(ErrorKind::Cancelled,"reconcile-index","index did not finish within cancellation budget"))),
    }
}
