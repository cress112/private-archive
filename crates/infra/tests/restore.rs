use archive_domain::*;
use archive_index::*;
use archive_infra::{DiskObjects, SqliteIndex};
use archive_sync::*;
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};
#[path = "support/cleanup_plan.rs"]
mod cleanup_plan;

#[tokio::test(start_paused = true)]
async fn successful_restoration_allows_disposal_beyond_three_seconds() {
    let mut source = LoaderFake::default();
    source
        .roots
        .insert("s3://bucket/p/".into(), vec![meta("p/a")]);
    let remote = Remote::default();
    let observer = Observer::default();
    let mut plan = cleanup_plan::DelayedPlan::new(Duration::from_secs(4));
    let result = RestoreEngine {
        loader: &source,
        remote: &remote,
        index: Err(OperationError::new(
            ErrorKind::Io,
            "index",
            "test omitted index",
        )),
    }
    .restore(
        &request(vec![selection("p", true)]),
        &mut plan,
        &observer,
        &CancelContext::new(),
    )
    .await
    .unwrap();
    assert_eq!((result.resolved, result.accepted), (1, 1));
    assert!(plan.cleaned);
}

fn id(key: &str) -> ObjectId {
    ObjectId::parse_s3(&format!("s3://bucket/{key}")).unwrap()
}
fn meta(key: &str) -> ObjectMetadata {
    let mut m = ObjectMetadata::file(Location::Remote(id(key)), 1);
    m.data_class = Some(StoredClass::Archived);
    m.restore_state = RestoreState::Archived;
    m.queried_at = Some(UNIX_EPOCH + Duration::from_secs(1));
    m.version_token = Some("v".into());
    m
}
fn selection(key: &str, recursive: bool) -> Selection {
    Selection::new(Location::Remote(id(key)), recursive, None, Path::new("/")).unwrap()
}
fn request(selections: Vec<Selection>) -> RestoreRequest {
    RestoreRequest {
        protected: None,
        selections,
        ignore: vec![],
        days: "1w".parse().unwrap(),
        tier: RestoreTier::Bulk,
    }
}
#[derive(Default)]
struct LoaderFake {
    roots: HashMap<String, Vec<ObjectMetadata>>,
    stats: HashMap<String, ObjectMetadata>,
    enumerations: AtomicUsize,
    heads: Mutex<Vec<String>>,
}
struct Cursor(VecDeque<ObjectMetadata>);
impl EntryCursor for Cursor {
    fn next<'a>(
        &'a mut self,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Option<EnumerationItem>> {
        Box::pin(async move {
            cancel.check("fake-next")?;
            Ok(self
                .0
                .pop_front()
                .map(|m| EnumerationItem::Entry(Box::new(m))))
        })
    }
}
impl Loader for LoaderFake {
    fn enumerate<'a>(
        &'a self,
        s: &'a Selection,
        _: &'a IgnoreFilter,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Box<dyn EntryCursor>> {
        Box::pin(async move {
            cancel.check("fake-list")?;
            self.enumerations.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Cursor(
                self.roots
                    .get(&s.root.display())
                    .cloned()
                    .unwrap_or_default()
                    .into(),
            )) as Box<dyn EntryCursor>)
        })
    }
    fn stat<'a>(
        &'a self,
        l: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ObjectMetadata> {
        Box::pin(async move {
            cancel.check("fake-head")?;
            self.heads.lock().unwrap().push(l.display());
            self.stats
                .get(&l.display())
                .cloned()
                .ok_or_else(|| OperationError::new(ErrorKind::Unavailable, "fake-head", "failed"))
        })
    }
    fn open<'a>(&'a self, _: &'a Location, _: &'a CancelContext) -> PortFuture<'a, OpenedSource> {
        Box::pin(async { panic!("restore must not download bodies") })
    }
}
#[derive(Default)]
struct Remote {
    calls: Mutex<Vec<String>>,
    fail: Option<usize>,
    cancel_first: bool,
}
impl RemoteRestore for Remote {
    fn restore<'a>(
        &'a self,
        m: &'a ObjectMetadata,
        days: RestoreDays,
        tier: RestoreTier,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, RestoreAccepted> {
        Box::pin(async move {
            cancel.check("fake-restore")?;
            assert_eq!(days.get(), 7);
            assert_eq!(tier, RestoreTier::Bulk);
            let n = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(m.location.display());
                calls.len()
            };
            if self.fail == Some(n) {
                let mut e =
                    OperationError::new(ErrorKind::Unavailable, "fake-restore", "response lost");
                e.outcome = OperationState::Unknown;
                return Err(e);
            }
            if self.cancel_first && n == 1 {
                cancel.cancel();
            }
            Ok(RestoreAccepted {
                queried_at: UNIX_EPOCH + Duration::from_secs(100),
            })
        })
    }
}
#[derive(Default)]
struct Observer {
    accepted: Mutex<Vec<String>>,
    warnings: Mutex<Vec<Warning>>,
    summary: Mutex<Option<RestoreSummary>>,
}
impl WarningSink for Observer {
    fn warn<'a>(&'a self, w: Warning) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.warnings.lock().unwrap().push(w);
            Ok(())
        })
    }
}
impl RestoreObserver for Observer {
    fn accepted<'a>(&'a self, l: Location, _: RestoreDays, _: RestoreTier) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.accepted.lock().unwrap().push(l.display());
            Ok(())
        })
    }
    fn finished<'a>(&'a self, s: RestoreSummary) -> PortFuture<'a, ()> {
        Box::pin(async move {
            *self.summary.lock().unwrap() = Some(s);
            Ok(())
        })
    }
}
#[tokio::test]
async fn ordered_deduplicated_all_uri_preflight_uses_fresh_store_and_updates_acceptance() {
    let temp = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(temp.path().join("index.db"))
        .await
        .unwrap();
    // Deliberately stale cache must not decide eligibility.
    let mut stale = meta("p/a");
    stale.restore_state = RestoreState::Restored { until: None };
    db.apply(MetadataPatch::full(stale).unwrap()).await.unwrap();
    let mut loader = LoaderFake::default();
    loader
        .roots
        .insert("s3://bucket/p/".into(), vec![meta("p/z"), meta("p/a")]);
    loader
        .roots
        .insert("s3://bucket/p/z".into(), vec![meta("p/z")]);
    loader
        .roots
        .insert("s3://bucket/q/c".into(), vec![meta("q/c")]);
    let remote = Remote::default();
    let observer = Observer::default();
    let mut plan = DiskObjects::new().await.unwrap();
    let result = RestoreEngine {
        loader: &loader,
        remote: &remote,
        index: Ok(&db),
    }
    .restore(
        &request(vec![
            selection("p", true),
            selection("p/z", false),
            selection("q/c", false),
        ]),
        &mut plan,
        &observer,
        &CancelContext::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.accepted, 3);
    assert_eq!(result.resolved, 3);
    assert_eq!(
        *remote.calls.lock().unwrap(),
        vec!["s3://bucket/p/a", "s3://bucket/p/z", "s3://bucket/q/c"]
    );
    assert!(loader.heads.lock().unwrap().is_empty());
    assert_eq!(
        db.get(&id("p/a")).await.unwrap().unwrap().restore_state,
        RestoreState::Restoring
    );
    assert!(plan.next(&CancelContext::new()).await.is_err());
    db.close().await.unwrap();
}
#[tokio::test]
async fn every_uri_is_resolved_before_known_ineligible_state_fails_with_zero_requests() {
    for state in [
        RestoreState::Restoring,
        RestoreState::Restored { until: None },
        RestoreState::NotRequired,
    ] {
        let mut loader = LoaderFake::default();
        let mut bad = meta("bad");
        bad.restore_state = state;
        loader
            .roots
            .insert("s3://bucket/valid".into(), vec![meta("valid")]);
        loader.roots.insert("s3://bucket/bad".into(), vec![bad]);
        let remote = Remote::default();
        let observer = Observer::default();
        let mut plan = DiskObjects::new().await.unwrap();
        let error = RestoreEngine {
            loader: &loader,
            remote: &remote,
            index: Err(OperationError::new(ErrorKind::Io, "index", "unavailable")),
        }
        .restore(
            &request(vec![selection("valid", false), selection("bad", false)]),
            &mut plan,
            &observer,
            &CancelContext::new(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert_eq!(loader.enumerations.load(Ordering::SeqCst), 2);
        assert!(remote.calls.lock().unwrap().is_empty());
        assert!(loader.heads.lock().unwrap().is_empty());
        assert!(observer.accepted.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn only_unknown_objects_receive_additional_head_and_unresolved_or_failed_head_blocks_all() {
    for outcome in 0..3 {
        let mut loader = LoaderFake::default();
        let mut unknown = meta("p/b");
        unknown.restore_state = RestoreState::Unknown;
        unknown.data_class = None;
        loader
            .roots
            .insert("s3://bucket/p/".into(), vec![meta("p/a"), unknown.clone()]);
        if outcome < 2 {
            loader.stats.insert(
                "s3://bucket/p/b".into(),
                if outcome == 0 { meta("p/b") } else { unknown },
            );
        }
        let remote = Remote::default();
        let observer = Observer::default();
        let mut plan = DiskObjects::new().await.unwrap();
        let result = RestoreEngine {
            loader: &loader,
            remote: &remote,
            index: Err(OperationError::new(ErrorKind::Io, "index", "unavailable")),
        }
        .restore(
            &request(vec![selection("p", true)]),
            &mut plan,
            &observer,
            &CancelContext::new(),
        )
        .await;
        assert_eq!(*loader.heads.lock().unwrap(), vec!["s3://bucket/p/b"]);
        if outcome == 0 {
            assert_eq!(result.unwrap().accepted, 2);
            assert_eq!(remote.calls.lock().unwrap().len(), 2);
            assert_eq!(
                observer
                    .warnings
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|w| w.category == WarningCategory::Index)
                    .count(),
                1
            );
        } else {
            assert!(result.is_err());
            assert!(remote.calls.lock().unwrap().is_empty());
        }
    }
}
#[tokio::test]
async fn first_mutation_failure_or_cancel_stops_next_request_and_retains_confirmed_acceptance() {
    for cancel_first in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let db = SqliteIndex::open(temp.path().join("index.db"))
            .await
            .unwrap();
        let mut loader = LoaderFake::default();
        loader.roots.insert(
            "s3://bucket/p/".into(),
            vec![meta("p/a"), meta("p/b"), meta("p/c")],
        );
        let remote = Remote {
            cancel_first,
            fail: if cancel_first { None } else { Some(2) },
            ..Default::default()
        };
        let observer = Observer::default();
        let mut plan = DiskObjects::new().await.unwrap();
        let cancel = CancelContext::new();
        let error = RestoreEngine {
            loader: &loader,
            remote: &remote,
            index: Ok(&db),
        }
        .restore(
            &request(vec![selection("p", true)]),
            &mut plan,
            &observer,
            &cancel,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            remote.calls.lock().unwrap().len(),
            if cancel_first { 1 } else { 2 }
        );
        assert_eq!(*observer.accepted.lock().unwrap(), vec!["s3://bucket/p/a"]);
        assert_eq!(
            db.get(&id("p/a")).await.unwrap().unwrap().restore_state,
            RestoreState::Restoring
        );
        assert_eq!(
            db.get(&id("p/b")).await.unwrap().unwrap().restore_state,
            RestoreState::Archived
        );
        assert_eq!(
            error.kind,
            if cancel_first {
                ErrorKind::Cancelled
            } else {
                ErrorKind::Unavailable
            }
        );
        if !cancel_first {
            assert_eq!(error.outcome, OperationState::Unknown);
        }
        db.close().await.unwrap();
    }
}
#[tokio::test]
async fn ignored_direct_and_recursive_empty_targets_are_errors_without_request_or_index_removal() {
    let temp = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(temp.path().join("index.db"))
        .await
        .unwrap();
    db.apply(MetadataPatch::full(meta("p/a")).unwrap())
        .await
        .unwrap();
    for recursive in [false, true] {
        let mut loader = LoaderFake::default();
        loader
            .roots
            .insert("s3://bucket/p/a".into(), vec![meta("p/a")]);
        let remote = Remote::default();
        let observer = Observer::default();
        let mut plan = DiskObjects::new().await.unwrap();
        let mut req = request(vec![selection(
            if recursive { "p" } else { "p/a" },
            recursive,
        )]);
        req.ignore = vec!["*".into()];
        assert!(
            RestoreEngine {
                loader: &loader,
                remote: &remote,
                index: Ok(&db)
            }
            .restore(&req, &mut plan, &observer, &CancelContext::new())
            .await
            .is_err()
        );
        if !recursive {
            assert_eq!(loader.enumerations.load(Ordering::SeqCst), 0);
        }
        assert!(remote.calls.lock().unwrap().is_empty());
        assert!(db.get(&id("p/a")).await.unwrap().is_some());
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn duplicate_observations_cannot_hide_ineligible_or_newly_unknown_state() {
    for scenario in 0..3 {
        let mut first = meta("p/a");
        let mut later = first.clone();
        if scenario == 0 {
            later.restore_state = RestoreState::Restoring;
        }
        if scenario == 1 {
            first.restore_state = RestoreState::Restoring;
        }
        if scenario == 2 {
            later.restore_state = RestoreState::Unknown;
            later.data_class = None;
        }
        let mut loader = LoaderFake::default();
        loader.roots.insert("s3://bucket/p/".into(), vec![first]);
        loader.roots.insert("s3://bucket/p/a".into(), vec![later]);
        let remote = Remote::default();
        let observer = Observer::default();
        let mut plan = DiskObjects::new().await.unwrap();
        assert!(
            RestoreEngine {
                loader: &loader,
                remote: &remote,
                index: Err(OperationError::new(ErrorKind::Io, "index", "unused"))
            }
            .restore(
                &request(vec![selection("p", true), selection("p/a", false)]),
                &mut plan,
                &observer,
                &CancelContext::new()
            )
            .await
            .is_err()
        );
        assert!(remote.calls.lock().unwrap().is_empty());
        assert_eq!(loader.enumerations.load(Ordering::SeqCst), 2);
        assert_eq!(
            loader.heads.lock().unwrap().len(),
            if scenario == 2 { 1 } else { 0 }
        );
    }
}
