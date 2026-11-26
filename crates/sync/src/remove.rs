use crate::*;
use archive_domain::*;
use archive_index::{IndexWrite, MetadataPatch};
use std::time::Duration;

#[derive(Clone)]
pub struct RemoveRequest {
    pub protected: Option<ObjectId>,
    pub selection: Selection,
    pub ignore: Vec<String>,
    pub auto_approve: bool,
}
pub trait RemoteRemove: Send + Sync {
    fn remove<'a>(&'a self, id: &'a ObjectId, cancel: &'a CancelContext) -> PortFuture<'a, ()>;
}
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoveSummary {
    pub resolved: u64,
    pub removed: u64,
    pub ignored: u64,
}
pub trait RemoveObserver: WarningSink {
    fn ignored<'a>(&'a self, warning: Warning, _direct: bool) -> PortFuture<'a, ()> {
        self.warn(warning)
    }
    fn confirm<'a>(
        &'a self,
        root: Location,
        summary: ObjectPlanSummary,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, bool>;
    fn removed<'a>(&'a self, location: Location) -> PortFuture<'a, ()>;
    fn finished<'a>(&'a self, _summary: RemoveSummary) -> PortFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}
pub struct RemoveEngine<'a> {
    pub loader: &'a dyn Loader,
    pub remote: &'a dyn RemoteRemove,
    pub index: Result<&'a dyn IndexWrite, OperationError>,
}
fn error(kind: ErrorKind, message: &str) -> OperationError {
    OperationError::new(kind, "rm", message)
}
impl RemoveEngine<'_> {
    pub async fn remove(
        &self,
        request: &RemoveRequest,
        plan: &mut dyn ObjectPlan,
        observer: &dyn RemoveObserver,
        cancel: &CancelContext,
    ) -> Result<RemoveSummary, OperationError> {
        let mut summary = RemoveSummary::default();
        let mut index = self.index.clone().ok();
        if let Err(e) = &self.index {
            index_warning(e, observer, cancel).await;
        }
        let result = self
            .run(request, plan, observer, cancel, &mut index, &mut summary)
            .await;
        if result.is_err() {
            cancel.record_failure();
        }
        let _ = ancillary(
            cancel,
            Duration::from_millis(100),
            Duration::from_secs(3),
            observer.finished(summary),
        )
        .await;
        let cleanup = ancillary(
            cancel,
            Duration::from_secs(47),
            Duration::from_secs(3),
            plan.discard(),
        )
        .await;
        match result {
            Err(mut e) => {
                e.context.causes.push(format!(
                    "resolved={}, removed={}, ignored={}",
                    summary.resolved, summary.removed, summary.ignored
                ));
                if let Some(Err(c)) = cleanup {
                    e.context.causes.push(format!("manifest cleanup: {c}"));
                }
                Err(e)
            }
            Ok(()) => {
                cleanup.ok_or_else(|| error(ErrorKind::Io, "manifest cleanup timed out"))??;
                Ok(summary)
            }
        }
    }
    async fn run(
        &self,
        request: &RemoveRequest,
        plan: &mut dyn ObjectPlan,
        observer: &dyn RemoveObserver,
        cancel: &CancelContext,
        index: &mut Option<&dyn IndexWrite>,
        summary: &mut RemoveSummary,
    ) -> Result<(), OperationError> {
        cancel.check("rm")?;
        let selection = &request.selection;
        if !matches!(&selection.root,Location::Remote(id) if selection.mode==SelectionMode::Descendants || !id.key.is_empty())
        {
            return Err(error(
                ErrorKind::Input,
                "expected a store object or an explicit recursive prefix",
            ));
        }
        let ignore = IgnoreFilter::new(selection.ignore_base.clone(), &request.ignore)?;
        let direct = selection.mode == SelectionMode::Exact;
        if direct && ignore.is_ignored(&selection.root, false)? {
            summary.ignored = 1;
            ignored(observer, &selection.root, direct, cancel).await;
            return Err(error(
                ErrorKind::NotFound,
                "no targets after selection and ignore",
            ));
        }
        let mut cursor = self.loader.enumerate(selection, &ignore, cancel).await?;
        while let Some(item) = cursor.next(cancel).await? {
            cancel.check("rm")?;
            let meta = match item {
                EnumerationItem::Entry(m) => m,
                EnumerationItem::Skipped(w) if w.category == WarningCategory::Ignored => {
                    summary.ignored += 1;
                    let _ = ancillary(
                        cancel,
                        Duration::from_millis(100),
                        Duration::from_secs(3),
                        observer.ignored(w, direct),
                    )
                    .await;
                    continue;
                }
                EnumerationItem::Skipped(_) => {
                    return Err(error(
                        ErrorKind::Integrity,
                        "store skipped an object without a confirmed ignore decision",
                    ));
                }
            };
            if meta.kind != EntryKind::File || !selection.contains(&meta.location) {
                return Err(error(
                    ErrorKind::Integrity,
                    "store returned an object outside the requested scope",
                ));
            }
            if ignore.is_ignored(&meta.location, false)? {
                summary.ignored += 1;
                ignored(observer, &meta.location, direct, cancel).await;
                continue;
            }
            if direct && summary.resolved > 0 {
                return Err(error(
                    ErrorKind::Integrity,
                    "exact lookup returned multiple objects",
                ));
            }
            check_protected(&meta.location, request.protected.as_ref())?;
            if let Some(port) = *index {
                let patch = if direct {
                    MetadataPatch::full(*meta.clone())
                } else {
                    MetadataPatch::listing(*meta.clone())
                };
                let result = ancillary(
                    cancel,
                    Duration::from_secs(3),
                    Duration::from_secs(3),
                    async { port.apply(patch?).await },
                )
                .await
                .unwrap_or_else(|| Err(error(ErrorKind::Unavailable, "index update timed out")));
                if let Err(e) = result {
                    *index = None;
                    index_warning(&e, observer, cancel).await;
                }
            }
            if plan
                .append(
                    0,
                    PlannedObject {
                        metadata: *meta,
                        full_observation: direct,
                    },
                    cancel,
                )
                .await?
            {
                summary.resolved += 1;
            }
        }
        let targets = plan.seal(cancel).await?;
        if targets.objects == 0 {
            return Err(error(
                ErrorKind::NotFound,
                "no targets after selection and ignore",
            ));
        }
        if !request.auto_approve
            && !observer
                .confirm(selection.root.clone(), targets, cancel)
                .await?
        {
            return Err(error(
                ErrorKind::Io,
                "confirmation declined or input ended; nothing deleted",
            ));
        }
        cancel.check("rm")?;
        while let Some(object) = plan.next(cancel).await? {
            cancel.check("rm")?;
            let location = object.metadata.location;
            let Location::Remote(id) = &location else {
                return Err(error(
                    ErrorKind::Integrity,
                    "manifest returned a local target",
                ));
            };
            self.remote.remove(id, cancel).await?;
            summary.removed += 1;
            if let Some(port) = *index {
                let result = ancillary(
                    cancel,
                    Duration::from_secs(3),
                    Duration::from_secs(3),
                    port.remove(id),
                )
                .await
                .unwrap_or_else(|| Err(error(ErrorKind::Unavailable, "index removal timed out")));
                if let Err(e) = result {
                    *index = None;
                    index_warning(&e, observer, cancel).await;
                }
            }
            ancillary(
                cancel,
                Duration::from_secs(5),
                Duration::from_secs(3),
                observer.removed(location),
            )
            .await
            .ok_or_else(|| error(ErrorKind::Io, "result delivery timed out"))??;
        }
        Ok(())
    }
}
async fn ignored(
    observer: &dyn RemoveObserver,
    location: &Location,
    direct: bool,
    cancel: &CancelContext,
) {
    let _ = ancillary(
        cancel,
        Duration::from_millis(100),
        Duration::from_secs(3),
        observer.ignored(
            Warning {
                category: WarningCategory::Ignored,
                stage: "rm",
                uri: location.display(),
                code: None,
                message: "skipped by ignore pattern".into(),
            },
            direct,
        ),
    )
    .await;
}
async fn index_warning(e: &OperationError, observer: &dyn RemoveObserver, cancel: &CancelContext) {
    let _ = ancillary(
        cancel,
        Duration::from_millis(100),
        Duration::from_secs(3),
        observer.warn(Warning {
            category: WarningCategory::Index,
            stage: e.stage,
            uri: e.context.source_uri.clone().unwrap_or_default(),
            code: e.code.clone(),
            message: format!("index disabled for this removal: {e}"),
        }),
    )
    .await;
}
