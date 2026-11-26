use archive_domain::*;
use archive_index::*;
use archive_infra::SqliteIndex;
use archive_sync::*;
use std::{
    collections::VecDeque,
    num::NonZeroU32,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};

fn id(key: &str) -> ObjectId {
    ObjectId::parse_s3(&format!("s3://bucket/{key}")).unwrap()
}
fn metadata(key: &str, query: u64) -> ObjectMetadata {
    let mut m = ObjectMetadata::file(Location::Remote(id(key)), 1);
    m.queried_at = Some(UNIX_EPOCH + Duration::from_secs(query));
    m.version_token = Some("v1".into());
    m
}
async fn put(db: &SqliteIndex, m: ObjectMetadata) {
    db.apply(MetadataPatch::full(m).unwrap()).await.unwrap();
}
fn request(key: &str, recursive: bool, ignore: &[&str]) -> ReconcileRequest {
    ReconcileRequest {
        selection: Selection::new(Location::Remote(id(key)), recursive, None, Path::new("/"))
            .unwrap(),
        ignore: ignore.iter().map(|v| v.to_string()).collect(),
    }
}
#[derive(Clone)]
enum Step {
    Object(Box<ObjectMetadata>),
    Fail(ErrorKind),
    Cancel,
    UnconfirmedSkip,
}
struct FakeLoader {
    steps: Vec<Step>,
    enumerations: AtomicUsize,
    next: Arc<AtomicUsize>,
    bodies: AtomicUsize,
}
impl FakeLoader {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps,
            enumerations: AtomicUsize::new(0),
            next: Arc::new(AtomicUsize::new(0)),
            bodies: AtomicUsize::new(0),
        }
    }
}
struct Cursor {
    steps: VecDeque<Step>,
    calls: Arc<AtomicUsize>,
}
impl EntryCursor for Cursor {
    fn next<'a>(
        &'a mut self,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Option<EnumerationItem>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.steps.pop_front() {
                Some(Step::Object(m)) => Ok(Some(EnumerationItem::Entry(m))),
                Some(Step::Fail(kind)) => Err(OperationError::new(
                    kind,
                    "fake-list",
                    "injected lookup failure",
                )),
                Some(Step::Cancel) => {
                    cancel.cancel();
                    Err(OperationError::new(
                        ErrorKind::Cancelled,
                        "fake-list",
                        "cancelled",
                    ))
                }
                Some(Step::UnconfirmedSkip) => Ok(Some(EnumerationItem::Skipped(Warning {
                    category: WarningCategory::Verification,
                    stage: "fake-list",
                    uri: "s3://bucket/p/a".into(),
                    code: None,
                    message: "not obtained".into(),
                }))),
                None => Ok(None),
            }
        })
    }
}
impl Loader for FakeLoader {
    fn stat<'a>(&'a self, _: &'a Location, _: &'a CancelContext) -> PortFuture<'a, ObjectMetadata> {
        Box::pin(async {
            Err(OperationError::new(
                ErrorKind::Unsupported,
                "fake-stat",
                "enumeration owns observations",
            ))
        })
    }
    fn open<'a>(&'a self, _: &'a Location, _: &'a CancelContext) -> PortFuture<'a, OpenedSource> {
        self.bodies.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(OperationError::new(
                ErrorKind::Unsupported,
                "fake-open",
                "body must not be read",
            ))
        })
    }
    fn enumerate<'a>(
        &'a self,
        _: &'a Selection,
        _: &'a IgnoreFilter,
        _: &'a CancelContext,
    ) -> PortFuture<'a, Box<dyn EntryCursor>> {
        self.enumerations.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(Box::new(Cursor {
                steps: self.steps.clone().into(),
                calls: self.next.clone(),
            }) as Box<dyn EntryCursor>)
        })
    }
}

