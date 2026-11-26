use archive_domain::*;
use archive_index::*;
use archive_infra::{DiskObjects, SqliteIndex};
use archive_sync::*;
use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::UNIX_EPOCH,
};
#[path = "support/cleanup_plan.rs"]
mod cleanup_plan;

#[tokio::test(start_paused = true)]
async fn successful_deletion_allows_disposal_beyond_three_seconds() {
    let source = loader(&["p/a"]);
    let remote = Remote::default();
    let observer = Observer::default();
    let mut plan = cleanup_plan::DelayedPlan::new(std::time::Duration::from_secs(4));
    let result = RemoveEngine {
        loader: &source,
        remote: &remote,
        index: Err(OperationError::new(
            ErrorKind::Io,
            "index",
            "test omitted index",
        )),
    }
    .remove(
        &request(true, vec![]),
        &mut plan,
        &observer,
        &CancelContext::new(),
    )
    .await
    .unwrap();
    assert_eq!((result.resolved, result.removed), (1, 1));
    assert!(plan.cleaned);
}

#[tokio::test(start_paused = true)]
async fn delayed_disposal_keeps_first_stop_budget_and_reserves_finalization_time() {
    let source = loader(&["p/a"]);
    let remote = Remote::default();
    let cancel = CancelContext::new();
    cancel.cancel();
    let first = cancel.stopped_at();
    tokio::time::advance(std::time::Duration::from_secs(44)).await;
    let start = tokio::time::Instant::now();
    let mut plan = cleanup_plan::DelayedPlan::new(std::time::Duration::from_secs(4));
    let result = RemoveEngine {
        loader: &source,
        remote: &remote,
        index: Err(OperationError::new(
            ErrorKind::Io,
            "index",
            "test omitted index",
        )),
    }
    .remove(
        &request(true, vec![]),
        &mut plan,
        &Observer::default(),
        &cancel,
    )
    .await;
    assert_eq!(result.err().unwrap().kind, ErrorKind::Cancelled);
    assert!(!plan.cleaned);
    assert_eq!(
        tokio::time::Instant::now() - start,
        std::time::Duration::from_secs(3)
    );
    assert_eq!(cancel.stopped_at(), first);
    assert!(remote.calls.lock().unwrap().is_empty());
}
fn id(key: &str) -> ObjectId {
    ObjectId::parse_s3(&format!("s3://bucket/{key}")).unwrap()
}
fn meta(key: &str) -> ObjectMetadata {
    let mut m = ObjectMetadata::file(Location::Remote(id(key)), 1);
    m.queried_at = Some(UNIX_EPOCH);
    m
}
fn request(auto: bool, ignore: Vec<String>) -> RemoveRequest {
    RemoveRequest {
        protected: None,
        selection: Selection::new(Location::Remote(id("p")), true, None, Path::new("/")).unwrap(),
        ignore,
        auto_approve: auto,
    }
}
struct LoaderFake {
    entries: Vec<ObjectMetadata>,
    fail_after: Option<usize>,
    calls: AtomicUsize,
}
struct Cursor {
    entries: VecDeque<ObjectMetadata>,
    fail_after: Option<usize>,
    n: usize,
}
impl EntryCursor for Cursor {
    fn next<'a>(
        &'a mut self,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Option<EnumerationItem>> {
        Box::pin(async move {
            cancel.check("fake-list")?;
            self.n += 1;
            if self.fail_after == Some(self.n) {
                return Err(OperationError::new(
                    ErrorKind::Unavailable,
                    "fake-list",
                    "later page failed",
                ));
            }
            Ok(self
                .entries
                .pop_front()
                .map(|m| EnumerationItem::Entry(Box::new(m))))
        })
    }
}
impl Loader for LoaderFake {
    fn enumerate<'a>(
        &'a self,
        _: &'a Selection,
        _: &'a IgnoreFilter,
        _: &'a CancelContext,
    ) -> PortFuture<'a, Box<dyn EntryCursor>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Cursor {
                entries: self.entries.clone().into(),
                fail_after: self.fail_after,
                n: 0,
            }) as Box<dyn EntryCursor>)
        })
    }
    fn stat<'a>(&'a self, _: &'a Location, _: &'a CancelContext) -> PortFuture<'a, ObjectMetadata> {
        Box::pin(async { panic!("enumeration already supplied fresh targets") })
    }
    fn open<'a>(&'a self, _: &'a Location, _: &'a CancelContext) -> PortFuture<'a, OpenedSource> {
        Box::pin(async { panic!("delete must not download bodies") })
    }
}
#[derive(Default)]
struct Remote {
    calls: Mutex<Vec<String>>,
    fail_second: bool,
    cancel_first: bool,
}
impl RemoteRemove for Remote {
    fn remove<'a>(&'a self, id: &'a ObjectId, cancel: &'a CancelContext) -> PortFuture<'a, ()> {
        Box::pin(async move {
            cancel.check("fake-delete")?;
            let n = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(id.key.clone());
                calls.len()
            };
            if self.fail_second && n == 2 {
                let mut e =
                    OperationError::new(ErrorKind::Unavailable, "fake-delete", "response lost");
                e.outcome = OperationState::Unknown;
                return Err(e);
            }
            if self.cancel_first && n == 1 {
                cancel.cancel();
            }
            Ok(())
        })
    }
}
#[derive(Default)]
struct Observer {
    approve: bool,
    confirmations: Mutex<Vec<ObjectPlanSummary>>,
    removed: Mutex<Vec<String>>,
    warnings: Mutex<Vec<Warning>>,
}
impl WarningSink for Observer {
    fn warn<'a>(&'a self, w: Warning) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.warnings.lock().unwrap().push(w);
            Ok(())
        })
    }
}
impl RemoveObserver for Observer {
    fn confirm<'a>(
        &'a self,
        root: Location,
        s: ObjectPlanSummary,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, bool> {
        Box::pin(async move {
            cancel.check("fake-confirm")?;
            assert_eq!(root.display(), "s3://bucket/p/");
            self.confirmations.lock().unwrap().push(s);
            Ok(self.approve)
        })
    }
    fn removed<'a>(&'a self, l: Location) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.removed.lock().unwrap().push(l.display());
            Ok(())
        })
    }
}
fn loader(keys: &[&str]) -> LoaderFake {
    LoaderFake {
        entries: keys.iter().map(|k| meta(k)).collect(),
        fail_after: None,
        calls: AtomicUsize::new(0),
    }
}
#[tokio::test]
async fn resolves_fresh_then_confirms_once_and_deletes_key_order_preserving_ignored_records() {
    let temp = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(temp.path().join("index.db"))
        .await
        .unwrap();
    for k in ["outside/stale", "p/keep.tmp"] {
        db.apply(MetadataPatch::full(meta(k)).unwrap())
            .await
            .unwrap();
    }
    let source = loader(&["p/z", "p/keep.tmp", "p/a"]);
    let remote = Remote::default();
    let observer = Observer {
        approve: true,
        ..Default::default()
    };
    let mut plan = DiskObjects::new().await.unwrap();
    let result = RemoveEngine {
        loader: &source,
        remote: &remote,
        index: Ok(&db),
    }
    .remove(
        &request(false, vec!["*.tmp".into()]),
        &mut plan,
        &observer,
        &CancelContext::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.removed, 2);
    assert_eq!(result.ignored, 1);
    assert_eq!(*remote.calls.lock().unwrap(), vec!["p/a", "p/z"]);
    {
        let confirmations = observer.confirmations.lock().unwrap();
        assert_eq!(confirmations.len(), 1);
        assert_eq!(confirmations[0].objects, 2);
        assert_eq!(confirmations[0].total_bytes, Some(2));
    }
    assert!(db.get(&id("p/a")).await.unwrap().is_none());
    assert!(db.get(&id("p/keep.tmp")).await.unwrap().is_some());
    assert!(db.get(&id("outside/stale")).await.unwrap().is_some());
    db.close().await.unwrap();
}
#[tokio::test]
async fn reject_late_enumeration_failure_and_all_ignored_never_delete() {
    for case in 0..3 {
        let mut source = loader(&["p/a", "p/b"]);
        if case == 1 {
            source.fail_after = Some(2);
        }
        let remote = Remote::default();
        let observer = Observer::default();
        let mut plan = DiskObjects::new().await.unwrap();
        let req = request(false, if case == 2 { vec!["*".into()] } else { vec![] });
        assert!(
            RemoveEngine {
                loader: &source,
                remote: &remote,
                index: Err(OperationError::new(ErrorKind::Io, "index", "unavailable"))
            }
            .remove(&req, &mut plan, &observer, &CancelContext::new())
            .await
            .is_err()
        );
        assert!(remote.calls.lock().unwrap().is_empty());
        assert_eq!(
            observer.confirmations.lock().unwrap().len(),
            if case == 0 { 1 } else { 0 }
        );
    }
}
#[tokio::test]
async fn first_failed_or_cancelled_delete_keeps_confirmed_output_and_index_removal_only() {
    for cancelled in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let db = SqliteIndex::open(temp.path().join("index.db"))
            .await
            .unwrap();
        let source = loader(&["p/c", "p/b", "p/a"]);
        let remote = Remote {
            fail_second: !cancelled,
            cancel_first: cancelled,
            ..Default::default()
        };
        let observer = Observer::default();
        let mut plan = DiskObjects::new().await.unwrap();
        let error = RemoveEngine {
            loader: &source,
            remote: &remote,
            index: Ok(&db),
        }
        .remove(
            &request(true, vec![]),
            &mut plan,
            &observer,
            &CancelContext::new(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            remote.calls.lock().unwrap().len(),
            if cancelled { 1 } else { 2 }
        );
        assert_eq!(*observer.removed.lock().unwrap(), vec!["s3://bucket/p/a"]);
        assert!(observer.confirmations.lock().unwrap().is_empty());
        assert!(db.get(&id("p/a")).await.unwrap().is_none());
        assert!(db.get(&id("p/b")).await.unwrap().is_some());
        assert_eq!(
            error.kind,
            if cancelled {
                ErrorKind::Cancelled
            } else {
                ErrorKind::Unavailable
            }
        );
        if !cancelled {
            assert_eq!(error.outcome, OperationState::Unknown);
        }
        db.close().await.unwrap();
    }
}
#[tokio::test]
async fn last_record_removal_removes_root_and_index_failure_does_not_block_data_deletion() {
    let temp = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(temp.path().join("index.db"))
        .await
        .unwrap();
    let source = loader(&["p/a"]);
    for attached in [true, false] {
        let remote = Remote::default();
        let observer = Observer::default();
        let mut plan = DiskObjects::new().await.unwrap();
        let index = if attached {
            Ok(&db as &dyn IndexWrite)
        } else {
            Err(OperationError::new(ErrorKind::Io, "index", "unavailable"))
        };
        assert_eq!(
            RemoveEngine {
                loader: &source,
                remote: &remote,
                index
            }
            .remove(
                &request(true, vec![]),
                &mut plan,
                &observer,
                &CancelContext::new()
            )
            .await
            .unwrap()
            .removed,
            1
        );
        assert_eq!(
            observer
                .warnings
                .lock()
                .unwrap()
                .iter()
                .filter(|w| w.category == WarningCategory::Index)
                .count(),
            if attached { 0 } else { 1 }
        );
        assert!(db.roots(None, 256).await.unwrap().roots.is_empty());
    }
    db.close().await.unwrap();
}
