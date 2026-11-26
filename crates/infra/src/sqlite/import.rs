use super::*;

type Key = (String, String, String, Vec<u8>);
pub(super) struct ActiveImport {
    id: u64,
    revision: i64,
    source: Option<Connection>,
    started: bool,
    cursor: Option<Key>,
    lease: std::sync::Arc<()>,
}
struct Import {
    index: SqliteIndex,
    id: u64,
    control: ImportControl,
    done: bool,
    _lease: std::sync::Arc<()>,
}

pub(super) fn discard_abandoned(w: &mut Worker) {
    if let Some(active) = &w.import
        && std::sync::Arc::strong_count(&active.lease) == 1
    {
        let _ = end(w, active.id);
    }
}
fn end(w: &mut Worker, id: u64) -> Result<(), OperationError> {
    if w.import.as_ref().is_some_and(|a| a.id == id) {
        w.db.execute_batch("DROP TABLE import_seen")
            .map_err(sql_error)?;
        w.import = None;
    }
    Ok(())
}
pub(super) async fn begin(index: &SqliteIndex) -> Result<Box<dyn SnapshotImport>, OperationError> {
    let lease = std::sync::Arc::new(());
    let held = lease.clone();
    let id = index.call(move |w| {
        if w.import.is_some() {
            return Err(error("index-import", "another import is active on this connection"));
        }
        let revision = w.db.query_row("SELECT revision FROM metadata WHERE singleton=1", [], |r| r.get(0)).map_err(sql_error)?;
        w.db.execute_batch("CREATE TEMP TABLE import_seen(namespace TEXT,scheme TEXT,authority TEXT,object_key BLOB,PRIMARY KEY(namespace,scheme,authority,object_key)) WITHOUT ROWID").map_err(sql_error)?;
        w.sequence += 1;
        let id = w.sequence;
        w.import = Some(ActiveImport { id, revision, source: None, started:false, cursor: None, lease: held });
        Ok(id)
    }).await?;
    Ok(Box::new(Import {
        index: index.clone(),
        id,
        control: ImportControl::default(),
        done: false,
        _lease: lease,
    }))
}
fn key(meta: &ObjectMetadata) -> Key {
    let Location::Remote(id) = &meta.location else {
        unreachable!()
    };
    (
        id.namespace.clone(),
        id.scheme.clone(),
        id.authority.clone(),
        id.key.as_bytes().to_vec(),
    )
}
/// Whole-file keyset scan, with both count and encoded-record byte bounds.
fn page(db: &Connection, after: Option<&Key>) -> Result<Vec<ObjectMetadata>, OperationError> {
    let mut sql = String::from("SELECT * FROM objects");
    if after.is_some() {
        sql.push_str(" WHERE (namespace,scheme,authority,object_key)>(?1,?2,?3,?4)");
    }
    sql.push_str(" ORDER BY namespace,scheme,authority,object_key LIMIT 256");
    let mut statement = db.prepare(&sql).map_err(sql_error)?;
    let mut rows = match after {
        Some((ns, scheme, authority, key)) => statement.query(params![ns, scheme, authority, key]),
        None => statement.query([]),
    }
    .map_err(sql_error)?;
    let mut entries = vec![];
    let mut bytes = 0;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let meta = codec::row(row).map_err(sql_error)?;
        validate_record(&meta)?;
        let len = codec::json(&meta)?.len();
        if !entries.is_empty() && bytes + len > MAX_PAGE_BYTES {
            break;
        }
        bytes += len;
        entries.push(meta);
    }
    Ok(entries)
}
impl SnapshotImport for Import {
    fn control(&self) -> ImportControl {
        self.control.clone()
    }
    fn load<'a>(&'a mut self, path: Option<PathBuf>) -> IndexFuture<'a, ImportSummary> {
        Box::pin(async move {
            if self.done {
                return Err(error("index-import", "session already ended"));
            }
            self.control.check()?;
            let id = self.id;
            let control = self.control.clone();
            self.index
                .call(move |w| {
                    let active = w
                        .import
                        .as_mut()
                        .filter(|a| a.id == id)
                        .ok_or_else(|| error("index-import", "session unavailable"))?;
                    if active.started {
                        return Err(error("index-import", "load already started"));
                    }
                    active.started = true;
                    let Some(path) = path else {
                        return Ok(());
                    };
                    let mut source = open_existing(&path, true)?;
                    source
                        .execute_batch(
                            "PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;",
                        )
                        .map_err(sql_error)?;
                    source
                        .progress_handler(1000, Some(move || control.check().is_err()))
                        .map_err(sql_error)?;
                    validate(&source, true)?;
                    active.source = Some(source);
                    Ok(())
                })
                .await?;
            let mut summary = ImportSummary::default();
            loop {
                self.control.check()?;
                let control = self.control.clone();
                let (count, exhausted) = self.index.call(move |w| {
                    control.check()?;
                    let active = w.import.as_mut().filter(|a| a.id == id).ok_or_else(|| error("index-import", "session unavailable"))?;
                    let Some(source) = active.source.as_ref() else { return Ok((0,true)); };
                    let entries = page(source, active.cursor.as_ref())?;
                    if entries.is_empty() { return Ok((0, true)); }
                    let last = key(entries.last().unwrap());
                    let tx = w.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql_error)?;
                    let mut imported = 0;
                    for meta in entries {
                        control.check()?;
                        let (ns, scheme, authority, key) = key(&meta);
                        let revision: Option<i64> = tx.query_row("SELECT revision FROM objects WHERE namespace=?1 AND scheme=?2 AND authority=?3 AND object_key=?4", params![ns,scheme,authority,key], |r| r.get(0)).optional().map_err(sql_error)?;
                        if revision.is_none_or(|r| r <= active.revision) {
                            apply(&tx, MetadataPatch::full(meta)?)?;
                            imported += 1;
                        }
                        tx.execute("INSERT OR IGNORE INTO import_seen VALUES(?1,?2,?3,?4)",params![ns,scheme,authority,key]).map_err(sql_error)?;
                    }
                    tx.commit().map_err(sql_error)?;
                    active.cursor = Some(last);
                    Ok((imported, false))
                }).await?;
                summary.imported += count;
                if exhausted {
                    break;
                }
            }
            // No absence-based deletion until every incoming page succeeded.
            let mut cursor = None;
            loop {
                self.control.check()?;
                let control = self.control.clone();
                let (removed, next) = self.index.call(move |w| {
                    control.check()?;
                    let revision = w.import.as_ref().filter(|a| a.id == id).ok_or_else(|| error("index-import", "session unavailable"))?.revision;
                    let entries = page(&w.db, cursor.as_ref())?;
                    let next = entries.last().map(key);
                    let tx = w.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql_error)?;
                    let mut removed = 0;
                    for meta in entries {
                        control.check()?;
                        let (ns, scheme, authority, key) = key(&meta);
                        removed += tx.execute("DELETE FROM objects WHERE namespace=?1 AND scheme=?2 AND authority=?3 AND object_key=?4 AND revision<=?5 AND NOT EXISTS(SELECT 1 FROM import_seen s WHERE s.namespace=?1 AND s.scheme=?2 AND s.authority=?3 AND s.object_key=?4)",params![ns,scheme,authority,key,revision]).map_err(sql_error)? as u64;
                    }
                    if removed > 0 { tx.execute("UPDATE metadata SET revision=revision+1 WHERE singleton=1",[]).map_err(sql_error)?; }
                    tx.commit().map_err(sql_error)?;
                    Ok((removed,next))
                }).await?;
                summary.removed += removed;
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
            self.index.call(move |w| end(w, id)).await?;
            self.done = true;
            Ok(summary)
        })
    }
    fn cancel<'a>(&'a mut self) -> IndexFuture<'a, ()> {
        Box::pin(async move {
            self.control.cancel();
            let id = self.id;
            self.index.call(move |w| end(w, id)).await?;
            self.done = true;
            Ok(())
        })
    }
}
