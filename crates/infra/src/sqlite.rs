//! SQLite is used only as a local copy of remote observations.
mod browse;
mod codec;
mod import;
use archive_domain::*;
use archive_index::*;
use archive_sync::{IgnoreFilter, Selection};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

const APP_ID: i64 = 0x50415243;
const RECORD_LIMIT: usize = 128 * 1024;
type Job = Box<dyn FnOnce(&mut Worker) + Send>;
#[derive(Clone)]
pub struct SqliteIndex {
    sender: mpsc::Sender<Job>,
    thread: std::sync::Arc<WorkerThread>,
}
struct WorkerThread(std::sync::Mutex<Option<std::thread::JoinHandle<()>>>);
impl Drop for WorkerThread {
    fn drop(&mut self) {
        if let Some(thread) = self.0.get_mut().unwrap().take() {
            let _ = thread.join();
        }
    }
}
struct Worker {
    db: Connection,
    reconcile: Option<ActiveReconcile>,
    import: Option<import::ActiveImport>,
    sequence: u64,
    stop: bool,
    #[cfg(test)]
    snapshot_runs: usize,
}
struct ActiveReconcile {
    id: u64,
    revision: i64,
    scope: IndexScope,
    lease: std::sync::Arc<()>,
}

pub(super) fn error(stage: &'static str, cause: impl std::fmt::Display) -> OperationError {
    OperationError::new(ErrorKind::Io, stage, cause.to_string())
}
fn sql_error(cause: rusqlite::Error) -> OperationError {
    let mut result = error("index-sqlite", &cause);
    if let rusqlite::Error::SqliteFailure(code, _) = cause {
        result.code = Some(code.extended_code.to_string());
    }
    result
}
fn validate(db: &Connection, thorough: bool) -> Result<(), OperationError> {
    let app: i64 = db
        .query_row("PRAGMA application_id", [], |r| r.get(0))
        .map_err(sql_error)?;
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(sql_error)?;
    if app != APP_ID || version != 1 {
        return Err(error(
            "index-format",
            format!(
                "unsupported database: application_id={app:#x}, schema={version}; no changes made"
            ),
        ));
    }
    db.query_row("SELECT revision FROM metadata WHERE singleton=1", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(sql_error)?;
    // Validate required columns even for an empty database.
    db.prepare("SELECT namespace,scheme,authority,object_key,size_bytes,data_class,restore_state,restore_expiry_seconds,original_mtime_seconds,original_mtime_nanos,original_creation_seconds,original_creation_nanos,unix_mode,queried_seconds,queried_nanos,store_modified_seconds,version_token,checksum_algorithm,checksum_type,checksum_value,revision FROM objects LIMIT 0").map_err(sql_error)?;
    if thorough {
        let check: String = db
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(sql_error)?;
        if check != "ok" {
            return Err(error("index-integrity", check));
        }
    }
    Ok(())
}
fn open_existing(path: &Path, readonly: bool) -> Result<Connection, OperationError> {
    let mut header = [0; 16];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .map_err(|e| error("index-header", e))?;
    if &header != b"SQLite format 3\0" {
        return Err(error(
            "index-header",
            "invalid SQLite header; existing file preserved",
        ));
    }
    let flags = if readonly {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let db = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(sql_error)?;
    db.busy_timeout(Duration::from_secs(2)).map_err(sql_error)?;
    validate(&db, false)?;
    Ok(db)
}
fn open(path: &Path) -> Result<Connection, OperationError> {
    if !path.try_exists().map_err(|e| error("index-open", e))? {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| error("index-init", e))?;
        let temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| error("index-init", e))?;
        {
            let db = Connection::open(temp.path()).map_err(sql_error)?;
            db.execute_batch("PRAGMA journal_mode=WAL")
                .map_err(sql_error)?;
            db.execute_batch(&format!("BEGIN IMMEDIATE; {} PRAGMA application_id={APP_ID}; PRAGMA user_version=1; COMMIT;",include_str!("sqlite/schema.sql"))).map_err(sql_error)?;
        }
        // Publish a fully initialized database without replacing any existing
        // path. Concurrent creators either publish or open the winner's file.
        match std::fs::hard_link(temp.path(), path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(error("index-init", e)),
        }
    }
    let db = open_existing(path, false)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        db.busy_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(sql_error)?;
        match db.execute_batch("PRAGMA journal_mode=WAL") {
            Ok(()) => break,
            Err(rusqlite::Error::SqliteFailure(code, _))
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => return Err(sql_error(e)),
        }
    }
    db.busy_timeout(Duration::from_secs(2)).map_err(sql_error)?;
    db.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;").map_err(sql_error)?;
    Ok(db)
}

