use archive_domain::*;
use archive_sync::*;
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::{mpsc, oneshot};

type Job = Box<dyn FnOnce(&mut Worker) + Send>;
pub struct DiskObjects {
    sender: Option<mpsc::Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
struct Worker {
    db: Option<Connection>,
    directory: Option<tempfile::TempDir>,
    sealed: bool,
    cursor: i64,
    summary: ObjectPlanSummary,
    known_bytes: u64,
    unknown_sizes: u64,
}
fn error(e: impl std::fmt::Display) -> OperationError {
    OperationError::new(ErrorKind::Io, "object-plan", e.to_string())
}
fn encode(object: &PlannedObject) -> Result<(ObjectId, String), OperationError> {
    let Location::Remote(id) = &object.metadata.location else {
        return Err(error("remote object required"));
    };
    if id.key.is_empty() || object.metadata.kind != EntryKind::File {
        return Err(error("remote file required"));
    }
    let record = serde_json::to_string(object).map_err(error)?;
    if record.len() > 128 * 1024 {
        return Err(error("one manifest record exceeds 128 KiB"));
    }
    Ok((id.clone(), record))
}
impl DiskObjects {
    pub async fn new() -> Result<Self, OperationError> {
        let (sender, mut receiver) = mpsc::channel::<Job>(16);
        let (ready, result) = oneshot::channel();
        let thread = std::thread::Builder::new().name("archive-objects".into()).spawn(move || {
            let opened = (|| {
                let directory = tempfile::Builder::new().prefix("private-archive-objects-").tempdir().map_err(error)?;
                let file = tempfile::NamedTempFile::new_in(directory.path()).map_err(error)?;
                let db = Connection::open(file.path()).map_err(error)?;
                // This private manifest is discarded on every exit, never
                // recovered. Durability belongs to data and the reference DB.
                db.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;
                    CREATE TABLE objects(namespace TEXT,scheme TEXT,authority TEXT,object_key BLOB,uri_order INTEGER NOT NULL,record TEXT NOT NULL,PRIMARY KEY(namespace,scheme,authority,object_key)) WITHOUT ROWID;").map_err(error)?;
                // Keep the database name alive until the connection is closed.
                let _ = file.keep().map_err(error)?;
                Ok::<_,OperationError>(Worker { db:Some(db), directory:Some(directory), sealed:false,cursor:0,summary:ObjectPlanSummary { objects:0,total_bytes:Some(0) },known_bytes:0,unknown_sizes:0 })
            })();
            let mut worker = match opened { Ok(w) => w,Err(e) => { let _ = ready.send(Err(e)); return; } };
            if ready.send(Ok(())).is_err() { return; }
            while let Some(job) = receiver.blocking_recv() { job(&mut worker); }
            worker.db.take();
            if let Some(dir) = worker.directory.take() { let _ = dir.close(); }
        }).map_err(error)?;
        match result.await.map_err(error)? {
            Ok(()) => Ok(Self {
                sender: Some(sender),
                thread: Some(thread),
            }),
            Err(e) => {
                let _ = thread.join();
                Err(e)
            }
        }
    }
    async fn call<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Worker) -> Result<T, OperationError> + Send + 'static,
    ) -> Result<T, OperationError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .as_ref()
            .ok_or_else(|| error("manifest discarded"))?
            .send(Box::new(move |w| {
                let _ = tx.send(job(w));
            }))
            .await
            .map_err(error)?;
        rx.await.map_err(error)?
    }
}
impl Drop for DiskObjects {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(t) = self.thread.take() {
            // discard() observes normal teardown. If that wait was abandoned
            // or its budget expired, Drop must not restart an unbounded join.
            // The worker owns all files/DB handles and stops on channel close.
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

impl ObjectPlan for DiskObjects {
    fn append<'a>(
        &'a mut self,
        uri_order: u64,
        object: PlannedObject,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, bool> {
        Box::pin(async move {
            cancel.check("object-plan")?;
            let (id, record) = encode(&object)?;
            let order = i64::try_from(uri_order).map_err(error)?;
            self.call(move |w| {
                if w.sealed {
                    return Err(error("manifest sealed"));
                }
                let db=w.db.as_ref().unwrap();
                let old:Option<String>=db.query_row("SELECT record FROM objects WHERE namespace=?1 AND scheme=?2 AND authority=?3 AND object_key=?4",params![id.namespace,id.scheme,id.authority,id.key.as_bytes()],|r| r.get(0)).optional().map_err(error)?;
                let inserted=old.is_none();
                let mut bytes=w.known_bytes;
                let mut unknown=w.unknown_sizes;
                if let Some(old)=old {
                    let old:PlannedObject=serde_json::from_str(&old).map_err(error)?;
                    if let Some(n)=old.metadata.size {bytes=bytes.checked_sub(n).ok_or_else(|| error("inconsistent size total"))?;} else {unknown=unknown.checked_sub(1).ok_or_else(|| error("inconsistent unknown count"))?;}
                }
                if let Some(n)=object.metadata.size {bytes=bytes.checked_add(n).ok_or_else(|| error("target size overflow"))?;} else {unknown+=1;}
                db.execute("INSERT INTO objects VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(namespace,scheme,authority,object_key) DO UPDATE SET record=excluded.record",params![id.namespace,id.scheme,id.authority,id.key.as_bytes(),order,record]).map_err(error)?;
                if inserted {w.summary.objects=w.summary.objects.checked_add(1).ok_or_else(|| error("target count overflow"))?;}
                w.known_bytes=bytes;w.unknown_sizes=unknown;
                w.summary.total_bytes=(unknown==0).then_some(bytes);
                Ok(inserted)
            })
            .await
        })
    }
    fn seal<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, ObjectPlanSummary> {
        Box::pin(async move {
            cancel.check("object-plan")?;
            let control = cancel.clone();
            self.call(move |w| {
                if w.sealed { return Err(error("manifest already sealed")); }
                let db = w.db.as_mut().unwrap();
                db.progress_handler(1000,Some(move || control.is_cancelled())).map_err(error)?;
                let result = db.execute_batch("CREATE TABLE ordered AS SELECT row_number() OVER(ORDER BY uri_order,object_key,namespace,scheme,authority) AS sequence,namespace,scheme,authority,object_key FROM objects;
                    CREATE UNIQUE INDEX ordered_sequence ON ordered(sequence);");
                db.progress_handler(0,None::<fn()->bool>).map_err(error)?;
                result.map_err(error)?;
                w.sealed = true; Ok(w.summary)
            }).await
        })
    }
    fn next<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, Option<PlannedObject>> {
        Box::pin(async move {
            cancel.check("object-plan")?;
            self.call(|w| {
                if !w.sealed { return Err(error("manifest must be sealed")); }
                let row:Option<(i64,String)> = w.db.as_ref().unwrap().query_row("SELECT o.sequence,t.record FROM ordered o JOIN objects t USING(namespace,scheme,authority,object_key) WHERE o.sequence>?1 ORDER BY o.sequence LIMIT 1",[w.cursor],|r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(error)?;
                row.map(|(seq,text)| { let object = serde_json::from_str(&text).map_err(error)?; w.cursor=seq; Ok(object) }).transpose()
            }).await
        })
    }
    fn replace<'a>(
        &'a mut self,
        id: &'a ObjectId,
        object: PlannedObject,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ()> {
        Box::pin(async move {
            cancel.check("object-plan")?;
            let (updated, record) = encode(&object)?;
            if &updated != id {
                return Err(error("replacement changed object identity"));
            }
            let id = id.clone();
            self.call(move |w| {
                if !w.sealed { return Err(error("manifest must be sealed")); }
                let changed = w.db.as_ref().unwrap().execute("UPDATE objects SET record=?5 WHERE namespace=?1 AND scheme=?2 AND authority=?3 AND object_key=?4",params![id.namespace,id.scheme,id.authority,id.key.as_bytes(),record]).map_err(error)?;
                if changed != 1 { return Err(error("replacement target not found")); } Ok(())
            }).await
        })
    }
    fn rewind<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, ()> {
        Box::pin(async move {
            cancel.check("object-plan")?;
            self.call(|w| {
                if !w.sealed {
                    return Err(error("manifest must be sealed"));
                }
                w.cursor = 0;
                Ok(())
            })
            .await
        })
    }
    fn discard<'a>(&'a mut self) -> PortFuture<'a, ()> {
        Box::pin(async move {
            let result = if self.sender.is_some() {
                self.call(|w| {
                    w.db.take();
                    if let Some(dir) = w.directory.take() {
                        dir.close().map_err(error)
                    } else {
                        Ok(())
                    }
                })
                .await
            } else {
                Ok(())
            };
            self.sender.take();
            if let Some(t) = self.thread.take() {
                tokio::task::spawn_blocking(move || t.join())
                    .await
                    .map_err(error)?
                    .map_err(|_| error("manifest worker panicked"))?;
            }
            result
        })
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn abandoned_plan_does_not_join_a_stalled_worker() {
        let plan = DiskObjects::new().await.unwrap();
        let sender = plan.sender.as_ref().unwrap().clone();
        let (entered, ready) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let (finished, done) = oneshot::channel();
        sender
            .send(Box::new(move |_| {
                let _ = entered.send(());
                let _ = held.recv();
                let _ = finished.send(());
            }))
            .await
            .unwrap();
        drop(sender);
        ready.await.unwrap();
        let (dropped, observed) = oneshot::channel();
        let dropper = std::thread::spawn(move || {
            drop(plan);
            let _ = dropped.send(());
        });
        let returned = tokio::time::timeout(Duration::from_secs(3), observed).await;
        release.send(()).unwrap();
        done.await.unwrap();
        dropper.join().unwrap();
        assert!(
            returned.is_ok(),
            "Drop waited for a deliberately stalled worker"
        );
    }
}
