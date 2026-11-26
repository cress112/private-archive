use crate::*;
use archive_domain::*;
use archive_index::{IndexWrite, MetadataPatch};
use std::{
    num::NonZeroU32,
    str::FromStr,
    time::{Duration, SystemTime},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreDays(NonZeroU32);
impl RestoreDays {
    pub fn get(self) -> u32 {
        self.0.get()
    }
}
impl FromStr for RestoreDays {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (digits, multiplier) = if let Some(n) = s.strip_suffix('d') {
            (n, 1u32)
        } else if let Some(n) = s.strip_suffix('w') {
            (n, 7u32)
        } else {
            return Err(
                "restore duration requires a positive integer followed by d or w (7d, 1w)".into(),
            );
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err("invalid restore duration".into());
        }
        let days = digits
            .parse::<u32>()
            .ok()
            .and_then(|n| n.checked_mul(multiplier))
            .filter(|n| *n <= i32::MAX as u32)
            .and_then(NonZeroU32::new)
            .ok_or("restore duration is zero or out of range")?;
        Ok(Self(days))
    }
}
#[derive(Clone)]
pub struct RestoreRequest {
    pub protected: Option<ObjectId>,
    pub selections: Vec<Selection>,
    pub ignore: Vec<String>,
    pub days: RestoreDays,
    pub tier: RestoreTier,
}
pub struct RestoreAccepted {
    pub queried_at: SystemTime,
}
pub trait RemoteRestore: Send + Sync {
    fn restore<'a>(
        &'a self,
        object: &'a ObjectMetadata,
        days: RestoreDays,
        tier: RestoreTier,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, RestoreAccepted>;
}
#[derive(Debug, Clone, Copy, Default)]
pub struct RestoreSummary {
    pub resolved: u64,
    pub accepted: u64,
    pub ignored: u64,
}
pub trait RestoreObserver: WarningSink {
    fn ignored<'a>(&'a self, warning: Warning, _direct: bool) -> PortFuture<'a, ()> {
        self.warn(warning)
    }
    fn accepted<'a>(
        &'a self,
        location: Location,
        days: RestoreDays,
        tier: RestoreTier,
    ) -> PortFuture<'a, ()>;
    fn finished<'a>(&'a self, _summary: RestoreSummary) -> PortFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}
