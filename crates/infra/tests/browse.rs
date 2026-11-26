use archive_domain::*;
use archive_index::*;
use archive_infra::{SqliteIndex, index_working_path};
use std::{
    num::NonZeroU32,
    time::{Duration, UNIX_EPOCH},
};

fn id(key: &str) -> ObjectId {
    ObjectId::parse_s3(&format!("s3://bucket/{key}")).unwrap()
}
fn request(key: &str, projection: BrowseProjection, sort: &str) -> BrowseRequest {
    BrowseRequest {
        target: BrowseTarget::Scope(IndexScope {
            root: id(key),
            recursive: projection != BrowseProjection::Exact,
            max_depth: None,
            ignore: vec![],
        }),
        projection,
        sort: sort.parse().unwrap(),
    }
}
async fn put(db: &SqliteIndex, key: &str, size: Option<u64>) {
    let mut meta = ObjectMetadata::file(Location::Remote(id(key)), 0);
    meta.size = size;
    meta.queried_at = Some(UNIX_EPOCH + Duration::new(100, 123));
    db.apply(MetadataPatch::full(meta).unwrap()).await.unwrap();
}
async fn entries(listing: &dyn IndexListing) -> Vec<IndexEntry> {
    let mut start = 0;
    let mut entries = vec![];
    loop {
        let page = listing.page(start, 137).await.unwrap();
        assert!(page.entries.len() <= 137);
        entries.extend(page.entries);
        match page.next {
            Some(next) => {
                assert!(next > start);
                start = next;
            }
            None => return entries,
        }
    }
}
async fn keys(db: &SqliteIndex, request: BrowseRequest) -> Vec<String> {
    let listing = db.browse(request, BrowseControl::default()).await.unwrap();
    entries(&*listing)
        .await
        .into_iter()
        .map(|e| e.id.key)
        .collect()
}

#[tokio::test]
async fn multi_sort_is_left_prioritized_unknown_last_and_stable_on_ties() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(dir.path().join("index.db"))
        .await
        .unwrap();
    for (key, size) in [
        ("x/same", Some(20)),
        ("y/same", Some(5)),
        ("z/same", Some(20)),
        ("Z", Some(20)),
        ("a", Some(0)),
        ("foo/file10", Some(10)),
        ("foo/file2", Some(2)),
        ("unknown", None),
    ] {
        put(&db, key, size).await;
    }
    assert_eq!(
        keys(
            &db,
            request("", BrowseProjection::Objects, "size-asc,name-desc")
        )
        .await,
        [
            "a",
            "foo/file2",
            "y/same",
            "foo/file10",
            "x/same",
            "z/same",
            "Z",
            "unknown"
        ]
    );
    assert_eq!(
        keys(
            &db,
            request("", BrowseProjection::Objects, "name-desc,size-asc")
        )
        .await,
        [
            "unknown",
            "y/same",
            "x/same",
            "z/same",
            "foo/file2",
            "foo/file10",
            "a",
            "Z"
        ]
    );
    assert_eq!(
        keys(&db, request("", BrowseProjection::Objects, "name-asc")).await,
        [
            "Z",
            "a",
            "foo/file10",
            "foo/file2",
            "x/same",
            "y/same",
            "z/same",
            "unknown"
        ]
    );
    assert_eq!(
        keys(&db, request("", BrowseProjection::Objects, "size-desc")).await,
        [
            "Z",
            "x/same",
            "z/same",
            "foo/file10",
            "y/same",
            "foo/file2",
            "a",
            "unknown"
        ]
    );
    let asc = keys(&db, request("", BrowseProjection::Objects, "uri-asc")).await;
    let mut desc = keys(&db, request("", BrowseProjection::Objects, "uri-desc")).await;
    desc.reverse();
    assert_eq!(asc, desc);
    db.close().await.unwrap();
}

