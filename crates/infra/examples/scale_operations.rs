//! Explicit acceptance driver: generated metadata, real engines/adapters, no HTTP/bodies.
use archive_domain::*;
use archive_index::*;
use archive_infra::{DiskObjects, SqliteIndex};
use archive_sync::*;
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, UNIX_EPOCH},
};

fn id(key: &str) -> ObjectId {
    ObjectId::parse_s3(&format!("s3://bucket/{key}")).unwrap()
}
fn selection() -> Selection {
    Selection::new(Location::Remote(id("p")), true, None, Path::new("/")).unwrap()
}
struct Generated {
    count: u64,
    emitted: Arc<AtomicU64>,
}
struct Cursor {
    remaining: u64,
    emitted: Arc<AtomicU64>,
}
impl EntryCursor for Cursor {
    fn next<'a>(
        &'a mut self,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Option<EnumerationItem>> {
        Box::pin(async move {
            cancel.check("generated-next")?;
            if self.remaining == 0 {
                return Ok(None);
            }
            self.remaining -= 1;
            let mut m =
                ObjectMetadata::file(Location::Remote(id(&format!("p/{:09}", self.remaining))), 1);
            m.data_class = Some(StoredClass::Archived);
            m.restore_state = RestoreState::Archived;
            m.queried_at = Some(UNIX_EPOCH);
            m.version_token = Some("v1".into());
            self.emitted.fetch_add(1, Ordering::Relaxed);
            Ok(Some(EnumerationItem::Entry(Box::new(m))))
        })
    }
}
impl Loader for Generated {
    fn enumerate<'a>(
        &'a self,
        _: &'a Selection,
        _: &'a IgnoreFilter,
        _: &'a CancelContext,
    ) -> PortFuture<'a, Box<dyn EntryCursor>> {
        Box::pin(async move {
            Ok(Box::new(Cursor {
                remaining: self.count,
                emitted: self.emitted.clone(),
            }) as Box<dyn EntryCursor>)
        })
    }
    fn stat<'a>(&'a self, _: &'a Location, _: &'a CancelContext) -> PortFuture<'a, ObjectMetadata> {
        Box::pin(async { panic!("known generated observations must not require HEAD") })
    }
    fn open<'a>(&'a self, _: &'a Location, _: &'a CancelContext) -> PortFuture<'a, OpenedSource> {
        Box::pin(async { panic!("metadata scale test must not read bodies") })
    }
}

// For rm/restore this test measures full target resolution/preflight/manifest
// iteration. Per-record durable index mutation is covered by reconcile below.
#[derive(Default)]
struct CountIndex(AtomicU64);
impl IndexWrite for CountIndex {
    fn apply<'a>(&'a self, _: MetadataPatch) -> IndexFuture<'a, ()> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    }
    fn remove<'a>(&'a self, _: &'a ObjectId) -> IndexFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn begin_reconcile<'a>(&'a self, _: IndexScope) -> IndexFuture<'a, Box<dyn Reconcile>> {
        Box::pin(async { panic!("data operations must not prune an index scope") })
    }
}
struct Remote<'a> {
    source: &'a Generated,
    index: &'a CountIndex,
    calls: AtomicU64,
    last: Mutex<Option<String>>,
}
impl Remote<'_> {
    fn record(&self, object: &ObjectId) {
        assert_eq!(
            self.source.emitted.load(Ordering::Relaxed),
            self.source.count
        );
        assert!(self.index.0.load(Ordering::Relaxed) >= self.source.count);
        let mut last = self.last.lock().unwrap();
        assert!(last.as_ref().is_none_or(|last| last < &object.key));
        *last = Some(object.key.clone());
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}
impl RemoteRemove for Remote<'_> {
    fn remove<'a>(&'a self, object: &'a ObjectId, cancel: &'a CancelContext) -> PortFuture<'a, ()> {
        Box::pin(async move {
            cancel.check("fake-remove")?;
            self.record(object);
            Ok(())
        })
    }
}
impl RemoteRestore for Remote<'_> {
    fn restore<'a>(
        &'a self,
        m: &'a ObjectMetadata,
        _: RestoreDays,
        _: RestoreTier,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, RestoreAccepted> {
        Box::pin(async move {
            cancel.check("fake-restore")?;
            let Location::Remote(object) = &m.location else {
                panic!()
            };
            self.record(object);
            Ok(RestoreAccepted {
                queried_at: UNIX_EPOCH,
            })
        })
    }
}
struct Observer;
impl WarningSink for Observer {
    fn warn<'a>(&'a self, w: Warning) -> PortFuture<'a, ()> {
        Box::pin(async move { panic!("unexpected warning: {w:?}") })
    }
}
impl RemoveObserver for Observer {
    fn confirm<'a>(
        &'a self,
        _: Location,
        _: ObjectPlanSummary,
        _: &'a CancelContext,
    ) -> PortFuture<'a, bool> {
        Box::pin(async { panic!("auto-approve must not read input") })
    }
    fn removed<'a>(&'a self, _: Location) -> PortFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}
