use super::*;
use std::sync::{Arc, Mutex};

struct QueryDb {
    db: Connection,
    // Drop the connection before unlinking its private query file.
    _file: tempfile::NamedTempFile,
}
struct Listing {
    db: Arc<Mutex<QueryDb>>,
    summary: ListingSummary,
    control: BrowseControl,
}
fn task_error(e: tokio::task::JoinError) -> OperationError {
    error("index-browse-worker", e)
}
fn guard(db: &mut Connection, control: &BrowseControl) -> Result<(), OperationError> {
    let control = control.clone();
    db.progress_handler(1000, Some(move || control.check().is_err()))
        .map_err(sql_error)
}
fn checked<T>(control: &BrowseControl, result: rusqlite::Result<T>) -> Result<T, OperationError> {
    control.check()?;
    result.map_err(sql_error)
}
impl Listing {
    async fn new(control: BrowseControl) -> Result<Self, OperationError> {
        let c = control.clone();
        let db=tokio::task::spawn_blocking(move||{
            c.check()?;
            let file=tempfile::NamedTempFile::new().map_err(|e|error("index-browse-temp",e))?;
            let mut db=Connection::open(file.path()).map_err(sql_error)?;
            db.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;
                CREATE TABLE entries(namespace TEXT NOT NULL,scheme TEXT NOT NULL,authority TEXT NOT NULL,object_key BLOB NOT NULL,kind INTEGER NOT NULL,uri BLOB NOT NULL,name BLOB NOT NULL,size_bytes INTEGER,metadata TEXT,PRIMARY KEY(namespace,scheme,authority,object_key,kind)) WITHOUT ROWID;
                CREATE TABLE excluded(seq INTEGER PRIMARY KEY,namespace TEXT NOT NULL,scheme TEXT NOT NULL,authority TEXT NOT NULL,object_key BLOB NOT NULL);")
                .map_err(sql_error)?;
            guard(&mut db,&c)?;
            Ok::<_,OperationError>(Arc::new(Mutex::new(QueryDb{db,_file:file})))
        }).await.map_err(task_error)??;
        Ok(Self {
            db,
            summary: ListingSummary {
                entries: 0,
                ignored: 0,
                directly_ignored: false,
            },
            control,
        })
    }
    async fn add(
        &mut self,
        entries: Vec<IndexEntry>,
        excluded: Vec<ObjectId>,
    ) -> Result<(), OperationError> {
        let db = self.db.clone();
        let control = self.control.clone();
        let start = self.summary.ignored;
        let (added, ignored) = tokio::task::spawn_blocking(move || {
            control.check()?;
            let mut db = db.lock().map_err(|e| error("index-browse-lock", e))?;
            let tx = db.db.transaction().map_err(sql_error)?;
            let mut added = 0;
            let mut insert = tx
                .prepare("INSERT OR IGNORE INTO entries VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")
                .map_err(sql_error)?;
            for entry in entries {
                control.check()?;
                let tag = match entry.kind {
                    IndexEntryKind::Object => 0,
                    IndexEntryKind::Directory => 1,
                    IndexEntryKind::Root => 2,
                };
                let wire = entry.metadata.as_ref().map(codec::json).transpose()?;
                let size = entry
                    .metadata
                    .as_ref()
                    .and_then(|m| m.size)
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|e| error("index-browse-size", e))?;
                added += checked(
                    &control,
                    insert.execute(params![
                        entry.id.namespace,
                        entry.id.scheme,
                        entry.id.authority,
                        entry.id.key.as_bytes(),
                        tag,
                        entry.uri().as_bytes(),
                        entry.name().as_bytes(),
                        size,
                        wire
                    ]),
                )? as u64;
            }
            drop(insert);
            let ignored = excluded.len() as u64;
            let mut insert_excluded = tx
                .prepare("INSERT INTO excluded VALUES(?1,?2,?3,?4,?5)")
                .map_err(sql_error)?;
            for (n, id) in excluded.into_iter().enumerate() {
                control.check()?;
                checked(
                    &control,
                    insert_excluded.execute(params![
                        (start + n as u64) as i64,
                        id.namespace,
                        id.scheme,
                        id.authority,
                        id.key.as_bytes()
                    ]),
                )?;
            }
            drop(insert_excluded);
            checked(&control, tx.commit())?;
            Ok::<_, OperationError>((added, ignored))
        })
        .await
        .map_err(task_error)??;
        self.summary.entries += added;
        self.summary.ignored += ignored;
        Ok(())
    }
    async fn finish(self, sort: SortSpec) -> Result<Self, OperationError> {
        let db = self.db.clone();
        let control = self.control.clone();
        tokio::task::spawn_blocking(move||{
            control.check()?;
            let db=db.lock().map_err(|e|error("index-browse-lock",e))?;
            let mut order=Vec::new();
            for term in sort.terms(){
                order.extend(match term {
                    SortTerm::NameAsc=>vec!["name ASC"], SortTerm::NameDesc=>vec!["name DESC"],
                    SortTerm::SizeAsc=>vec!["(size_bytes IS NULL) ASC","size_bytes ASC"],
                    SortTerm::SizeDesc=>vec!["(size_bytes IS NULL) ASC","size_bytes DESC"],
                    SortTerm::UriAsc=>vec!["uri ASC"], SortTerm::UriDesc=>vec!["uri DESC"],
                });
            }
            order.extend(["uri ASC","kind ASC","namespace ASC"]);
            // Every SQL fragment above comes from an enum, never user SQL.
            checked(&control,db.db.execute_batch(&format!("CREATE TABLE ordered(seq INTEGER PRIMARY KEY,namespace TEXT,scheme TEXT,authority TEXT,object_key BLOB,kind INTEGER,metadata TEXT);
                INSERT INTO ordered SELECT row_number() OVER(ORDER BY {})-1,namespace,scheme,authority,object_key,kind,metadata FROM entries;
                DROP TABLE entries;",order.join(","))))?;
            Ok::<_,OperationError>(())
        }).await.map_err(task_error)??;
        Ok(self)
    }
}

impl IndexListing for Listing {
    fn summary(&self) -> ListingSummary {
        self.summary
    }
    fn position<'a>(
        &'a self,
        id: &'a ObjectId,
        kind: IndexEntryKind,
    ) -> IndexFuture<'a, Option<u64>> {
        let db = self.db.clone();
        let control = self.control.clone();
        let id = id.clone();
        Box::pin(async move {
            control.check()?;
            tokio::task::spawn_blocking(move || {
                let db=db.lock().map_err(|e|error("index-browse-lock",e))?;
                let tag=match kind{IndexEntryKind::Object=>0,IndexEntryKind::Directory=>1,IndexEntryKind::Root=>2};
                checked(&control,db.db.query_row("SELECT seq FROM ordered WHERE namespace=?1 AND scheme=?2 AND authority=?3 AND object_key=?4 AND kind=?5 LIMIT 1",params![id.namespace,id.scheme,id.authority,id.key.as_bytes(),tag],|r|r.get::<_,i64>(0)).optional()).map(|v|v.map(|v|v as u64))
            }).await.map_err(task_error)?
        })
    }
    fn page<'a>(&'a self, start: u64, limit: usize) -> IndexFuture<'a, ListingPage> {
        let db = self.db.clone();
        let control = self.control.clone();
        let total = self.summary.entries;
        Box::pin(async move {
            control.check()?;
            if start >= total {
                return Ok(ListingPage {
                    entries: vec![],
                    next: None,
                });
            }
            tokio::task::spawn_blocking(move||{
                let db=db.lock().map_err(|e|error("index-browse-lock",e))?;
                let mut statement=db.db.prepare("SELECT seq,namespace,scheme,authority,object_key,kind,metadata FROM ordered WHERE seq>=?1 ORDER BY seq LIMIT ?2").map_err(sql_error)?;
                let mut rows=statement.query(params![start as i64,limit.clamp(1,MAX_PAGE_ENTRIES) as i64]).map_err(sql_error)?;
                let mut entries=Vec::new();let mut bytes=0;let mut next=start;
                while let Some(row)=checked(&control,rows.next())? {
                    let wire:Option<String>=row.get(6).map_err(sql_error)?;
                    let id=read_id(row,1)?;
                    let length=wire.as_ref().map_or(0,String::len)+id.key.len()+id.namespace.len()+id.scheme.len()+id.authority.len();
                    if length>MAX_PAGE_BYTES{return Err(error("index-browse-page","one entry exceeds page byte limit"));}
                    if bytes+length>MAX_PAGE_BYTES {break;}
                    let kind=match row.get::<_,i64>(5).map_err(sql_error)?{0=>IndexEntryKind::Object,1=>IndexEntryKind::Directory,2=>IndexEntryKind::Root,_=>return Err(error("index-browse-kind","invalid query result kind"))};
                    let metadata=wire.map(|s|serde_json::from_str(&s).map_err(|e|error("index-browse-codec",e))).transpose()?;
                    entries.push(IndexEntry{id,kind,metadata});bytes+=length;
                    next=row.get::<_,i64>(0).map_err(sql_error)? as u64+1;
                }
                Ok(ListingPage{entries,next:(next<total).then_some(next)})
            }).await.map_err(task_error)?
        })
    }
    fn excluded<'a>(&'a self, start: u64, limit: usize) -> IndexFuture<'a, ExcludedPage> {
        let db = self.db.clone();
        let control = self.control.clone();
        let total = self.summary.ignored;
        Box::pin(async move {
            control.check()?;
            if start >= total {
                return Ok(ExcludedPage {
                    ids: vec![],
                    next: None,
                });
            }
            tokio::task::spawn_blocking(move||{
                let db=db.lock().map_err(|e|error("index-browse-lock",e))?;
                let mut st=db.db.prepare("SELECT seq,namespace,scheme,authority,object_key FROM excluded WHERE seq>=?1 ORDER BY seq LIMIT ?2").map_err(sql_error)?;
                let mut rows=st.query(params![start as i64,limit.clamp(1,MAX_PAGE_ENTRIES) as i64]).map_err(sql_error)?;
                let mut ids=vec![];let mut next=start;let mut bytes=0;
                while let Some(row)=checked(&control,rows.next())?{
                    let id=read_id(row,1)?;
                    let len=id.key.len()+id.scheme.len()+id.namespace.len()+id.authority.len();
                    if len>MAX_PAGE_BYTES{return Err(error("index-browse-page","one excluded entry exceeds byte limit"));}
                    if bytes+len>MAX_PAGE_BYTES{break;}
                    bytes+=len;next=row.get::<_,i64>(0).map_err(sql_error)? as u64+1;ids.push(id);
                }
                Ok(ExcludedPage{ids,next:(next<total).then_some(next)})
            }).await.map_err(task_error)?
        })
    }
}
fn read_id(row: &rusqlite::Row<'_>, start: usize) -> Result<ObjectId, OperationError> {
    Ok(ObjectId {
        namespace: row.get(start).map_err(sql_error)?,
        scheme: row.get(start + 1).map_err(sql_error)?,
        authority: row.get(start + 2).map_err(sql_error)?,
        key: String::from_utf8(row.get(start + 3).map_err(sql_error)?)
            .map_err(|e| error("index-browse-key", e))?,
    })
}