#[tokio::test]
async fn sorting_is_global_across_pages_and_query_result_does_not_hold_source_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let db = SqliteIndex::open(&path).await.unwrap();
    for i in 0..600 {
        put(&db, &format!("p/{i:04}"), Some(599 - i)).await;
    }
    let listing = db
        .browse(
            request("p", BrowseProjection::Objects, "size-asc"),
            BrowseControl::default(),
        )
        .await
        .unwrap();
    assert_eq!(listing.summary().entries, 600);
    db.close().await.unwrap();
    std::fs::remove_file(&path).unwrap();
    let middle = listing.page(300, 17).await.unwrap();
    assert_eq!(middle.entries[0].id.key, "p/0299");
    assert_eq!(middle.next, Some(317));
    let all = entries(&*listing).await;
    assert_eq!(all.len(), 600);
    for (i, entry) in all.iter().enumerate() {
        assert_eq!(entry.metadata.as_ref().unwrap().size, Some(i as u64));
    }
    assert!(listing.page(0, usize::MAX).await.unwrap().entries.len() <= MAX_PAGE_ENTRIES);
}

#[tokio::test]
async fn exact_object_precedes_virtual_directory_and_depth_ignore_boundaries_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(dir.path().join("index.db"))
        .await
        .unwrap();
    for key in [
        "foo",
        "foo/one",
        "foo/.DS_Store",
        "foo/.git/config",
        "foo/sub/deep",
        "foobar/outside",
        "literal%_/雪",
        "literalXX/outside",
    ] {
        put(&db, key, Some(12)).await;
    }
    let listing = db
        .browse(
            request("foo", BrowseProjection::Exact, "uri-asc"),
            BrowseControl::default(),
        )
        .await
        .unwrap();
    let exact = entries(&*listing).await;
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].kind, IndexEntryKind::Object);
    let mut ignored = request("foo", BrowseProjection::Exact, "uri-asc");
    if let BrowseTarget::Scope(s) = &mut ignored.target {
        s.ignore = vec!["foo".into()];
    }
    let listing = db.browse(ignored, BrowseControl::default()).await.unwrap();
    assert_eq!(listing.summary().entries, 0);
    assert!(listing.summary().directly_ignored);
    db.remove(&id("foo")).await.unwrap();
    let listing = db
        .browse(
            request("foo", BrowseProjection::Exact, "uri-asc"),
            BrowseControl::default(),
        )
        .await
        .unwrap();
    let folder = entries(&*listing).await;
    assert_eq!(folder[0].id.key, "foo/");
    assert_eq!(folder[0].kind, IndexEntryKind::Directory);
    assert!(folder[0].metadata.is_none());
    let mut shallow = request("foo", BrowseProjection::Objects, "uri-asc");
    if let BrowseTarget::Scope(s) = &mut shallow.target {
        s.max_depth = NonZeroU32::new(1);
    }
    assert_eq!(keys(&db, shallow).await, ["foo/.DS_Store", "foo/one"]);
    assert_eq!(
        keys(&db, request("foo", BrowseProjection::Objects, "uri-asc")).await,
        [
            "foo/.DS_Store",
            "foo/.git/config",
            "foo/one",
            "foo/sub/deep"
        ]
    );
    assert_eq!(
        keys(
            &db,
            request("literal%_", BrowseProjection::Objects, "uri-asc")
        )
        .await,
        ["literal%_/雪"]
    );
    assert!(
        keys(&db, request("missing", BrowseProjection::Exact, "uri-asc"))
            .await
            .is_empty()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn children_keep_object_prefix_identity_and_do_not_aggregate_directory_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(dir.path().join("index.db"))
        .await
        .unwrap();
    for (key, size) in [
        ("same", Some(9)),
        ("same/a", Some(1)),
        ("same/b", Some(2)),
        ("unknown", None),
    ] {
        put(&db, key, size).await;
    }
    let listing = db
        .browse(
            request("", BrowseProjection::Children, "size-desc"),
            BrowseControl::default(),
        )
        .await
        .unwrap();
    let rows = entries(&*listing).await;
    assert_eq!(
        rows.iter().map(|r| r.id.key.as_str()).collect::<Vec<_>>(),
        ["same", "same/", "unknown"]
    );
    assert_eq!(rows[0].kind, IndexEntryKind::Object);
    assert_eq!(rows[1].kind, IndexEntryKind::Directory);
    assert!(rows[1].metadata.is_none());
    db.close().await.unwrap();
}