#[tokio::test]
async fn fresh_records_add_update_and_prune_only_successfully_observed_scope() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(dir.path().join("index.db"))
        .await
        .unwrap();
    for key in ["p/stale", "p/ignored", "p/deep/keep", "q/outside"] {
        put(&db, metadata(key, 1)).await;
    }
    let mut old = metadata("p/shared", 1);
    old.original_mtime = Some(UNIX_EPOCH - Duration::from_secs(9));
    put(&db, old).await;
    let source = FakeLoader::new(vec![
        Step::Object(Box::new(metadata("p/new", 2))),
        Step::Object(Box::new(metadata("p/shared", 2))),
        Step::Object(Box::new(metadata("p/ignored", 2))),
    ]);
    let mut req = request("p", true, &["ignored"]);
    req.selection.max_depth = NonZeroU32::new(1);
    let summary = ReconcileEngine {
        loader: &source,
        index: &db,
    }
    .reconcile(&req, &(), &CancelContext::new())
    .await
    .unwrap();
    assert_eq!(
        (summary.observed, summary.removed, summary.ignored),
        (2, 1, 1)
    );
    assert!(db.get(&id("p/stale")).await.unwrap().is_none());
    for key in ["p/new", "p/shared", "p/ignored", "p/deep/keep", "q/outside"] {
        assert!(db.get(&id(key)).await.unwrap().is_some());
    }
    let shared = db.get(&id("p/shared")).await.unwrap().unwrap();
    assert_eq!(
        shared.original_mtime,
        Some(UNIX_EPOCH - Duration::from_secs(9))
    );
    assert_eq!(shared.queried_at, Some(UNIX_EPOCH + Duration::from_secs(2)));
    assert_eq!(
        db.get(&id("p/ignored")).await.unwrap().unwrap().queried_at,
        Some(UNIX_EPOCH + Duration::from_secs(1))
    );
    assert_eq!(source.enumerations.load(Ordering::SeqCst), 1);
    assert_eq!(source.bodies.load(Ordering::SeqCst), 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn confirmed_empty_and_exact_missing_are_success_but_do_not_delete_other_scopes() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(dir.path().join("index.db"))
        .await
        .unwrap();
    put(&db, metadata("p/stale", 1)).await;
    put(&db, metadata("q/kept", 1)).await;
    let empty = FakeLoader::new(vec![]);
    let summary = ReconcileEngine {
        loader: &empty,
        index: &db,
    }
    .reconcile(&request("p", true, &[]), &(), &CancelContext::new())
    .await
    .unwrap();
    assert_eq!((summary.observed, summary.removed), (0, 1));
    assert!(db.get(&id("q/kept")).await.unwrap().is_some());
    put(&db, metadata("p/file", 1)).await;
    put(&db, metadata("p/file/child", 1)).await;
    let missing = FakeLoader::new(vec![Step::Fail(ErrorKind::NotFound)]);
    let summary = ReconcileEngine {
        loader: &missing,
        index: &db,
    }
    .reconcile(&request("p/file", false, &[]), &(), &CancelContext::new())
    .await
    .unwrap();
    assert_eq!((summary.observed, summary.removed), (0, 1));
    assert!(db.get(&id("p/file/child")).await.unwrap().is_some());
    db.close().await.unwrap();
}