fn ancestor(id: ObjectId, kind: IndexEntryKind) -> IndexEntry {
    IndexEntry {
        id,
        kind,
        metadata: None,
    }
}
fn object(meta: ObjectMetadata) -> Result<IndexEntry, OperationError> {
    let Location::Remote(id) = &meta.location else {
        return Err(error("index-browse", "only remote records can be listed"));
    };
    Ok(IndexEntry {
        id: id.clone(),
        kind: IndexEntryKind::Object,
        metadata: Some(meta),
    })
}

/// Streams the source DB in bounded keyset pages. No view-long read transaction.
async fn scan(
    index: &SqliteIndex,
    scope: IndexScope,
    projection: BrowseProjection,
    root_only: bool,
    out: &mut Listing,
) -> Result<(), OperationError> {
    scan_view(index, scope, projection, root_only, None, out).await
}
async fn scan_view(
    index: &SqliteIndex,
    scope: IndexScope,
    projection: BrowseProjection,
    root_only: bool,
    ignore_base: Option<ObjectId>,
    out: &mut Listing,
) -> Result<(), OperationError> {
    let selected = Selection::new(
        Location::Remote(scope.root.clone()),
        true,
        scope.max_depth,
        Path::new("/"),
    )?;
    let Location::Remote(prefix) = &selected.root else {
        unreachable!()
    };
    let filter = IgnoreFilter::new(
        ignore_base
            .map(Location::Remote)
            .unwrap_or_else(|| selected.ignore_base.clone()),
        &scope.ignore,
    )?;
    let raw = IndexScope {
        root: scope.root.clone(),
        recursive: true,
        max_depth: scope.max_depth,
        ignore: vec![],
    };
    let mut cursor = None;
    loop {
        out.control.check()?;
        let page = index.query(&raw, cursor, MAX_PAGE_ENTRIES).await?;
        let mut entries = Vec::new();
        let mut excluded = Vec::new();
        let mut found_root = false;
        for meta in page.entries {
            out.control.check()?;
            let Location::Remote(id) = &meta.location else {
                unreachable!()
            };
            if filter.is_ignored(&meta.location, false)? {
                excluded.push(id.clone());
                continue;
            }
            if root_only {
                entries.push(ancestor(
                    scope.root.with_key(String::new()),
                    IndexEntryKind::Root,
                ));
                found_root = true;
                break;
            }
            if projection == BrowseProjection::Children {
                let relative = id
                    .key
                    .strip_prefix(&prefix.key)
                    .expect("scope query obeys prefix");
                if let Some(slash) = relative.find('/') {
                    entries.push(ancestor(
                        id.with_key(format!("{}{}", prefix.key, &relative[..=slash])),
                        IndexEntryKind::Directory,
                    ));
                    // A real object whose name ends with '/' remains a distinct object.
                    if slash + 1 == relative.len() {
                        entries.push(object(meta)?);
                    }
                } else {
                    entries.push(object(meta)?);
                }
            } else {
                entries.push(object(meta)?);
            }
        }
        out.add(entries, excluded).await?;
        if found_root || page.next.is_none() {
            break;
        }
        cursor = page.next;
    }
    Ok(())
}