impl RestoreObserver for Observer {
    fn accepted<'a>(&'a self, _: Location, _: RestoreDays, _: RestoreTier) -> PortFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 3, "scale_operations reconcile|rm|restore COUNT");
    let count: u64 = args[2].parse().unwrap();
    assert!((1..=1_000_000).contains(&count));
    let start = Instant::now();
    let source = Generated {
        count,
        emitted: Arc::new(AtomicU64::new(0)),
    };
    let cancel = CancelContext::new();
    if args[1] == "reconcile" {
        let directory = tempfile::tempdir().unwrap();
        let db = SqliteIndex::open(directory.path().join("index.db"))
            .await
            .unwrap();
        let result = ReconcileEngine {
            loader: &source,
            index: &db,
        }
        .reconcile(
            &ReconcileRequest {
                selection: selection(),
                ignore: vec![],
            },
            &(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!((result.observed, result.removed), (count, 0));
        let mut cursor = None;
        let mut observed = 0;
        let scope = IndexScope {
            root: id("p"),
            recursive: true,
            max_depth: None,
            ignore: vec![],
        };
        loop {
            let page = db.query(&scope, cursor, MAX_PAGE_ENTRIES).await.unwrap();
            observed += page.entries.len() as u64;
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(observed, count);
        let snapshot = db.snapshot().await.unwrap();
        db.inspect_snapshot(snapshot.path().into()).await.unwrap();
        assert!(std::fs::metadata(snapshot.path()).unwrap().len() > 0);
        db.close().await.unwrap();
    } else {
        let index = CountIndex::default();
        let remote = Remote {
            source: &source,
            index: &index,
            calls: AtomicU64::new(0),
            last: Mutex::new(None),
        };
        let mut plan = DiskObjects::new().await.unwrap();
        match args[1].as_str() {
            "rm" => {
                let summary = RemoveEngine {
                    loader: &source,
                    remote: &remote,
                    index: Ok(&index),
                }
                .remove(
                    &RemoveRequest {
                        protected: None,
                        selection: selection(),
                        ignore: vec![],
                        auto_approve: true,
                    },
                    &mut plan,
                    &Observer,
                    &cancel,
                )
                .await
                .unwrap();
                assert_eq!((summary.resolved, summary.removed), (count, count));
            }
            "restore" => {
                let summary = RestoreEngine {
                    loader: &source,
                    remote: &remote,
                    index: Ok(&index),
                }
                .restore(
                    &RestoreRequest {
                        protected: None,
                        selections: vec![selection()],
                        ignore: vec![],
                        days: "1d".parse().unwrap(),
                        tier: RestoreTier::Bulk,
                    },
                    &mut plan,
                    &Observer,
                    &cancel,
                )
                .await
                .unwrap();
                assert_eq!((summary.resolved, summary.accepted), (count, count));
            }
            _ => panic!("unknown scale mode"),
        }
        assert_eq!(remote.calls.load(Ordering::Relaxed), count);
        assert!(
            plan.next(&cancel).await.is_err(),
            "manifest must be discarded"
        );
    }
    assert_eq!(source.emitted.load(Ordering::Relaxed), count);
    println!(
        "{{\"mode\":\"{}\",\"count\":{},\"elapsed_seconds\":{:.3},\"body_reads\":0,\"real_aws_requests\":0}}",
        args[1],
        count,
        start.elapsed().as_secs_f64()
    );
}
