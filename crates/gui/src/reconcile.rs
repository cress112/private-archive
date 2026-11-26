//! Explicit store path. Browsing never calls this composition.
use crate::{GuiArgs, PreparedGui, browser::input};
use archive_domain::*;
use archive_index::*;
use archive_infra::{ManagedIndex, S3Store};
use archive_sync::*;
use std::{num::NonZeroU32, sync::Arc};

pub enum Target {
    Top,
    Selected(Box<IndexEntry>),
    Roots(Arc<dyn IndexListing>),
}
pub async fn execute(
    args: &GuiArgs,
    prepared: &PreparedGui,
    target: Target,
    depth: Option<NonZeroU32>,
    observer: &dyn ReconcileObserver,
    cancel: &CancelContext,
) -> Result<ReconcileSummary, OperationError> {
    cancel.check("gui-reconcile")?;
    let target = if matches!(target, Target::Top) {
        let control = BrowseControl::default();
        let stop = control.clone();
        let signal = cancel.clone();
        let bridge = CancelBridge(tokio::spawn(async move {
            signal.cancelled().await;
            stop.cancel();
        }));
        let path = archive_infra::prepare_index_path(
            &prepared.paths.home,
            &prepared.settings.index_location.value,
            &prepared.namespace,
        )?;
        let index = archive_infra::SqliteIndex::open(path).await?;
        let listing = index
            .browse(
                BrowseRequest {
                    target: BrowseTarget::Roots { ignore: vec![] },
                    projection: BrowseProjection::Exact,
                    sort: SortSpec::default(),
                },
                control,
            )
            .await;
        let closed = index.close().await;
        drop(bridge);
        let listing = listing?;
        closed?;
        Target::Roots(Arc::from(listing))
    } else {
        target
    };
    if matches!(&target, Target::Roots(listing) if listing.summary().entries == 0) {
        return Err(input("再照合する既知バケットがありません"));
    }
    match &target {
        Target::Selected(entry) if entry.id.namespace != prepared.namespace => {
            return Err(input("選択対象は現在の接続先の記録ではありません"));
        }
        Target::Roots(listing) => {
            let mut start = 0;
            loop {
                let page = listing.page(start, crate::PAGE_SIZE).await?;
                if page
                    .entries
                    .iter()
                    .any(|e| e.id.namespace != prepared.namespace)
                {
                    return Err(input("既知バケットの記録が現在の接続先と異なります"));
                }
                match page.next {
                    Some(next) => start = next,
                    None => break,
                }
            }
        }
        _ => {}
    }
    cancel.check("gui-reconcile")?;
    #[cfg(feature = "test-fixtures")]
    let store = match &args.test_endpoint {
        Some(endpoint) => S3Store::for_test(endpoint)?,
        None => S3Store::from_env(cancel).await?,
    };
    #[cfg(not(feature = "test-fixtures"))]
    let store = {
        let _ = args;
        S3Store::from_env(cancel).await?
    };
    // Validate only the selected scope identities, never fetch bucket discovery.
    let index = ManagedIndex::open(
        &prepared.paths.home,
        &prepared.settings.index_location.value,
        &store,
        cancel,
    )
    .await?;
    let result = async {
        if let Some(issue) = index.issues().first() {
            return Err(issue.clone());
        }
        let loader = store.loader();
        let engine = ReconcileEngine {
            loader: &loader,
            index: index.index(),
        };
        let mut summary = ReconcileSummary::default();
        let mut reconcile = async |entry: IndexEntry| {
            cancel.check("gui-reconcile")?;
            if entry.id.namespace != store.namespace() {
                return Err(input("選択対象は現在の接続先の記録ではありません"));
            }
            let exact = entry.kind == IndexEntryKind::Object;
            let mut selection = Selection::new(
                Location::Remote(entry.id),
                !exact,
                if exact { None } else { depth },
                &prepared.paths.cwd,
            )?;
            if exact {
                selection.max_depth = None;
            }
            let result = engine
                .reconcile(
                    &ReconcileRequest {
                        selection,
                        ignore: prepared.settings.ignore.value.clone(),
                    },
                    observer,
                    cancel,
                )
                .await?;
            summary.observed += result.observed;
            summary.removed += result.removed;
            summary.ignored += result.ignored;
            Ok::<_, OperationError>(())
        };
        match target {
            Target::Top => unreachable!(),
            Target::Selected(entry) => reconcile(*entry).await?,
            Target::Roots(listing) => {
                let mut start = 0;
                loop {
                    let page = listing.page(start, crate::PAGE_SIZE).await?;
                    for entry in page.entries {
                        reconcile(entry).await?;
                    }
                    match page.next {
                        Some(next) => start = next,
                        None => break,
                    }
                }
            }
        }
        Ok(summary)
    }
    .await;
    if result.is_err() {
        cancel.record_failure();
    }
    let issues = index.finish(cancel).await;
    match result {
        Err(mut error) => {
            for issue in issues {
                error.context.causes.push(issue.to_string());
            }
            Err(error)
        }
        Ok(summary) => match issues.into_iter().next() {
            Some(issue) => Err(issue),
            None => Ok(summary),
        },
    }
}

struct CancelBridge(tokio::task::JoinHandle<()>);
impl Drop for CancelBridge {
    fn drop(&mut self) {
        self.0.abort();
    }
}
