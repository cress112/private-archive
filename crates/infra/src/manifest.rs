//! The transfer manifest has its own temporary database and worker. It never
//! uses the optional management index connection or keeps a file list in RAM.
use archive_domain::*;
use archive_sync::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    ffi::OsString,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

type Job = Box<dyn FnOnce(&mut Worker) + Send>;
pub struct DiskPlan {
    sender: Option<mpsc::Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
    cleanup_limit: std::sync::Arc<std::sync::Mutex<Option<Instant>>>,
}
struct Worker {
    db: Option<Connection>,
    directory: Option<tempfile::TempDir>,
    sealed: bool,
    cursor: i64,
    discarded: bool,
    cleanup_limit: std::sync::Arc<std::sync::Mutex<Option<Instant>>>,
}
fn error(cause: impl std::fmt::Display) -> OperationError {
    OperationError::new(ErrorKind::Io, "cp-manifest", cause.to_string())
}
fn collision(location: &Location) -> OperationError {
    OperationError::new(
        ErrorKind::Input,
        "cp-manifest",
        "multiple sources map to the same destination or overlapping local paths",
    )
    .at(location)
}
impl DiskPlan {
    pub async fn new() -> Result<Self, OperationError> {
        let (sender, mut receiver) = mpsc::channel::<Job>(16);
        let (ready, result) = oneshot::channel();
        let cleanup_limit = std::sync::Arc::new(std::sync::Mutex::new(None));
        let worker_limit = cleanup_limit.clone();
        let thread = std::thread::Builder::new()
            .name("archive-plan".into())
            .spawn(move || {
                let mut worker = match Worker::new(worker_limit) {
                    Ok(worker) => worker,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                while let Some(job) = receiver.blocking_recv() {
                    job(&mut worker);
                }
            })
            .map_err(error)?;
        match result.await.map_err(error)? {
            Ok(()) => Ok(Self {
                sender: Some(sender),
                thread: Some(thread),
                cleanup_limit,
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
        let (sender, receiver) = oneshot::channel();
        self.sender
            .as_ref()
            .ok_or_else(|| error("manifest already discarded"))?
            .send(Box::new(move |w| {
                let _ = sender.send(job(w));
            }))
            .await
            .map_err(error)?;
        receiver.await.map_err(error)?
    }
}
impl Worker {
    fn new(
        cleanup_limit: std::sync::Arc<std::sync::Mutex<Option<Instant>>>,
    ) -> Result<Self, OperationError> {
        let directory = tempfile::Builder::new()
            .prefix("private-archive-plan-")
            .tempdir()
            .map_err(error)?;
        let path = directory.path().join("manifest.db");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(error)?;
        let db = Connection::open(path).map_err(error)?;
        db.execute_batch(include_str!("manifest/schema.sql"))
            .map_err(error)?;
        Ok(Self {
            db: Some(db),
            directory: Some(directory),
            sealed: false,
            cursor: 0,
            discarded: false,
            cleanup_limit,
        })
    }
    fn db(&self) -> Result<&Connection, OperationError> {
        self.db
            .as_ref()
            .ok_or_else(|| error("manifest already discarded"))
    }
    fn remember(&self, path: &Path, is_dir: bool) -> Result<(), OperationError> {
        self.db()?
            .execute(
                "INSERT INTO cleanup VALUES(?1,?2,?3)",
                params![
                    path.as_os_str().as_bytes(),
                    is_dir,
                    path.components().count() as i64
                ],
            )
            .map_err(error)?;
        Ok(())
    }
    fn local_identity(&self, location: &Location, path: &Path) -> Result<Vec<u8>, OperationError> {
        let mut parent = path.parent().ok_or_else(|| collision(location))?;
        let mut suffix = vec![
            path.file_name()
                .ok_or_else(|| collision(location))?
                .to_os_string(),
        ];
        loop {
            match std::fs::metadata(parent) {
                Ok(meta) => {
                    if !meta.is_dir() {
                        return Err(collision(location));
                    }
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    suffix.push(
                        parent
                            .file_name()
                            .ok_or_else(|| collision(location))?
                            .to_os_string(),
                    );
                    parent = parent.parent().ok_or_else(|| collision(location))?;
                }
                Err(e) => return Err(error(e)),
            }
        }
        let parent = std::fs::canonicalize(parent).map_err(error)?;
        let meta = std::fs::metadata(&parent).map_err(error)?;
        let anchor = format!("{}:{}", meta.dev(), meta.ino());
        let existing: Option<Vec<u8>> = self
            .db()?
            .query_row("SELECT path FROM shadows WHERE anchor=?1", [&anchor], |r| {
                r.get(0)
            })
            .optional()
            .map_err(error)?;
        let root = if let Some(bytes) = existing {
            PathBuf::from(OsString::from_vec(bytes))
        } else {
            let dir = tempfile::Builder::new()
                .prefix(".private-archive-plan-")
                .tempdir_in(&parent)
                .map_err(error)?;
            self.remember(dir.path(), true)?;
            self.db()?
                .execute(
                    "INSERT INTO shadows VALUES(?1,?2)",
                    params![anchor, dir.path().as_os_str().as_bytes()],
                )
                .map_err(error)?;
            dir.keep()
        };
        let mut marker = root;
        // The shadow lives on the destination filesystem, so its actual case
        // and Unicode name rules detect aliases without inventing a collation.
        let count = suffix.len();
        for (n, part) in suffix.into_iter().rev().enumerate() {
            if part == "." || part == ".." {
                return Err(collision(location));
            }
            marker.push(part);
            if n + 1 < count {
                match std::fs::create_dir(&marker) {
                    Ok(()) => self.remember(&marker, true)?,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && marker.is_dir() => {}
                    Err(_) => return Err(collision(location)),
                }
            } else {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&marker)
                {
                    Ok(_) => self.remember(&marker, false)?,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        return Err(collision(location));
                    }
                    Err(e) => return Err(error(e)),
                }
            }
        }
        let meta = std::fs::metadata(&marker).map_err(error)?;
        Ok(format!("local:{}:{}", meta.dev(), meta.ino()).into_bytes())
    }
    fn cleanup(&mut self) -> Result<(), OperationError> {
        if self.discarded {
            return Ok(());
        }
        self.discarded = true;
        let deadline = self
            .cleanup_limit
            .lock()
            .unwrap()
            .map_or(Instant::now() + Duration::from_secs(3), |limit| {
                limit.min(Instant::now() + Duration::from_secs(3))
            });
        let mut result = (|| {
            loop {
                if Instant::now() >= deadline {
                    return Err(error(
                        "cleanup deadline reached; temporary manifest retained",
                    ));
                }
                let mut statement = self
                    .db()?
                    .prepare("SELECT path,is_dir FROM cleanup ORDER BY depth DESC,path LIMIT 128")
                    .map_err(error)?;
                let rows = statement
                    .query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, bool>(1)?)))
                    .map_err(error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(error)?;
                drop(statement);
                if rows.is_empty() {
                    break;
                }
                for row in rows {
                    if Instant::now() >= deadline {
                        return Err(error(
                            "shadow cleanup exceeded three seconds; temporary manifest retained",
                        ));
                    }
                    let (bytes, is_dir) = row;
                    let path = PathBuf::from(OsString::from_vec(bytes.clone()));
                    let removed = if is_dir {
                        std::fs::remove_dir(&path)
                    } else {
                        std::fs::remove_file(&path)
                    };
                    if let Err(e) = removed
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        return Err(error(format!(
                            "{}: {e}; temporary manifest retained",
                            path.display()
                        )));
                    }
                    self.db()?
                        .execute("DELETE FROM cleanup WHERE path=?1", [bytes])
                        .map_err(error)?;
                }
            }
            Ok(())
        })();
        self.db.take();
        if let Some(directory) = self.directory.take() {
            if result.is_ok() {
                directory.close().map_err(error)?;
            } else {
                if let Err(error) = &mut result {
                    error
                        .message
                        .push_str(&format!("; retained at {}", directory.path().display()));
                }
                let _ = directory.keep();
            }
        }
        result
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}
impl Drop for DiskPlan {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl PlanStore for DiskPlan {
    fn set_cleanup_deadline(&mut self, deadline: tokio::time::Instant) {
        *self.cleanup_limit.lock().unwrap() = Some(deadline.into_std());
    }
    fn append<'a>(
        &'a mut self,
        entry: ResolvedCopy,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ()> {
        Box::pin(async move {
            cancel.check("cp-manifest")?;
            let destination = entry.route.destination();
            let record = serde_json::to_string(&entry).map_err(error)?;
            if record.len() > 128 * 1024 {
                return Err(error("one manifest record exceeds 128KiB"));
            }
            self.call(move |w| {
                if w.sealed {
                    return Err(error("cannot append to sealed manifest"));
                }
                let identity = match &destination {
                    Location::Local(path) => w.local_identity(&destination, path)?,
                    Location::Remote(id) => serde_json::to_vec(id).map_err(error)?,
                };
                let result = w.db()?.execute(
                    "INSERT INTO copies(destination,record) VALUES(?1,?2)",
                    params![identity, record],
                );
                match result {
                    Ok(_) => Ok(()),
                    Err(rusqlite::Error::SqliteFailure(code, _))
                        if code.code == rusqlite::ErrorCode::ConstraintViolation =>
                    {
                        Err(collision(&destination))
                    }
                    Err(e) => Err(error(e)),
                }
            })
            .await?;
            cancel.check("cp-manifest")
        })
    }
    fn seal<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, ()> {
        Box::pin(async move {
            cancel.check("cp-manifest")?;
            self.call(|w| {
                w.db()?;
                if w.sealed {
                    return Err(error("manifest already sealed"));
                }
                w.sealed = true;
                Ok(())
            })
            .await?;
            cancel.check("cp-manifest")
        })
    }
    fn next<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, Option<ResolvedCopy>> {
        Box::pin(async move {
            cancel.check("cp-manifest")?;
            self.call(|w|{
        if !w.sealed{return Err(error("manifest must be sealed before reading"));}
        let row:Option<(i64,String)>=w.db()?.query_row("SELECT sequence,record FROM copies WHERE sequence>?1 ORDER BY sequence LIMIT 1",[w.cursor],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(error)?;
        row.map(|(n,text)|{let entry=serde_json::from_str(&text).map_err(error)?;w.cursor=n;Ok(entry)}).transpose()
    }).await
        })
    }
    fn discard<'a>(&'a mut self) -> PortFuture<'a, ()> {
        Box::pin(async move {
            if self.sender.is_none() {
                return Ok(());
            }
            let result = self.call(|w| w.cleanup()).await;
            self.sender.take();
            if let Some(thread) = self.thread.take() {
                tokio::task::spawn_blocking(move || thread.join())
                    .await
                    .map_err(error)?
                    .map_err(|_| error("manifest worker panicked"))?;
            }
            result
        })
    }
}