impl SqliteIndex {
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, OperationError> {
        let path = path.into();
        let (sender, mut receiver) = mpsc::channel::<Job>(16);
        let (ready, result) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("archive-index".into())
            .spawn(move || {
                let db = match open(&path) {
                    Ok(db) => db,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let mut worker = Worker {
                    db,
                    reconcile: None,
                    import: None,
                    sequence: 0,
                    stop: false,
                    #[cfg(test)]
                    snapshot_runs: 0,
                };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                while let Some(job) = receiver.blocking_recv() {
                    import::discard_abandoned(&mut worker);
                    if let Some(active) = &worker.reconcile
                        && std::sync::Arc::strong_count(&active.lease) == 1
                    {
                        let id = active.id;
                        let _ = cancel_reconcile(&mut worker, id);
                    }
                    job(&mut worker);
                    if worker.stop {
                        break;
                    }
                }
            })
            .map_err(|e| error("index-worker", e))?;
        result.await.map_err(|e| error("index-worker", e))??;
        Ok(Self {
            sender,
            thread: std::sync::Arc::new(WorkerThread(std::sync::Mutex::new(Some(thread)))),
        })
    }
    async fn call<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Worker) -> Result<T, OperationError> + Send + 'static,
    ) -> Result<T, OperationError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(Box::new(move |worker| {
                let _ = sender.send(job(worker));
            }))
            .await
            .map_err(|e| error("index-worker", e))?;
        receiver.await.map_err(|e| error("index-worker", e))?
    }
    /// Join the worker before an async runtime shuts down. Clones become closed.
    pub async fn close(self) -> Result<(), OperationError> {
        self.call(|w| {
            w.stop = true;
            Ok(())
        })
        .await?;
        let thread = self.thread.0.lock().unwrap().take();
        if let Some(thread) = thread {
            tokio::task::spawn_blocking(move || thread.join())
                .await
                .map_err(|e| error("index-close", e))?
                .map_err(|_| error("index-close", "worker panicked"))?;
        }
        Ok(())
    }
    pub fn sqlite_version() -> &'static str {
        rusqlite::version()
    }
}
fn get(db: &Connection, id: &ObjectId) -> Result<Option<ObjectMetadata>, OperationError> {
    db.query_row(
        "SELECT * FROM objects WHERE namespace=?1 AND scheme=?2 AND authority=?3 AND object_key=?4",
        params![id.namespace, id.scheme, id.authority, id.key.as_bytes()],
        codec::row,
    )
    .optional()
    .map_err(sql_error)
}
fn validate_record(meta: &ObjectMetadata) -> Result<(), OperationError> {
    if codec::json(meta)?.len() > RECORD_LIMIT {
        return Err(error("index-record", "metadata exceeds 128KiB limit"));
    }
    if let Some(n) = meta.size {
        i64::try_from(n).map_err(|e| error("index-size", e))?;
    }
    let Location::Remote(id) = &meta.location else {
        return Err(error("index-record", "remote identity required"));
    };
    if id.key.is_empty()
        || id.namespace.len() > 4096
        || id.scheme.len() > 4096
        || id.authority.len() > 4096
    {
        return Err(error("index-record", "invalid object identity"));
    }
    Ok(())
}
fn apply(db: &Connection, patch: MetadataPatch) -> Result<(), OperationError> {
    // Read and merge inside the caller's short write transaction.
    let meta = patch.clone().merge(get(db, &patch.id)?);
    validate_record(&meta)?;
    let id = &patch.id;
    let size = meta
        .size
        .map(|v| i64::try_from(v).map_err(|e| error("index-size", e)))
        .transpose()?;
    let class = meta.data_class.as_ref().map(codec::json).transpose()?;
    let (restore, expiry) = match meta.restore_state {
        RestoreState::NotRequired => (Some("not-required"), None),
        RestoreState::Archived => (Some("archived"), None),
        RestoreState::Restoring => (Some("restoring"), None),
        RestoreState::Restored { until } => (Some("restored"), codec::split(until)?.0),
        RestoreState::Unknown => (None, None),
    };
    let (mtime, mtime_ns) = codec::split(meta.original_mtime)?;
    let (creation, creation_ns) = codec::split(meta.original_creation_time)?;
    let (queried, queried_ns) = codec::split(meta.queried_at)?;
    let modified = codec::split(meta.store_modified)?.0;
    let algorithm = meta
        .checksum
        .as_ref()
        .map(|c| codec::json(&c.algorithm))
        .transpose()?;
    let scope = meta
        .checksum
        .as_ref()
        .map(|c| codec::json(&c.scope))
        .transpose()?;
    let value = meta
        .checksum
        .as_ref()
        .map(|c| codec::json(&c.value))
        .transpose()?;
    db.execute(
        "UPDATE metadata SET revision=revision+1 WHERE singleton=1",
        [],
    )
    .map_err(sql_error)?;
    db.execute("INSERT INTO objects VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,(SELECT revision FROM metadata WHERE singleton=1)) ON CONFLICT(namespace,scheme,authority,object_key) DO UPDATE SET size_bytes=excluded.size_bytes,data_class=excluded.data_class,restore_state=excluded.restore_state,restore_expiry_seconds=excluded.restore_expiry_seconds,original_mtime_seconds=excluded.original_mtime_seconds,original_mtime_nanos=excluded.original_mtime_nanos,original_creation_seconds=excluded.original_creation_seconds,original_creation_nanos=excluded.original_creation_nanos,unix_mode=excluded.unix_mode,queried_seconds=excluded.queried_seconds,queried_nanos=excluded.queried_nanos,store_modified_seconds=excluded.store_modified_seconds,version_token=excluded.version_token,checksum_algorithm=excluded.checksum_algorithm,checksum_type=excluded.checksum_type,checksum_value=excluded.checksum_value,revision=excluded.revision",params![id.namespace,id.scheme,id.authority,id.key.as_bytes(),size,class,restore,expiry,mtime,mtime_ns,creation,creation_ns,meta.unix_mode,queried,queried_ns,modified,meta.version_token,algorithm,scope,value]).map_err(sql_error)?;
    Ok(())
}
fn selection(scope: &IndexScope) -> Result<(Selection, IgnoreFilter), OperationError> {
    let selected = Selection::new(
        Location::Remote(scope.root.clone()),
        scope.recursive,
        scope.max_depth,
        Path::new("/"),
    )?;
    let ignore = IgnoreFilter::new(selected.ignore_base.clone(), &scope.ignore)?;
    Ok((selected, ignore))
}
fn matches(scope: &IndexScope, meta: &ObjectMetadata) -> Result<bool, OperationError> {
    let (s, i) = selection(scope)?;
    Ok(s.contains(&meta.location) && !i.is_ignored(&meta.location, false)?)
}
fn successor(mut bytes: Vec<u8>) -> Option<Vec<u8>> {
    while let Some(last) = bytes.pop() {
        if last < 255 {
            bytes.push(last + 1);
            return Some(bytes);
        }
    }
    None
}

