use archive_domain::*;
use archive_index::*;
use archive_infra::{DiskPlan, LocalLoader, LocalSaver, SqliteIndex};
use archive_sync::*;
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::SystemTime,
};

fn remote(key: &str) -> Location {
    Location::Remote(ObjectId::parse_s3(&format!("s3://bucket/{key}")).unwrap())
}
fn key(location: &Location) -> String {
    let Location::Remote(id) = location else {
        panic!()
    };
    id.key.clone()
}
fn meta(location: Location, body: &[u8]) -> ObjectMetadata {
    let mut m = ObjectMetadata::file(location, body.len() as u64);
    m.queried_at = Some(SystemTime::now());
    m.version_token = Some(format!("size-{}", body.len()));
    m
}
fn failure(stage: &'static str) -> OperationError {
    OperationError::new(ErrorKind::Unavailable, stage, "injected operation failure")
}
#[derive(Default, Clone)]
struct MemoryStore {
    objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    events: Arc<Mutex<Vec<String>>>,
    fail_stat: Option<String>,
    fail_save: Option<String>,
    cancel_after_commit: Option<CancelContext>,
}
impl MemoryStore {
    fn put(&self, key: &str, value: &[u8]) {
        self.objects
            .lock()
            .unwrap()
            .insert(key.into(), value.into());
    }
    fn body_calls(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with("open:") || s.starts_with("begin:") || s.starts_with("copy:"))
            .cloned()
            .collect()
    }
    fn engine<'a>(&'a self, index: Result<&'a dyn IndexWrite, OperationError>) -> CopyEngine<'a> {
        CopyEngine {
            local_loader: &LocalLoader,
            local_saver: &LocalSaver,
            remote_loader: self,
            remote_saver: self,
            remote_copy: self,
            index,
        }
    }
}
struct Cursor(VecDeque<ObjectMetadata>);
impl EntryCursor for Cursor {
    fn next<'a>(
        &'a mut self,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Option<EnumerationItem>> {
        Box::pin(async move {
            cancel.check("fake-list")?;
            Ok(self
                .0
                .pop_front()
                .map(|m| EnumerationItem::Entry(Box::new(m))))
        })
    }
}
impl Loader for MemoryStore {
    fn stat<'a>(
        &'a self,
        location: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ObjectMetadata> {
        Box::pin(async move {
            cancel.check("fake-stat")?;
            let k = key(location);
            self.events.lock().unwrap().push(format!("stat:{k}"));
            if self.fail_stat.as_ref() == Some(&k) {
                return Err(failure("fake-stat"));
            }
            let data = self.objects.lock().unwrap();
            let value = data
                .get(&k)
                .ok_or_else(|| OperationError::new(ErrorKind::NotFound, "fake-stat", "missing"))?;
            Ok(meta(location.clone(), value))
        })
    }
    fn enumerate<'a>(
        &'a self,
        selection: &'a Selection,
        _ignore: &'a IgnoreFilter,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Box<dyn EntryCursor>> {
        Box::pin(async move {
            cancel.check("fake-list")?;
            let objects = self.objects.lock().unwrap();
            let entries = objects
                .iter()
                .map(|(k, v)| meta(remote(k), v))
                .filter(|m| selection.contains(&m.location))
                .collect();
            Ok(Box::new(Cursor(entries)) as Box<dyn EntryCursor>)
        })
    }
    fn open<'a>(
        &'a self,
        location: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, OpenedSource> {
        Box::pin(async move {
            let metadata = self.stat(location, cancel).await?;
            let k = key(location);
            self.events.lock().unwrap().push(format!("open:{k}"));
            let data = self.objects.lock().unwrap()[&k].clone();
            Ok(OpenedSource {
                metadata,
                body: Box::pin(std::io::Cursor::new(data)),
            })
        })
    }
}
struct Writer {
    store: MemoryStore,
    destination: Location,
    data: Vec<u8>,
}
impl Saver for MemoryStore {
    fn probe<'a>(
        &'a self,
        destination: &'a Location,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, DestinationObservation> {
        Box::pin(async move {
            match self.stat(destination, cancel).await {
                Ok(m) => Ok(DestinationObservation::Existing(Box::new(m))),
                Err(e) if e.kind == ErrorKind::NotFound => Ok(DestinationObservation::Missing),
                Err(e) => Err(e),
            }
        })
    }
    fn begin<'a>(
        &'a self,
        destination: &'a Location,
        _source: &'a ObjectMetadata,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Box<dyn WriteSession>> {
        Box::pin(async move {
            cancel.check("fake-begin")?;
            self.events
                .lock()
                .unwrap()
                .push(format!("begin:{}", key(destination)));
            Ok(Box::new(Writer {
                store: self.clone(),
                destination: destination.clone(),
                data: vec![],
            }) as Box<dyn WriteSession>)
        })
    }
}
impl WriteSession for Writer {
    fn destination(&self) -> &Location {
        &self.destination
    }
    fn write<'a>(&'a mut self, chunk: &'a [u8], cancel: &'a CancelContext) -> PortFuture<'a, ()> {
        Box::pin(async move {
            cancel.check("fake-write")?;
            self.data.extend_from_slice(chunk);
            Ok(())
        })
    }
    fn finish<'a>(
        &'a mut self,
        complete: bool,
        _warnings: &'a dyn WarningSink,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, SaveOutcome> {
        Box::pin(async move {
            cancel.check("fake-finish")?;
            assert!(complete);
            if self.store.fail_save.as_ref() == Some(&key(&self.destination)) {
                return Err(failure("fake-save"));
            }
            self.store.put(&key(&self.destination), &self.data);
            if let Some(cancel) = &self.store.cancel_after_commit {
                cancel.cancel();
            }
            Ok(SaveOutcome {
                destination: self.destination.clone(),
                bytes: self.data.len() as u64,
                verification: Verification::SizeOnly,
                observed: Some(Box::new(meta(self.destination.clone(), &self.data))),
            })
        })
    }
    fn abort<'a>(&'a mut self) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.store
                .events
                .lock()
                .unwrap()
                .push(format!("abort:{}", key(&self.destination)));
            Ok(())
        })
    }
}
impl RemoteCopy for MemoryStore {
    fn copy<'a>(
        &'a self,
        source: &'a ObjectMetadata,
        destination: &'a Location,
        _warnings: &'a dyn WarningSink,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, SaveOutcome> {
        Box::pin(async move {
            cancel.check("fake-copy")?;
            self.events
                .lock()
                .unwrap()
                .push(format!("copy:{}", key(&source.location)));
            if self.fail_save.as_ref() == Some(&key(destination)) {
                return Err(failure("fake-copy"));
            }
            let data = self.objects.lock().unwrap()[&key(&source.location)].clone();
            self.put(&key(destination), &data);
            Ok(SaveOutcome {
                destination: destination.clone(),
                bytes: data.len() as u64,
                verification: Verification::SizeOnly,
                observed: Some(Box::new(meta(destination.clone(), &data))),
            })
        })
    }
}
#[derive(Default)]
struct Observer {
    warnings: Mutex<Vec<Warning>>,
    results: Mutex<Vec<CopyOutcome>>,
    fail_output: bool,
}
impl WarningSink for Observer {
    fn warn<'a>(&'a self, w: Warning) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.warnings.lock().unwrap().push(w);
            Ok(())
        })
    }
}
impl OperationObserver for Observer {
    fn copied<'a>(&'a self, outcome: CopyOutcome) -> PortFuture<'a, ()> {
        Box::pin(async move {
            if self.fail_output {
                return Err(failure("fake-output"));
            }
            self.results.lock().unwrap().push(outcome);
            Ok(())
        })
    }
}
fn request(source: Location, destination: Location, recursive: bool, cwd: &Path) -> CopyRequest {
    CopyRequest {
        protected: None,
        source: Selection::new(source, recursive, None, cwd).unwrap(),
        destination,
        cwd: cwd.into(),
        ignore: vec![],
    }
}