async fn exact(
    index: &SqliteIndex,
    scope: IndexScope,
    out: &mut Listing,
) -> Result<(), OperationError> {
    let selection = Selection::new(
        Location::Remote(scope.root.clone()),
        false,
        scope.max_depth,
        Path::new("/"),
    )?;
    let filter = IgnoreFilter::new(selection.ignore_base, &scope.ignore)?;
    if let Some(meta) = index.get(&scope.root).await? {
        if filter.is_ignored(&meta.location, false)? {
            out.summary.directly_ignored = true;
            out.add(vec![], vec![scope.root]).await?;
        } else {
            out.add(vec![object(meta)?], vec![]).await?;
        }
        return Ok(());
    }
    let key = if scope.root.key.is_empty() || scope.root.key.ends_with('/') {
        scope.root.key.clone()
    } else {
        format!("{}/", scope.root.key)
    };
    let folder = scope.root.with_key(key);
    let base_key = folder
        .key
        .strip_suffix('/')
        .unwrap_or(&folder.key)
        .rsplit_once('/')
        .map_or(String::new(), |(p, _)| format!("{p}/"));
    let filter = IgnoreFilter::new(Location::Remote(folder.with_key(base_key)), &scope.ignore)?;
    if filter.is_ignored(&Location::Remote(folder.clone()), true)? {
        out.summary.directly_ignored = true;
        out.add(vec![], vec![folder]).await?;
        return Ok(());
    }
    let raw = IndexScope {
        root: folder.clone(),
        recursive: true,
        max_depth: None,
        ignore: vec![],
    };
    let mut cursor = None;
    loop {
        out.control.check()?;
        let page = index.query(&raw, cursor, MAX_PAGE_ENTRIES).await?;
        let mut excluded = Vec::new();
        let mut found = false;
        for meta in page.entries {
            if filter.is_ignored(&meta.location, false)? {
                let Location::Remote(id) = meta.location else {
                    unreachable!()
                };
                excluded.push(id);
            } else {
                found = true;
                break;
            }
        }
        let entries = if found {
            vec![ancestor(
                folder.clone(),
                if folder.key.is_empty() {
                    IndexEntryKind::Root
                } else {
                    IndexEntryKind::Directory
                },
            )]
        } else {
            vec![]
        };
        out.add(entries, excluded).await?;
        if found || page.next.is_none() {
            return Ok(());
        }
        cursor = page.next;
    }
}