#[cfg(test)]
mod keyset_regression {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    #[test]
    fn late_page_seeks_to_cursor_without_rescanning_the_prefix() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("sqlite/schema.sql")).unwrap();
        {
            let tx = db.transaction().unwrap();
            {
                let mut insert = tx.prepare("INSERT INTO objects(namespace,scheme,authority,object_key,size_bytes,queried_seconds,queried_nanos,revision) VALUES('aws','s3','bucket',?1,1,1,0,1)").unwrap();
                for n in 0..20_000 {
                    insert.execute([format!("foo/{n:06}").as_bytes()]).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let root = ObjectId::parse_s3("s3://bucket/foo").unwrap();
        let mut scope = IndexScope {
            root,
            recursive: true,
            max_depth: None,
            ignore: vec![],
        };
        let steps = Arc::new(AtomicU64::new(0));
        let observed = steps.clone();
        db.progress_handler(
            100,
            Some(move || {
                observed.fetch_add(100, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
        let first = page(&db, &scope, None, 256).unwrap();
        let first_steps = steps.swap(0, Ordering::Relaxed);
        let late = page(&db, &scope, Some(b"foo/019000".to_vec()), 256).unwrap();
        let late_steps = steps.swap(0, Ordering::Relaxed);
        assert_eq!((first.entries.len(), late.entries.len()), (256, 256));
        assert_eq!(late.entries[0].location.display(), "s3://bucket/foo/019001");
        assert!(
            late_steps <= first_steps * 2 + 1000,
            "early VM steps={first_steps}, late={late_steps}"
        );
        eprintln!(
            "SQLite {}: early page {first_steps} VM steps, late page {late_steps}",
            SqliteIndex::sqlite_version()
        );
        db.progress_handler(0, None::<fn() -> bool>).unwrap();
        let before = page(&db, &scope, Some(b"a".to_vec()), 256).unwrap();
        assert_eq!(before.entries[0].location, first.entries[0].location);
        assert!(
            page(&db, &scope, Some(b"z".to_vec()), 256)
                .unwrap()
                .entries
                .is_empty()
        );
        scope.recursive = false;
        scope.root = ObjectId::parse_s3("s3://bucket/foo/000001").unwrap();
        assert_eq!(
            page(&db, &scope, Some(b"a".to_vec()), 256)
                .unwrap()
                .entries
                .len(),
            1
        );
        for cursor in [b"foo/000001".to_vec(), b"z".to_vec()] {
            assert!(
                page(&db, &scope, Some(cursor), 256)
                    .unwrap()
                    .entries
                    .is_empty()
            );
        }
    }
}

fn page(
    db: &Connection,
    scope: &IndexScope,
    cursor: Option<Vec<u8>>,
    limit: usize,
) -> Result<IndexPage, OperationError> {
    let (selected, ignore) = selection(scope)?;
    let Location::Remote(root) = &selected.root else {
        unreachable!()
    };
    let lower = root.key.as_bytes();
    let upper = if scope.recursive {
        successor(lower.to_vec())
    } else {
        None
    };
    let limit = limit.clamp(1, MAX_PAGE_ENTRIES);
    // Use explicit key ranges: nullable-parameter OR predicates can force a
    // rescan from the bucket start for each page on large indexes.
    use rusqlite::types::Value;
    let mut sql =
        String::from("SELECT * FROM objects WHERE namespace=?1 AND scheme=?2 AND authority=?3");
    let mut values = vec![
        Value::Text(root.namespace.clone()),
        Value::Text(root.scheme.clone()),
        Value::Text(root.authority.clone()),
    ];
    if scope.recursive {
        // SQLite can choose the weaker prefix bound when both >=prefix and
        // >cursor are present, then rescan every earlier page. Supply exactly
        // one effective lower bound so each page seeks straight to its cursor.
        match cursor {
            Some(cursor) if cursor.as_slice() >= lower => {
                values.push(Value::Blob(cursor));
                sql.push_str(" AND object_key>?4");
            }
            _ => {
                values.push(Value::Blob(lower.to_vec()));
                sql.push_str(" AND object_key>=?4");
            }
        }
        if let Some(upper) = upper {
            values.push(Value::Blob(upper));
            sql.push_str(&format!(" AND object_key<?{}", values.len()));
        }
    } else {
        if cursor.as_deref().is_some_and(|cursor| cursor >= lower) {
            return Ok(IndexPage::default());
        }
        values.push(Value::Blob(lower.to_vec()));
        sql.push_str(" AND object_key=?4");
    }
    values.push(Value::Integer(limit as i64));
    sql.push_str(&format!(" ORDER BY object_key LIMIT ?{}", values.len()));
    let mut statement = db.prepare(&sql).map_err(sql_error)?;
    let mut rows = statement
        .query(rusqlite::params_from_iter(values))
        .map_err(sql_error)?;
    let mut result = IndexPage::default();
    let mut bytes = 0;
    let mut examined = 0;
    let mut last = None;
    let mut limited = false;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let meta = codec::row(row).map_err(sql_error)?;
        let Location::Remote(id) = &meta.location else {
            unreachable!()
        };
        let include =
            selected.contains(&meta.location) && !ignore.is_ignored(&meta.location, false)?;
        let length = if include {
            codec::json(&meta)?.len()
        } else {
            0
        };
        if length > MAX_PAGE_BYTES {
            return Err(error("index-page", "one record exceeds page byte limit"));
        }
        if bytes + length > MAX_PAGE_BYTES {
            limited = true;
            break;
        }
        last = Some(id.key.as_bytes().to_vec());
        examined += 1;
        bytes += length;
        if include {
            result.entries.push(meta);
        }
    }
    if limited || examined == limit {
        result.next = last;
    }
    Ok(result)
}
impl IndexRead for SqliteIndex {
    fn get<'a>(&'a self, id: &'a ObjectId) -> IndexFuture<'a, Option<ObjectMetadata>> {
        let id = id.clone();
        Box::pin(self.call(move |w| get(&w.db, &id)))
    }
    fn query<'a>(
        &'a self,
        scope: &'a IndexScope,
        cursor: Option<Vec<u8>>,
        limit: usize,
    ) -> IndexFuture<'a, IndexPage> {
        let scope = scope.clone();
        Box::pin(self.call(move |w| page(&w.db, &scope, cursor, limit)))
    }
    fn roots<'a>(&'a self, cursor: Option<ObjectId>, limit: usize) -> IndexFuture<'a, RootPage> {
        Box::pin(self.call(move |w| {
            let limit = limit.clamp(1, MAX_PAGE_ENTRIES);
            let start = cursor.map(|id| (id.namespace, id.scheme, id.authority));
            let mut statement =
                w.db.prepare(
                    "SELECT DISTINCT namespace,scheme,authority FROM objects
                 WHERE ?1 IS NULL OR (namespace,scheme,authority)>(?1,?2,?3)
                 ORDER BY namespace,scheme,authority LIMIT ?4",
                )
                .map_err(sql_error)?;
            let (a, b, c) = match &start {
                Some((a, b, c)) => (Some(a), Some(b), Some(c)),
                None => (None, None, None),
            };
            let rows = statement
                .query_map(params![a, b, c, limit as i64], |r| {
                    Ok(ObjectId {
                        namespace: r.get(0)?,
                        scheme: r.get(1)?,
                        authority: r.get(2)?,
                        key: String::new(),
                    })
                })
                .map_err(sql_error)?;
            let mut roots = Vec::new();
            let mut bytes = 0;
            let mut limited = false;
            for row in rows {
                let id = row.map_err(sql_error)?;
                let length = id.namespace.len() + id.scheme.len() + id.authority.len();
                if bytes + length > MAX_PAGE_BYTES {
                    limited = true;
                    break;
                }
                bytes += length;
                roots.push(id);
            }
            let next = if limited || roots.len() == limit {
                roots.last().cloned()
            } else {
                None
            };
            Ok(RootPage { roots, next })
        }))
    }
}
impl IndexWrite for SqliteIndex {
    fn apply<'a>(&'a self, patch: MetadataPatch) -> IndexFuture<'a, ()> {
        if let Err(e) = validate_record(&patch.clone().merge(None)) {
            return Box::pin(async { Err(e) });
        }
        Box::pin(self.call(move |w| {
            let tx =
                w.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql_error)?;
            apply(&tx, patch)?;
            tx.commit().map_err(sql_error)
        }))
    }
    fn remove<'a>(&'a self, id: &'a ObjectId) -> IndexFuture<'a, ()> {
        let id = id.clone();
        Box::pin(self.call(move |w| {
            let tx = w.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql_error)?;
            let removed = tx.execute(
                "DELETE FROM objects WHERE namespace=?1 AND scheme=?2 AND authority=?3 AND object_key=?4",
                params![id.namespace,id.scheme,id.authority,id.key.as_bytes()]
            ).map_err(sql_error)?;
            if removed > 0 { tx.execute("UPDATE metadata SET revision=revision+1 WHERE singleton=1",[]).map_err(sql_error)?; }
            tx.commit().map_err(sql_error)
        }))
    }
    fn begin_reconcile<'a>(&'a self, scope: IndexScope) -> IndexFuture<'a, Box<dyn Reconcile>> {
        Box::pin(async move {
            let (id, lease) = self
                .call(move |w| {
                    selection(&scope)?;
                    if w.reconcile.is_some() {
                        return Err(error(
                            "index-reconcile",
                            "a reconciliation is already active on this adapter",
                        ));
                    }
                    w.db.execute_batch(
                        "CREATE TEMP TABLE seen(object_key BLOB PRIMARY KEY) WITHOUT ROWID",
                    )
                    .map_err(sql_error)?;
                    let revision = w
                        .db
                        .query_row("SELECT revision FROM metadata WHERE singleton=1", [], |r| {
                            r.get(0)
                        })
                        .map_err(sql_error)?;
                    w.sequence += 1;
                    let id = w.sequence;
                    let lease = std::sync::Arc::new(());
                    w.reconcile = Some(ActiveReconcile {
                        id,
                        revision,
                        scope,
                        lease: lease.clone(),
                    });
                    Ok((id, lease))
                })
                .await?;
            Ok(Box::new(ReconcileSession {
                index: self.clone(),
                id,
                done: false,
                control: ReconcileControl::default(),
                _lease: lease,
            }) as Box<dyn Reconcile>)
        })
    }
}
struct ReconcileSession {
    index: SqliteIndex,
    id: u64,
    done: bool,
    control: ReconcileControl,
    _lease: std::sync::Arc<()>,
}
impl Reconcile for ReconcileSession {
    fn control(&self) -> ReconcileControl {
        self.control.clone()
    }
    fn observe<'a>(&'a mut self, patch: MetadataPatch) -> IndexFuture<'a, ()> {
        Box::pin(async move {
            if self.done {
                return Err(error("index-reconcile", "session already ended"));
            }
            self.control.check()?;
            validate_record(&patch.clone().merge(None))?;
            let id = self.id;
            self.index
                .call(move |w| {
                    let active = w
                        .reconcile
                        .as_ref()
                        .filter(|a| a.id == id)
                        .ok_or_else(|| error("index-reconcile", "session unavailable"))?;
                    if !matches(&active.scope, &patch.clone().merge(None))? {
                        return Err(error(
                            "index-reconcile",
                            "observation outside selected scope or ignored",
                        ));
                    }
                    let key = patch.id.key.as_bytes().to_vec();
                    let tx =
                        w.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                            .map_err(sql_error)?;
                    apply(&tx, patch)?;
                    tx.execute("INSERT OR IGNORE INTO seen VALUES(?1)", [key])
                        .map_err(sql_error)?;
                    tx.commit().map_err(sql_error)
                })
                .await
        })
    }
    fn finish<'a>(&'a mut self) -> IndexFuture<'a, u64> {
        Box::pin(async move {
            if self.done {
                return Err(error("index-reconcile", "session already ended"));
            }
            let id = self.id;
            let control = self.control.clone();
            let deleted = self.index.call(move |w| {
                let active = w.reconcile.as_ref().filter(|a| a.id == id)
                    .ok_or_else(|| error("index-reconcile","session unavailable"))?;
                let scope = active.scope.clone();
                let start_revision = active.revision;
                let mut cursor = None;
                let mut deleted = 0;
                loop {
                    control.check()?;
                    let result = page(&w.db,&scope,cursor,MAX_PAGE_ENTRIES)?;
                    let mut page_deleted = 0;
                    let tx = w.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql_error)?;
                    for meta in result.entries {
                        let Location::Remote(id) = meta.location else { unreachable!() };
                        page_deleted += tx.execute(
                            "DELETE FROM objects
                             WHERE namespace=?1 AND scheme=?2 AND authority=?3 AND object_key=?4
                             AND revision<=?5 AND NOT EXISTS(SELECT 1 FROM seen WHERE object_key=?4)",
                            params![id.namespace,id.scheme,id.authority,id.key.as_bytes(),start_revision]
                        ).map_err(sql_error)? as u64;
                    }
                    if page_deleted > 0 {
                        tx.execute("UPDATE metadata SET revision=revision+1 WHERE singleton=1",[]).map_err(sql_error)?;
                    }
                    tx.commit().map_err(sql_error)?;
                    deleted += page_deleted;
                    cursor = result.next;
                    if cursor.is_none() { break; }
                }
                w.db.execute_batch("DROP TABLE seen").map_err(sql_error)?;
                w.reconcile = None;
                Ok(deleted)
            }).await?;
            self.done = true;
            Ok(deleted)
        })
    }
    fn cancel<'a>(&'a mut self) -> IndexFuture<'a, ()> {
        Box::pin(async move {
            self.control.cancel();
            let id = self.id;
            self.index.call(move |w| cancel_reconcile(w, id)).await?;
            self.done = true;
            Ok(())
        })
    }
}
fn cancel_reconcile(w: &mut Worker, id: u64) -> Result<(), OperationError> {
    if w.reconcile.as_ref().is_some_and(|a| a.id == id) {
        w.db.execute_batch("DROP TABLE seen").map_err(sql_error)?;
        w.reconcile = None;
    }
    Ok(())
}
struct Snapshot(tempfile::NamedTempFile);
struct SnapshotCancellation(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for SnapshotCancellation {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}
fn snapshot_cancelled(stopped: &std::sync::atomic::AtomicBool) -> Result<(), OperationError> {
    if stopped.load(std::sync::atomic::Ordering::Acquire) {
        Err(OperationError::new(
            ErrorKind::Cancelled,
            "index-snapshot",
            "snapshot work cancelled",
        ))
    } else {
        Ok(())
    }
}
impl SnapshotFile for Snapshot {
    fn path(&self) -> &Path {
        self.0.path()
    }
}
impl IndexSnapshot for SqliteIndex {
    fn begin_import<'a>(&'a self) -> IndexFuture<'a, Box<dyn SnapshotImport>> {
        Box::pin(import::begin(self))
    }
    fn snapshot<'a>(&'a self) -> IndexFuture<'a, Box<dyn SnapshotFile>> {
        Box::pin(async move {
            let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let _cancellation = SnapshotCancellation(stopped.clone());
            self.call(move |w| {
                snapshot_cancelled(&stopped)?;
                #[cfg(test)]
                {
                    w.snapshot_runs += 1;
                }
                let deadline = Instant::now() + Duration::from_secs(10);
                let file =
                    tempfile::NamedTempFile::new().map_err(|e| error("index-snapshot", e))?;
                {
                    let mut target = Connection::open(file.path()).map_err(sql_error)?;
                    target
                        .execute_batch(
                            "PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;",
                        )
                        .map_err(sql_error)?;
                    let backup =
                        rusqlite::backup::Backup::new(&w.db, &mut target).map_err(sql_error)?;
                    loop {
                        snapshot_cancelled(&stopped)?;
                        if Instant::now() >= deadline {
                            return Err(error("index-snapshot", "snapshot exceeded 10 seconds"));
                        }
                        match backup.step(256).map_err(sql_error)? {
                            rusqlite::backup::StepResult::Done => break,
                            rusqlite::backup::StepResult::More => {}
                            _ => std::thread::sleep(Duration::from_millis(10)),
                        }
                    }
                }
                let mut checked = open_existing(file.path(), true)?;
                let progress_stop = stopped.clone();
                checked
                    .progress_handler(
                        10_000,
                        Some(move || {
                            Instant::now() >= deadline
                                || progress_stop.load(std::sync::atomic::Ordering::Acquire)
                        }),
                    )
                    .map_err(sql_error)?;
                let valid = validate(&checked, true);
                snapshot_cancelled(&stopped)?;
                valid?;
                drop(checked);
                Ok(Box::new(Snapshot(file)) as Box<dyn SnapshotFile>)
            })
            .await
        })
    }
    fn inspect_snapshot<'a>(&'a self, path: PathBuf) -> IndexFuture<'a, ()> {
        Box::pin(self.call(move |_| {
            let mut db = open_existing(&path, true)?;
            let deadline = Instant::now() + Duration::from_secs(10);
            db.progress_handler(10_000, Some(move || Instant::now() >= deadline))
                .map_err(sql_error)?;
            validate(&db, true)
        }))
    }
}