pub struct RestoreEngine<'a> {
    pub loader: &'a dyn Loader,
    pub remote: &'a dyn RemoteRestore,
    pub index: Result<&'a dyn IndexWrite, OperationError>,
}
fn error(kind: ErrorKind, message: &str) -> OperationError {
    OperationError::new(kind, "restore", message)
}
enum Eligibility {
    Ready,
    Invalid,
    Unknown,
}
fn eligibility(m: &ObjectMetadata) -> Eligibility {
    if matches!(
        m.restore_state,
        RestoreState::Restoring | RestoreState::Restored { .. } | RestoreState::NotRequired
    ) || matches!(&m.data_class, Some(StoredClass::Standard))
        || matches!(&m.data_class,Some(StoredClass::Other(c)) if matches!(c.as_str(),"STANDARD_IA"|"ONEZONE_IA"|"GLACIER_IR"))
    {
        return Eligibility::Invalid;
    }
    let cold = matches!(m.data_class, Some(StoredClass::Archived))
        || matches!(&m.data_class,Some(StoredClass::Other(c)) if c=="GLACIER");
    if cold && m.restore_state == RestoreState::Archived {
        Eligibility::Ready
    } else {
        Eligibility::Unknown
    }
}
impl RestoreEngine<'_> {
    pub async fn restore(
        &self,
        request: &RestoreRequest,
        plan: &mut dyn ObjectPlan,
        observer: &dyn RestoreObserver,
        cancel: &CancelContext,
    ) -> Result<RestoreSummary, OperationError> {
        let mut summary = RestoreSummary::default();
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
                    "resolved={}, accepted={}, ignored={}",
                    summary.resolved, summary.accepted, summary.ignored
                ));
                if let Some(Err(cleanup)) = cleanup {
                    e.context
                        .causes
                        .push(format!("manifest cleanup: {cleanup}"));
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
        request: &RestoreRequest,
        plan: &mut dyn ObjectPlan,
        observer: &dyn RestoreObserver,
        cancel: &CancelContext,
        index: &mut Option<&dyn IndexWrite>,
        summary: &mut RestoreSummary,
    ) -> Result<(), OperationError> {
        cancel.check("restore")?;
        if request.selections.is_empty() {
            return Err(error(
                ErrorKind::Input,
                "at least one store URI is required",
            ));
        }
        for selection in &request.selections {
            if !matches!(&selection.root,Location::Remote(id) if selection.mode==SelectionMode::Descendants || !id.key.is_empty())
            {
                return Err(error(
                    ErrorKind::Input,
                    "expected a store object, or an explicit recursive prefix",
                ));
            }
        }
        let mut invalid_observation = None;
        for (order, selection) in request.selections.iter().enumerate() {
            cancel.check("restore")?;
            let ignore = IgnoreFilter::new(selection.ignore_base.clone(), &request.ignore)?;
            let direct = selection.mode == SelectionMode::Exact;
            if direct && ignore.is_ignored(&selection.root, false)? {
                summary.ignored += 1;
                let warning = Warning {
                    category: WarningCategory::Ignored,
                    stage: "restore",
                    uri: selection.root.display(),
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
                return Err(error(
                    ErrorKind::NotFound,
                    "no targets after selection and ignore",
                ));
            }
            let mut found = 0u64;
            let mut cursor = self.loader.enumerate(selection, &ignore, cancel).await?;
            while let Some(item) = cursor.next(cancel).await? {
                cancel.check("restore")?;
                let meta = match item {
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
                        return Err(error(ErrorKind::Integrity, "unconfirmed skipped object"));
                    }
                    EnumerationItem::Entry(meta) => meta,
                };
                if meta.kind != EntryKind::File || !selection.contains(&meta.location) {
                    return Err(error(
                        ErrorKind::Integrity,
                        "store returned an object outside the requested scope",
                    ));
                }
                if ignore.is_ignored(&meta.location, false)? {
                    summary.ignored += 1;
                    let warning = Warning {
                        category: WarningCategory::Ignored,
                        stage: "restore",
                        uri: meta.location.display(),
                        code: None,
                        message: "skipped by ignore pattern".into(),
                    };
                    let _ = ancillary(
                        cancel,
                        Duration::from_millis(100),
                        Duration::from_secs(3),
                        observer.ignored(warning, direct),
                    )
                    .await;
                    continue;
                }
                found += 1;
                check_protected(&meta.location, request.protected.as_ref())?;
                if direct && found > 1 {
                    return Err(error(
                        ErrorKind::Integrity,
                        "exact lookup returned multiple objects",
                    ));
                }
                let object = PlannedObject {
                    metadata: *meta,
                    full_observation: direct,
                };
                if invalid_observation.is_none()
                    && matches!(eligibility(&object.metadata), Eligibility::Invalid)
                {
                    invalid_observation = Some(
                        error(
                            ErrorKind::Unsupported,
                            "object is restoring, restored, or immediately accessible",
                        )
                        .at(&object.metadata.location),
                    );
                }
                observe(index, &object, observer, cancel).await;
                if plan.append(order as u64, object, cancel).await? {
                    summary.resolved += 1;
                }
            }
            if found == 0 {
                return Err(error(
                    ErrorKind::NotFound,
                    "no targets after selection and ignore",
                ));
            }
        }
        if let Some(error) = invalid_observation {
            return Err(error);
        }
        if plan.seal(cancel).await?.objects == 0 {
            return Err(error(ErrorKind::NotFound, "no restore targets"));
        }
        // Resolve and inspect every URI before the first mutation.
        while let Some(mut object) = plan.next(cancel).await? {
            if matches!(eligibility(&object.metadata), Eligibility::Unknown) {
                let location = object.metadata.location.clone();
                cancel.check("restore")?;
                let fresh = self.loader.stat(&location, cancel).await?;
                if fresh.location != location || fresh.kind != EntryKind::File {
                    return Err(error(
                        ErrorKind::Integrity,
                        "additional lookup changed object identity",
                    ));
                }
                object = PlannedObject {
                    metadata: fresh,
                    full_observation: true,
                };
                observe(index, &object, observer, cancel).await;
                let Location::Remote(id) = &location else {
                    unreachable!()
                };
                plan.replace(id, object.clone(), cancel).await?;
            }
            match eligibility(&object.metadata) {
                Eligibility::Ready => {}
                Eligibility::Invalid => {
                    return Err(error(
                        ErrorKind::Unsupported,
                        "object is restoring, restored, or immediately accessible",
                    )
                    .at(&object.metadata.location));
                }
                Eligibility::Unknown => {
                    return Err(error(
                        ErrorKind::Integrity,
                        "restore eligibility could not be determined",
                    )
                    .at(&object.metadata.location));
                }
            }
        }
        plan.rewind(cancel).await?;
        while let Some(mut object) = plan.next(cancel).await? {
            cancel.check("restore")?;
            let accepted = self
                .remote
                .restore(&object.metadata, request.days, request.tier, cancel)
                .await?;
            object.metadata.restore_state = RestoreState::Restoring;
            object.metadata.queried_at = Some(accepted.queried_at);
            observe(index, &object, observer, cancel).await;
            summary.accepted += 1;
            ancillary(
                cancel,
                Duration::from_secs(5),
                Duration::from_secs(3),
                observer.accepted(object.metadata.location, request.days, request.tier),
            )
            .await
            .ok_or_else(|| error(ErrorKind::Io, "result delivery timed out"))??;
        }
        Ok(())
    }
}
async fn observe(
    index: &mut Option<&dyn IndexWrite>,
    object: &PlannedObject,
    observer: &dyn RestoreObserver,
    cancel: &CancelContext,
) {
    let Some(port) = *index else {
        return;
    };
    let patch = if object.full_observation {
        MetadataPatch::full(object.metadata.clone())
    } else {
        MetadataPatch::listing(object.metadata.clone())
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
async fn index_warning(e: &OperationError, observer: &dyn RestoreObserver, cancel: &CancelContext) {
    let _ = ancillary(
        cancel,
        Duration::from_millis(100),
        Duration::from_secs(3),
        observer.warn(Warning {
            category: WarningCategory::Index,
            stage: e.stage,
            uri: e.context.source_uri.clone().unwrap_or_default(),
            code: e.code.clone(),
            message: format!("index disabled for this restore: {e}"),
        }),
    )
    .await;
}