impl IndexBrowse for SqliteIndex {
    fn browse<'a>(
        &'a self,
        request: BrowseRequest,
        control: BrowseControl,
    ) -> IndexFuture<'a, Box<dyn IndexListing>> {
        Box::pin(async move {
            let mut out = Listing::new(control).await?;
            match request.target {
                BrowseTarget::Roots { ignore } => {
                    let mut cursor = None;
                    loop {
                        out.control.check()?;
                        let roots = self.roots(cursor, MAX_PAGE_ENTRIES).await?;
                        for root in roots.roots {
                            scan(
                                self,
                                IndexScope {
                                    root,
                                    recursive: true,
                                    max_depth: None,
                                    ignore: ignore.clone(),
                                },
                                BrowseProjection::Objects,
                                true,
                                &mut out,
                            )
                            .await?;
                        }
                        if roots.next.is_none() {
                            break;
                        }
                        cursor = roots.next;
                    }
                }
                BrowseTarget::Scope(scope) => {
                    if request.projection == BrowseProjection::Exact {
                        exact(self, scope, &mut out).await?;
                    } else {
                        if !scope.recursive {
                            return Err(OperationError::new(
                                ErrorKind::Input,
                                "index-browse",
                                "descendant projection requires recursive scope",
                            ));
                        }
                        scan(self, scope, request.projection, false, &mut out).await?;
                    }
                }
                BrowseTarget::View { scope, ignore_base } => {
                    if request.projection != BrowseProjection::Children {
                        return Err(error("index-browse", "view requires children projection"));
                    }
                    scan_view(
                        self,
                        scope,
                        request.projection,
                        false,
                        Some(ignore_base),
                        &mut out,
                    )
                    .await?;
                }
            }
            Ok(Box::new(out.finish(request.sort).await?) as Box<dyn IndexListing>)
        })
    }
}