#[tokio::test]
async fn three_routes_copy_real_local_bytes_and_index_remote_observations() {
    let temp = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(temp.path().join("index.db"))
        .await
        .unwrap();
    let store = MemoryStore::default();
    let cancel = CancelContext::new();
    let observer = Observer::default();
    let source = temp.path().join("source");
    std::fs::write(&source, b"actual body").unwrap();
    let upload = request(
        Location::Local(source),
        remote("uploaded"),
        false,
        temp.path(),
    );
    let mut plan = DiskPlan::new().await.unwrap();
    assert_eq!(
        store
            .engine(Ok(&db))
            .copy(&upload, &mut plan, &observer, &cancel)
            .await
            .unwrap()
            .files,
        1
    );
    let output = temp.path().join("output");
    let download = request(
        remote("uploaded"),
        Location::Local(output.clone()),
        false,
        temp.path(),
    );
    let mut plan = DiskPlan::new().await.unwrap();
    store
        .engine(Ok(&db))
        .copy(&download, &mut plan, &observer, &cancel)
        .await
        .unwrap();
    assert_eq!(std::fs::read(output).unwrap(), b"actual body");
    let copy = request(remote("uploaded"), remote("copied"), false, temp.path());
    let mut plan = DiskPlan::new().await.unwrap();
    store
        .engine(Ok(&db))
        .copy(&copy, &mut plan, &observer, &cancel)
        .await
        .unwrap();
    assert_eq!(store.objects.lock().unwrap()["copied"], b"actual body");
    assert_eq!(observer.results.lock().unwrap().len(), 3);
    for name in ["uploaded", "copied"] {
        let Location::Remote(id) = remote(name) else {
            panic!()
        };
        assert_eq!(db.get(&id).await.unwrap().unwrap().size, Some(11));
    }
    db.close().await.unwrap();
}
#[tokio::test]
async fn late_stat_failure_and_destination_collision_start_zero_bodies() {
    let temp = tempfile::tempdir().unwrap();
    let store = MemoryStore {
        fail_stat: Some("src/b".into()),
        ..Default::default()
    };
    for name in ["src/a", "src/b"] {
        store.put(name, b"body");
    }
    let output = temp.path().join("output");
    let req = request(
        remote("src"),
        Location::Local(output.clone()),
        true,
        temp.path(),
    );
    let mut plan = DiskPlan::new().await.unwrap();
    assert!(
        store
            .engine(Err(failure("index-init")))
            .copy(&req, &mut plan, &Observer::default(), &CancelContext::new())
            .await
            .is_err()
    );
    assert!(store.body_calls().is_empty());
    assert!(!output.exists());
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    // Exercise the actual destination volume's case behaviour.
    let probe = temp.path().join("Probe");
    std::fs::write(&probe, b"").unwrap();
    let insensitive = temp.path().join("probe").exists();
    std::fs::remove_file(probe).unwrap();
    let store = MemoryStore::default();
    for name in ["src/A", "src/a"] {
        store.put(name, b"body");
    }
    let mut plan = DiskPlan::new().await.unwrap();
    let result = store
        .engine(Err(failure("index-init")))
        .copy(&req, &mut plan, &Observer::default(), &CancelContext::new())
        .await;
    if insensitive {
        assert_eq!(result.err().unwrap().kind, ErrorKind::Input);
        assert!(store.body_calls().is_empty());
        assert!(!output.exists());
    } else {
        assert_eq!(result.unwrap().files, 2);
    }
}
#[derive(Default)]
struct BrokenIndex(AtomicUsize);
impl IndexWrite for BrokenIndex {
    fn apply<'a>(&'a self, _patch: MetadataPatch) -> IndexFuture<'a, ()> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(failure("index-write"))
        })
    }
    fn remove<'a>(&'a self, _id: &'a ObjectId) -> IndexFuture<'a, ()> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(failure("index-remove"))
        })
    }
    fn begin_reconcile<'a>(&'a self, _scope: IndexScope) -> IndexFuture<'a, Box<dyn Reconcile>> {
        Box::pin(async { Err(failure("index-reconcile")) })
    }
}
#[tokio::test]
async fn index_init_or_write_failure_warns_once_and_all_copies_still_complete() {
    let temp = tempfile::tempdir().unwrap();
    for unavailable in [false, true] {
        let store = MemoryStore::default();
        for name in ["src/a", "src/b"] {
            store.put(name, b"body");
        }
        let broken = BrokenIndex::default();
        let index = if unavailable {
            Err(failure("index-init"))
        } else {
            Ok(&broken as &dyn IndexWrite)
        };
        let observer = Observer::default();
        let mut plan = DiskPlan::new().await.unwrap();
        let result = store
            .engine(index)
            .copy(
                &request(remote("src"), remote("dest"), true, temp.path()),
                &mut plan,
                &observer,
                &CancelContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.files, 2);
        assert_eq!(result.warnings, 1);
        assert_eq!(
            observer.warnings.lock().unwrap()[0].category,
            WarningCategory::Index
        );
        assert!(broken.0.load(Ordering::Relaxed) <= 1);
        assert_eq!(store.objects.lock().unwrap()["dest/b"], b"body");
    }
}
#[tokio::test]
async fn first_transfer_failure_or_output_failure_stops_new_files_and_keeps_successes() {
    let temp = tempfile::tempdir().unwrap();
    for output_failure in [false, true] {
        let store = MemoryStore {
            fail_save: if output_failure {
                None
            } else {
                Some("dest/b".into())
            },
            ..Default::default()
        };
        for name in ["src/a", "src/b", "src/c"] {
            store.put(name, b"body");
        }
        let db = SqliteIndex::open(temp.path().join(format!("index-{output_failure}.db")))
            .await
            .unwrap();
        let observer = Observer {
            fail_output: output_failure,
            ..Default::default()
        };
        let mut plan = DiskPlan::new().await.unwrap();
        let result = store
            .engine(Ok(&db))
            .copy(
                &request(remote("src"), remote("dest"), true, temp.path()),
                &mut plan,
                &observer,
                &CancelContext::new(),
            )
            .await;
        assert!(result.is_err());
        assert!(store.objects.lock().unwrap().contains_key("dest/a"));
        assert!(!store.objects.lock().unwrap().contains_key("dest/c"));
        let Location::Remote(id) = remote("dest/a") else {
            panic!()
        };
        assert!(db.get(&id).await.unwrap().is_some());
        assert!(
            !store
                .body_calls()
                .iter()
                .any(|call| call.ends_with("src/c"))
        );
        if output_failure {
            assert!(
                !store
                    .body_calls()
                    .iter()
                    .any(|call| call.ends_with("src/b"))
            );
        }
    }
}
#[tokio::test]
async fn destination_ignore_and_invalid_remote_local_layout_are_resolved_before_body_io() {
    let temp = tempfile::tempdir().unwrap();
    let store = MemoryStore::default();
    store.put("source", b"body");
    let mut req = request(
        remote("source"),
        remote("dest/skip.tmp"),
        false,
        temp.path(),
    );
    req.ignore = vec!["*.tmp".into()];
    let observer = Observer::default();
    let mut plan = DiskPlan::new().await.unwrap();
    let result = store
        .engine(Err(failure("index-init")))
        .copy(&req, &mut plan, &observer, &CancelContext::new())
        .await
        .err()
        .unwrap();
    assert_eq!(result.kind, ErrorKind::NotFound);
    assert!(
        observer
            .warnings
            .lock()
            .unwrap()
            .iter()
            .any(|w| w.category == WarningCategory::Ignored)
    );
    assert!(store.body_calls().is_empty());
    assert!(
        !store
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.starts_with("stat:"))
    );
    store.put("src/a//b", b"body");
    let req = request(
        remote("src"),
        Location::Local(temp.path().join("out")),
        true,
        temp.path(),
    );
    let mut plan = DiskPlan::new().await.unwrap();
    assert_eq!(
        store
            .engine(Err(failure("index-init")))
            .copy(&req, &mut plan, &Observer::default(), &CancelContext::new())
            .await
            .err()
            .unwrap()
            .kind,
        ErrorKind::Input
    );
    assert!(store.body_calls().is_empty());
}
struct FailingPlan {
    inner: DiskPlan,
    appends: usize,
    discarded: bool,
}
impl PlanStore for FailingPlan {
    fn append<'a>(
        &'a mut self,
        entry: ResolvedCopy,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.appends += 1;
            if self.appends == 2 {
                return Err(failure("manifest-disk"));
            }
            self.inner.append(entry, cancel).await
        })
    }
    fn seal<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, ()> {
        self.inner.seal(cancel)
    }
    fn next<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, Option<ResolvedCopy>> {
        self.inner.next(cancel)
    }
    fn discard<'a>(&'a mut self) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.discarded = true;
            self.inner.discard().await
        })
    }
}
#[tokio::test]
async fn mandatory_manifest_failure_is_an_error_and_starts_zero_bodies() {
    let temp = tempfile::tempdir().unwrap();
    let store = MemoryStore::default();
    for name in ["src/a", "src/b"] {
        store.put(name, b"body");
    }
    let mut plan = FailingPlan {
        inner: DiskPlan::new().await.unwrap(),
        appends: 0,
        discarded: false,
    };
    let result = store
        .engine(Err(failure("index-init")))
        .copy(
            &request(remote("src"), remote("dest"), true, temp.path()),
            &mut plan,
            &Observer::default(),
            &CancelContext::new(),
        )
        .await;
    assert_eq!(result.err().unwrap().stage, "manifest-disk");
    assert!(store.body_calls().is_empty());
    assert!(plan.discarded);
}
#[tokio::test]
async fn confirmed_success_is_output_and_indexed_before_cancellation_stops_next_file() {
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancelContext::new();
    let store = MemoryStore {
        cancel_after_commit: Some(cancel.clone()),
        ..Default::default()
    };
    let db = SqliteIndex::open(temp.path().join("index.db"))
        .await
        .unwrap();
    let source = temp.path().join("sources");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("a"), b"body").unwrap();
    std::fs::write(source.join("b"), b"body").unwrap();
    let observer = Observer::default();
    let mut plan = DiskPlan::new().await.unwrap();
    let result = store
        .engine(Ok(&db))
        .copy(
            &request(Location::Local(source), remote("dest"), true, temp.path()),
            &mut plan,
            &observer,
            &cancel,
        )
        .await;
    assert_eq!(result.err().unwrap().kind, ErrorKind::Cancelled);
    let id = {
        let output = observer.results.lock().unwrap();
        assert_eq!(output.len(), 1);
        let Location::Remote(id) = &output[0].destination else {
            panic!()
        };
        id.clone()
    };
    assert!(db.get(&id).await.unwrap().is_some());
    assert_eq!(store.body_calls().len(), 1);
}

