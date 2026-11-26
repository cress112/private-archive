use archive_domain::*;
use archive_index::*;
use archive_infra::SqliteIndex;
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn id(key: &str) -> ObjectId {
    ObjectId::parse_s3(&format!("s3://bucket/{key}")).unwrap()
}
fn observed(key: &str) -> ObjectMetadata {
    let mut m = ObjectMetadata::file(Location::Remote(id(key)), 100_000_000_000);
    m.queried_at = Some(UNIX_EPOCH + Duration::new(123, 456));
    m.version_token = Some("version-a".into());
    m
}
fn full(key: &str) -> MetadataPatch {
    MetadataPatch::full(observed(key)).unwrap()
}
fn scope(prefix: &str) -> IndexScope {
    IndexScope {
        root: id(prefix),
        recursive: true,
        max_depth: None,
        ignore: vec![],
    }
}
async fn all(index: &SqliteIndex, scope: &IndexScope) -> Vec<ObjectMetadata> {
    let mut cursor = None;
    let mut result = vec![];
    loop {
        let page = index.query(scope, cursor, 256).await.unwrap();
        assert!(page.entries.len() <= 256);
        result.extend(page.entries);
        cursor = page.next;
        if cursor.is_none() {
            return result;
        }
    }
}

#[tokio::test]
async fn metadata_roundtrip_unknowns_integer_bounds_and_query_time() {
    eprintln!("bundled SQLite {}", SqliteIndex::sqlite_version());
    let version: Vec<_> = SqliteIndex::sqlite_version()
        .split('.')
        .map(|s| s.parse::<u32>().unwrap())
        .collect();
    assert!(version.as_slice() >= [3, 51, 3].as_slice());
    let temp = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(temp.path().join("index.db"))
        .await
        .unwrap();
    let mut meta = observed("a//./literal%20?_雪");
    meta.original_mtime = Some(UNIX_EPOCH - Duration::new(2, 123));
    meta.original_creation_time = Some(UNIX_EPOCH + Duration::new(2, 789));
    meta.unix_mode = Some(0o640);
    meta.data_class = Some(StoredClass::Other("external".into()));
    meta.restore_state = RestoreState::Restored {
        until: Some(UNIX_EPOCH + Duration::from_secs(999)),
    };
    meta.checksum = Some(Checksum {
        algorithm: ChecksumAlgorithm::Sha256,
        scope: ChecksumScope::FullObject,
        value: vec![17; 32],
    });
    db.apply(MetadataPatch::full(meta.clone()).unwrap())
        .await
        .unwrap();
    let read = db.get(&id("a//./literal%20?_雪")).await.unwrap().unwrap();
    assert_eq!(read.size, meta.size);
    assert_eq!(read.original_mtime, meta.original_mtime);
    assert_eq!(read.original_creation_time, meta.original_creation_time);
    assert_eq!(read.checksum, meta.checksum);
    assert_eq!(read.queried_at, meta.queried_at);
    assert_eq!(read.restore_state, meta.restore_state);
    assert_eq!(read.unix_mode, meta.unix_mode);
    let mut invalid = observed("overflow");
    invalid.size = Some(u64::MAX);
    assert!(
        db.apply(MetadataPatch::full(invalid).unwrap())
            .await
            .is_err()
    );
    assert!(db.get(&id("overflow")).await.unwrap().is_none());
    db.apply(full("unknown")).await.unwrap();
    let unknown = db.get(&id("unknown")).await.unwrap().unwrap();
    assert!(unknown.original_mtime.is_none());
    assert!(unknown.data_class.is_none());
    assert_eq!(unknown.restore_state, RestoreState::Unknown);
}
#[tokio::test]
async fn keyset_pages_prefix_boundaries_and_filtered_pages_advance() {
    let temp = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(temp.path().join("index.db"))
        .await
        .unwrap();
    for n in 0..600 {
        db.apply(full(&format!("foo/{n:04}"))).await.unwrap();
    }
    for key in ["foo", "foobar/a", "foo%_/a", "foo/_%literal"] {
        db.apply(full(key)).await.unwrap();
    }
    assert_eq!(all(&db, &scope("foo")).await.len(), 601);
    assert_eq!(all(&db, &scope("foo%_")).await.len(), 1);
    let mut ignored = scope("foo");
    ignored.ignore = vec!["*".into()];
    let first = db.query(&ignored, None, 256).await.unwrap();
    assert!(first.entries.is_empty());
    assert!(first.next.is_some());
    assert!(all(&db, &ignored).await.is_empty());
    let exact = IndexScope {
        root: id("foo"),
        recursive: false,
        max_depth: None,
        ignore: vec![],
    };
    assert_eq!(all(&db, &exact).await.len(), 1);
}
#[tokio::test]
async fn patches_preserve_only_confirmed_same_version_and_observed_unknown_clears() {
    let temp = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(temp.path().join("index.db"))
        .await
        .unwrap();
    let mut meta = observed("file");
    meta.unix_mode = Some(0o600);
    meta.original_mtime = Some(UNIX_EPOCH);
    db.apply(MetadataPatch::full(meta).unwrap()).await.unwrap();
    let mut patch = MetadataPatch::listing(observed("file")).unwrap();
    patch.queried_at = SystemTime::now();
    db.apply(patch).await.unwrap();
    assert_eq!(
        db.get(&id("file")).await.unwrap().unwrap().unix_mode,
        Some(0o600)
    );
    let mut patch = MetadataPatch::listing(observed("file")).unwrap();
    patch.original_mtime = FieldPatch::Observed(None);
    db.apply(patch).await.unwrap();
    let read = db.get(&id("file")).await.unwrap().unwrap();
    assert_eq!(read.unix_mode, Some(0o600));
    assert!(read.original_mtime.is_none());
    let mut changed = observed("file");
    changed.version_token = Some("version-b".into());
    db.apply(MetadataPatch::listing(changed).unwrap())
        .await
        .unwrap();
    assert!(
        db.get(&id("file"))
            .await
            .unwrap()
            .unwrap()
            .unix_mode
            .is_none()
    );
}
#[tokio::test]
async fn reconcile_cancel_preserves_unseen_and_finish_respects_ignore_depth_and_concurrent_writes()
{
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.db");
    let db = SqliteIndex::open(&path).await.unwrap();
    let other = SqliteIndex::open(&path).await.unwrap();
    for key in [
        "foo/seen",
        "foo/stale",
        "foo/keep.tmp",
        "foo/deep/file",
        "foo/concurrent",
        "outside/file",
    ] {
        db.apply(full(key)).await.unwrap();
    }
    let mut selected = scope("foo");
    selected.max_depth = std::num::NonZeroU32::new(1);
    selected.ignore = vec!["*.tmp".into()];
    let mut partial = db.begin_reconcile(selected.clone()).await.unwrap();
    partial.observe(full("foo/new")).await.unwrap();
    partial.cancel().await.unwrap();
    assert!(db.get(&id("foo/stale")).await.unwrap().is_some());
    assert!(db.get(&id("foo/new")).await.unwrap().is_some());
    let mut scan = db.begin_reconcile(selected).await.unwrap();
    scan.observe(full("foo/seen")).await.unwrap();
    other.apply(full("foo/concurrent")).await.unwrap();
    assert_eq!(scan.finish().await.unwrap(), 2);
    for key in [
        "foo/seen",
        "foo/keep.tmp",
        "foo/deep/file",
        "foo/concurrent",
        "outside/file",
    ] {
        assert!(db.get(&id(key)).await.unwrap().is_some(), "{key}");
    }
    assert!(db.get(&id("foo/stale")).await.unwrap().is_none());
    assert!(db.get(&id("foo/new")).await.unwrap().is_none());
    let entries = all(&db, &scope("")).await;
    for entry in entries {
        let Location::Remote(id) = entry.location else {
            panic!()
        };
        db.remove(&id).await.unwrap();
    }
    assert!(db.roots(None, 256).await.unwrap().roots.is_empty());
}
#[tokio::test]
async fn snapshot_from_wal_is_consistent_and_bad_existing_files_are_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.db");
    let db = SqliteIndex::open(&path).await.unwrap();
    db.apply(full("snap")).await.unwrap();
    let snapshot = db.snapshot().await.unwrap();
    db.inspect_snapshot(snapshot.path().into()).await.unwrap();
    let copy = temp.path().join("snapshot.db");
    std::fs::copy(snapshot.path(), &copy).unwrap();
    let read = SqliteIndex::open(copy).await.unwrap();
    assert!(read.get(&id("snap")).await.unwrap().is_some());
    for (name, bytes) in [
        ("empty", b"".as_slice()),
        ("corrupt", b"not a database".as_slice()),
    ] {
        let path = temp.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        assert!(SqliteIndex::open(&path).await.is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    let foreign = temp.path().join("foreign.db");
    {
        let conn = rusqlite::Connection::open(&foreign).unwrap();
        conn.execute_batch("CREATE TABLE keep(value); PRAGMA user_version=999")
            .unwrap();
    }
    assert!(SqliteIndex::open(foreign).await.is_err());
    let unknown = temp.path().join("unknown.db");
    std::fs::copy(snapshot.path(), &unknown).unwrap();
    {
        let conn = rusqlite::Connection::open(&unknown).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    assert!(SqliteIndex::open(unknown).await.is_err());
}
#[tokio::test]
async fn sqlite_process_child() {
    let Some(path) = std::env::var_os("ARCHIVE_INDEX_TEST_DB") else {
        return;
    };
    let number = std::env::var("ARCHIVE_INDEX_TEST_NUMBER").unwrap();
    let barrier = std::env::var_os("ARCHIVE_INDEX_TEST_GO").unwrap();
    let start = std::time::Instant::now();
    while !Path::new(&barrier).exists() {
        assert!(start.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let db = SqliteIndex::open(path).await.unwrap();
    for n in 0..50 {
        let key = format!("process-{number}/{n}");
        db.apply(full(&key)).await.unwrap();
        assert!(db.get(&id(&key)).await.unwrap().is_some());
        let _ = db.roots(None, 256).await.unwrap();
    }
}
#[test]
fn two_processes_initialize_query_and_write_the_same_database() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("shared.db");
    let barrier = temp.path().join("go");
    let mut children = vec![];
    for number in 0..2 {
        children.push(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "sqlite_process_child", "--nocapture"])
                .env("ARCHIVE_INDEX_TEST_DB", &path)
                .env("ARCHIVE_INDEX_TEST_NUMBER", number.to_string())
                .env("ARCHIVE_INDEX_TEST_GO", &barrier)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    std::fs::write(barrier, b"go").unwrap();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let db = SqliteIndex::open(path).await.unwrap();
        assert_eq!(all(&db, &scope("")).await.len(), 100);
    });
}
#[tokio::test]
async fn sqlite_busy_is_bounded_and_does_not_replace_the_database() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("busy.db");
    let db = SqliteIndex::open(&path).await.unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = std::time::Instant::now();
    let err = db.apply(full("blocked")).await.err().unwrap();
    assert!(start.elapsed() < Duration::from_secs(4));
    assert!(err.code.is_some());
    conn.execute_batch("ROLLBACK").unwrap();
    assert!(db.get(&id("blocked")).await.unwrap().is_none());
}

#[tokio::test]
async fn reconcile_control_cancels_deletion_and_worker_shutdown_is_observed() {
    let temp = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(temp.path().join("index.db"))
        .await
        .unwrap();
    db.apply(full("keep")).await.unwrap();
    let mut scan = db.begin_reconcile(scope("")).await.unwrap();
    scan.control().cancel();
    assert_eq!(
        scan.finish().await.err().unwrap().kind,
        ErrorKind::Cancelled
    );
    scan.cancel().await.unwrap();
    assert!(db.get(&id("keep")).await.unwrap().is_some());
    drop(scan);
    let mut abandoned = db.begin_reconcile(scope("")).await.unwrap();
    abandoned.observe(full("new")).await.unwrap();
    drop(abandoned);
    let mut next = db.begin_reconcile(scope("")).await.unwrap();
    next.cancel().await.unwrap();
    drop(next);
    assert!(db.get(&id("keep")).await.unwrap().is_some());
    assert!(db.get(&id("new")).await.unwrap().is_some());
    db.close().await.unwrap();
}