impl IndexDatabase for SqliteIndex {
    fn revision<'a>(&'a self) -> IndexFuture<'a, u64> {
        Box::pin(self.call(|w| {
            let revision: i64 =
                w.db.query_row("SELECT revision FROM metadata WHERE singleton=1", [], |r| {
                    r.get(0)
                })
                .map_err(sql_error)?;
            u64::try_from(revision).map_err(|e| error("index-revision", e))
        }))
    }
    fn close<'a>(&'a self) -> IndexFuture<'a, ()> {
        Box::pin(self.clone().close())
    }
}

#[cfg(test)]
mod snapshot_cancellation {
    use super::*;
    #[tokio::test]
    async fn dropped_queued_snapshot_never_starts_and_next_snapshot_still_works() {
        let temp = tempfile::tempdir().unwrap();
        let index = SqliteIndex::open(temp.path().join("index.db"))
            .await
            .unwrap();
        let (started, ready) = oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let other = index.clone();
        let blocked = tokio::spawn(async move {
            other
                .call(move |_| {
                    let _ = started.send(());
                    wait.recv_timeout(Duration::from_secs(10))
                        .map_err(|e| error("test-barrier", e))?;
                    Ok(())
                })
                .await
        });
        ready.await.unwrap();
        let mut pending = index.snapshot();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut pending)
                .await
                .is_err()
        );
        drop(pending);
        release.send(()).unwrap();
        blocked.await.unwrap().unwrap();
        assert_eq!(index.call(|w| Ok(w.snapshot_runs)).await.unwrap(), 0);
        let snapshot = index.snapshot().await.unwrap();
        index
            .inspect_snapshot(snapshot.path().into())
            .await
            .unwrap();
        assert_eq!(index.call(|w| Ok(w.snapshot_runs)).await.unwrap(), 1);
        index.close().await.unwrap();
    }
}