#[tokio::test]
async fn all_ignored_errors_do_not_prune_and_explicit_ignore_avoids_store_calls() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(dir.path().join("index.db"))
        .await
        .unwrap();
    put(&db, metadata("p/stale", 1)).await;
    let source = FakeLoader::new(vec![Step::Object(Box::new(metadata("p/a.tmp", 2)))]);
    let error = ReconcileEngine {
        loader: &source,
        index: &db,
    }
    .reconcile(&request("p", true, &["*.tmp"]), &(), &CancelContext::new())
    .await
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotFound);
    assert!(db.get(&id("p/stale")).await.unwrap().is_some());
    assert!(db.get(&id("p/a.tmp")).await.unwrap().is_none());
    let source = FakeLoader::new(vec![Step::Fail(ErrorKind::Permission)]);
    let error = ReconcileEngine {
        loader: &source,
        index: &db,
    }
    .reconcile(
        &request("p/stale", false, &["stale"]),
        &(),
        &CancelContext::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotFound);
    assert_eq!(source.enumerations.load(Ordering::SeqCst), 0);
    assert!(db.get(&id("p/stale")).await.unwrap().is_some());
    // The failed sessions have been released; another explicit run can finish.
    let empty = FakeLoader::new(vec![]);
    assert_eq!(
        ReconcileEngine {
            loader: &empty,
            index: &db
        }
        .reconcile(&request("p", true, &[]), &(), &CancelContext::new())
        .await
        .unwrap()
        .removed,
        1
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn failure_cancel_unknown_skip_and_out_of_scope_observations_never_prune() {
    for last in [
        Step::Fail(ErrorKind::Unavailable),
        Step::Fail(ErrorKind::Permission),
        Step::Cancel,
        Step::UnconfirmedSkip,
        Step::Object(Box::new(metadata("q/outside", 2))),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteIndex::open(dir.path().join("index.db"))
            .await
            .unwrap();
        put(&db, metadata("p/stale", 1)).await;
        let source = FakeLoader::new(vec![
            Step::Object(Box::new(metadata("p/new", 2))),
            last,
            Step::Object(Box::new(metadata("p/not-requested", 2))),
        ]);
        let cancel = CancelContext::new();
        let error = ReconcileEngine {
            loader: &source,
            index: &db,
        }
        .reconcile(&request("p", true, &[]), &(), &cancel)
        .await
        .unwrap_err();
        assert_ne!(error.kind, ErrorKind::NotFound);
        assert!(db.get(&id("p/stale")).await.unwrap().is_some());
        assert!(db.get(&id("p/new")).await.unwrap().is_some());
        assert!(db.get(&id("p/not-requested")).await.unwrap().is_none());
        assert!(db.get(&id("q/outside")).await.unwrap().is_none());
        assert_eq!(source.next.load(Ordering::SeqCst), 2);
        assert_eq!(source.bodies.load(Ordering::SeqCst), 0);
        db.close().await.unwrap();
    }
}

struct FailingIndex<'a> {
    db: &'a SqliteIndex,
    fail_finish: bool,
}
struct FailingSession {
    inner: Box<dyn Reconcile>,
    fail_finish: bool,
}
impl IndexWrite for FailingIndex<'_> {
    fn apply<'a>(&'a self, p: MetadataPatch) -> IndexFuture<'a, ()> {
        self.db.apply(p)
    }
    fn remove<'a>(&'a self, id: &'a ObjectId) -> IndexFuture<'a, ()> {
        self.db.remove(id)
    }
    fn begin_reconcile<'a>(&'a self, scope: IndexScope) -> IndexFuture<'a, Box<dyn Reconcile>> {
        Box::pin(async move {
            Ok(Box::new(FailingSession {
                inner: self.db.begin_reconcile(scope).await?,
                fail_finish: self.fail_finish,
            }) as Box<dyn Reconcile>)
        })
    }
}
impl Reconcile for FailingSession {
    fn control(&self) -> ReconcileControl {
        self.inner.control()
    }
    fn observe<'a>(&'a mut self, p: MetadataPatch) -> IndexFuture<'a, ()> {
        if !self.fail_finish && p.id.key.ends_with("/stop") {
            Box::pin(async {
                Err(OperationError::new(
                    ErrorKind::Io,
                    "injected-index",
                    "cannot write",
                ))
            })
        } else {
            self.inner.observe(p)
        }
    }
    fn finish<'a>(&'a mut self) -> IndexFuture<'a, u64> {
        if self.fail_finish {
            Box::pin(async {
                Err(OperationError::new(
                    ErrorKind::Io,
                    "injected-index",
                    "cannot finish",
                ))
            })
        } else {
            self.inner.finish()
        }
    }
    fn cancel<'a>(&'a mut self) -> IndexFuture<'a, ()> {
        self.inner.cancel()
    }
}
#[tokio::test]
async fn required_index_observation_and_finish_failures_are_errors_with_partial_updates_retained() {
    for fail_finish in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteIndex::open(dir.path().join("index.db"))
            .await
            .unwrap();
        put(&db, metadata("p/stale", 1)).await;
        let source = FakeLoader::new(vec![
            Step::Object(Box::new(metadata("p/new", 2))),
            Step::Object(Box::new(metadata("p/stop", 2))),
        ]);
        let failing = FailingIndex {
            db: &db,
            fail_finish,
        };
        let error = ReconcileEngine {
            loader: &source,
            index: &failing,
        }
        .reconcile(&request("p", true, &[]), &(), &CancelContext::new())
        .await
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Io);
        assert!(db.get(&id("p/new")).await.unwrap().is_some());
        assert!(db.get(&id("p/stale")).await.unwrap().is_some());
        assert_eq!(db.get(&id("p/stop")).await.unwrap().is_some(), fail_finish);
        db.close().await.unwrap();
    }
}