#[tokio::test]
async fn all_ignored_pages_advance_and_root_list_comes_only_from_visible_object_records() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(dir.path().join("index.db"))
        .await
        .unwrap();
    for i in 0..300 {
        put(&db, &format!("p/{i}.tmp"), Some(1)).await;
    }
    let mut req = request("p", BrowseProjection::Objects, "uri-asc");
    if let BrowseTarget::Scope(s) = &mut req.target {
        s.ignore = vec!["*.tmp".into()];
    }
    let listing = db.browse(req, BrowseControl::default()).await.unwrap();
    assert_eq!(listing.summary().entries, 0);
    assert_eq!(listing.summary().ignored, 300);
    assert_eq!(listing.excluded(256, 256).await.unwrap().ids.len(), 44);
    let root_req = BrowseRequest {
        target: BrowseTarget::Roots {
            ignore: vec!["*.tmp".into()],
        },
        projection: BrowseProjection::Exact,
        sort: SortSpec::default(),
    };
    assert!(keys(&db, root_req.clone()).await.is_empty());
    put(&db, "visible", Some(1)).await;
    let listing = db
        .browse(root_req.clone(), BrowseControl::default())
        .await
        .unwrap();
    let roots = entries(&*listing).await;
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].uri(), "s3://bucket");
    assert!(roots[0].metadata.is_none());
    db.remove(&id("visible")).await.unwrap();
    assert!(keys(&db, root_req).await.is_empty());
    db.close().await.unwrap();
}

#[tokio::test]
async fn pages_respect_byte_cap_and_cancellation_does_not_mutate_observations() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteIndex::open(dir.path().join("index.db"))
        .await
        .unwrap();
    for i in 0..6 {
        let mut m = ObjectMetadata::file(Location::Remote(id(&format!("p/{i}"))), 1);
        m.queried_at = Some(UNIX_EPOCH);
        m.data_class = Some(StoredClass::Other("X".repeat(100_000)));
        db.apply(MetadataPatch::full(m).unwrap()).await.unwrap();
    }
    let control = BrowseControl::default();
    let listing = db
        .browse(
            request("p", BrowseProjection::Objects, "uri-asc"),
            control.clone(),
        )
        .await
        .unwrap();
    let first = listing.page(0, 256).await.unwrap();
    assert!(first.entries.len() < 6);
    assert!(first.next.is_some());
    assert_eq!(entries(&*listing).await.len(), 6);
    control.cancel();
    assert!(matches!(
        listing.page(0, 256).await,
        Err(OperationError {
            kind: ErrorKind::Cancelled,
            ..
        })
    ));
    let stopped = BrowseControl::default();
    stopped.cancel();
    assert!(matches!(
        db.browse(request("p", BrowseProjection::Objects, "uri-asc"), stopped)
            .await,
        Err(OperationError {
            kind: ErrorKind::Cancelled,
            ..
        })
    ));
    assert_eq!(
        db.get(&id("p/0")).await.unwrap().unwrap().queried_at,
        Some(UNIX_EPOCH)
    );
    db.close().await.unwrap();
}

#[test]
fn cache_path_is_local_and_distinguishes_namespace_and_literal_keys() {
    let home = std::path::Path::new("/home/example");
    let a = Location::parse(std::ffi::OsStr::new("s3://bucket/a%20.db"), home).unwrap();
    let b = Location::parse(std::ffi::OsStr::new("s3://bucket/a .db"), home).unwrap();
    let path = index_working_path(home, &a, "aws");
    assert!(path.starts_with(home.join(".private-archive/cache/index")));
    assert!(path.ends_with("working.db"));
    assert_ne!(path, index_working_path(home, &b, "aws"));
    assert_ne!(
        path,
        index_working_path(home, &a, "test:http://127.0.0.1:8333")
    );
}
