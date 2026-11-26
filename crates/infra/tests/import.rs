use archive_domain::*;
use archive_index::*;
use archive_infra::SqliteIndex;
use std::time::{Duration, UNIX_EPOCH};

fn id(key: &str) -> ObjectId {
    ObjectId::parse_s3(&format!("s3://bucket/{key}")).unwrap()
}
fn patch(key: &str, size: u64) -> MetadataPatch {
    let mut meta = ObjectMetadata::file(Location::Remote(id(key)), size);
    meta.queried_at = Some(UNIX_EPOCH + Duration::new(123, 456));
    MetadataPatch::full(meta).unwrap()
}
#[tokio::test]
async fn import_is_exact_paged_and_preserves_post_start_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SqliteIndex::open(tmp.path().join("source.db"))
        .await
        .unwrap();
    for n in 0..600 {
        source
            .apply(patch(&format!("key-{n:04}"), n))
            .await
            .unwrap();
    }
    let snapshot = source.snapshot().await.unwrap();
    let path = tmp.path().join("working.db");
    let target = SqliteIndex::open(&path).await.unwrap();
    let other = SqliteIndex::open(&path).await.unwrap();
    for key in ["stale", "new-concurrent", "key-0000"] {
        target.apply(patch(key, 999)).await.unwrap();
    }
    let mut session = target.begin_import().await.unwrap();
    other.apply(patch("new-concurrent", 777)).await.unwrap();
    other.apply(patch("key-0000", 888)).await.unwrap();
    let result = session.load(Some(snapshot.path().into())).await.unwrap();
    assert_eq!(
        result,
        ImportSummary {
            imported: 599,
            removed: 1
        }
    );
    assert!(target.get(&id("stale")).await.unwrap().is_none());
    assert_eq!(
        target
            .get(&id("new-concurrent"))
            .await
            .unwrap()
            .unwrap()
            .size,
        Some(777)
    );
    assert_eq!(
        target.get(&id("key-0000")).await.unwrap().unwrap().size,
        Some(888)
    );
    let imported = target.get(&id("key-0599")).await.unwrap().unwrap();
    assert_eq!(imported.size, Some(599));
    assert_eq!(
        imported.queried_at,
        Some(UNIX_EPOCH + Duration::new(123, 456))
    );
    assert!(imported.original_mtime.is_none());
    assert_eq!(imported.restore_state, RestoreState::Unknown);
    // A new invocation trusts the incoming copy, not previous unsent changes.
    let mut next = target.begin_import().await.unwrap();
    next.load(Some(snapshot.path().into())).await.unwrap();
    assert!(target.get(&id("new-concurrent")).await.unwrap().is_none());
    assert_eq!(
        target.get(&id("key-0000")).await.unwrap().unwrap().size,
        Some(0)
    );
    let mut empty = target.begin_import().await.unwrap();
    other.apply(patch("post-start", 333)).await.unwrap();
    assert_eq!(
        empty.load(None).await.unwrap(),
        ImportSummary {
            imported: 0,
            removed: 600
        }
    );
    assert!(target.get(&id("key-0000")).await.unwrap().is_none());
    assert_eq!(
        target.get(&id("post-start")).await.unwrap().unwrap().size,
        Some(333)
    );
    other.close().await.unwrap();
    target.close().await.unwrap();
    source.close().await.unwrap();
}
#[tokio::test]
async fn malformed_late_page_keeps_partial_import_and_never_prunes() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SqliteIndex::open(tmp.path().join("source.db"))
        .await
        .unwrap();
    for n in 0..300 {
        source
            .apply(patch(&format!("key-{n:04}"), n))
            .await
            .unwrap();
    }
    let snapshot = source.snapshot().await.unwrap();
    {
        let bad = rusqlite::Connection::open(snapshot.path()).unwrap();
        bad.execute(
            "UPDATE objects SET restore_state='invalid' WHERE object_key=?1",
            [b"key-0299".as_slice()],
        )
        .unwrap();
    }
    let target = SqliteIndex::open(tmp.path().join("working.db"))
        .await
        .unwrap();
    target.apply(patch("stale", 1)).await.unwrap();
    let mut session = target.begin_import().await.unwrap();
    assert!(session.load(Some(snapshot.path().into())).await.is_err());
    session.cancel().await.unwrap();
    assert!(target.get(&id("key-0000")).await.unwrap().is_some());
    assert!(target.get(&id("key-0299")).await.unwrap().is_none());
    assert!(target.get(&id("stale")).await.unwrap().is_some());
    target.close().await.unwrap();
    source.close().await.unwrap();
}
#[tokio::test]
async fn invalid_cancelled_and_abandoned_imports_do_not_replace_working_file() {
    let tmp = tempfile::tempdir().unwrap();
    let target = SqliteIndex::open(tmp.path().join("working.db"))
        .await
        .unwrap();
    target.apply(patch("keep", 1)).await.unwrap();
    let bad = tmp.path().join("bad.db");
    std::fs::write(&bad, b"invalid").unwrap();
    let mut session = target.begin_import().await.unwrap();
    assert!(session.load(Some(bad.clone())).await.is_err());
    session.cancel().await.unwrap();
    let mut cancelled = target.begin_import().await.unwrap();
    cancelled.control().cancel();
    assert_eq!(
        cancelled.load(Some(bad)).await.err().unwrap().kind,
        ErrorKind::Cancelled
    );
    cancelled.cancel().await.unwrap();
    drop(target.begin_import().await.unwrap());
    let mut next = target.begin_import().await.unwrap();
    next.cancel().await.unwrap();
    assert!(target.get(&id("keep")).await.unwrap().is_some());
    target.close().await.unwrap();
}