struct LateConfirmedCopy {
    store: MemoryStore,
}
impl RemoteCopy for LateConfirmedCopy {
    fn copy<'a>(
        &'a self,
        source: &'a ObjectMetadata,
        destination: &'a Location,
        warnings: &'a dyn WarningSink,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, SaveOutcome> {
        Box::pin(async move {
            let saved = self
                .store
                .copy(source, destination, warnings, cancel)
                .await?;
            cancel.cancel();
            tokio::time::sleep(std::time::Duration::from_secs(44)).await;
            Ok(saved)
        })
    }
}
struct StalledObserver;
impl WarningSink for StalledObserver {
    fn warn<'a>(&'a self, _warning: Warning) -> PortFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}
impl OperationObserver for StalledObserver {
    fn copied<'a>(&'a self, _outcome: CopyOutcome) -> PortFuture<'a, ()> {
        Box::pin(std::future::pending())
    }
}
struct StalledDiscard {
    inner: DiskPlan,
}
impl PlanStore for StalledDiscard {
    fn set_cleanup_deadline(&mut self, deadline: tokio::time::Instant) {
        self.inner.set_cleanup_deadline(deadline);
    }
    fn append<'a>(
        &'a mut self,
        entry: ResolvedCopy,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ()> {
        self.inner.append(entry, cancel)
    }
    fn seal<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, ()> {
        self.inner.seal(cancel)
    }
    fn next<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, Option<ResolvedCopy>> {
        self.inner.next(cancel)
    }
    fn discard<'a>(&'a mut self) -> PortFuture<'a, ()> {
        Box::pin(std::future::pending())
    }
}
#[tokio::test(start_paused = true)]
async fn cancellation_budget_is_shared_by_confirmed_copy_index_output_and_manifest() {
    use std::time::Duration;
    let temp = tempfile::tempdir().unwrap();
    let store = MemoryStore::default();
    store.put("source", b"body");
    let late = LateConfirmedCopy {
        store: store.clone(),
    };
    let mut engine = store.engine(Err(failure("index-init")));
    engine.remote_copy = &late;
    let cancel = CancelContext::new();
    let mut plan = StalledDiscard {
        inner: DiskPlan::new().await.unwrap(),
    };
    let result = engine
        .copy(
            &request(remote("source"), remote("destination"), false, temp.path()),
            &mut plan,
            &StalledObserver,
            &cancel,
        )
        .await;
    assert_eq!(result.err().unwrap().kind, ErrorKind::Cancelled);
    let elapsed = tokio::time::Instant::now() - cancel.cancelled_at().unwrap();
    assert!(
        elapsed <= Duration::from_secs(50),
        "cleanup extended the stop budget: {elapsed:?}"
    );
    assert_eq!(store.objects.lock().unwrap()["destination"], b"body");
    assert_eq!(store.body_calls().len(), 1);
}
