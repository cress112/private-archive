use archive_config::{ConfigKey, Paths};
use archive_domain::*;
use archive_index::*;
use archive_infra::SqliteIndex;
use clap::Parser;
use private_archive_gui::{
    GuiArgs,
    browser::{Browser, details},
    cache::{Page, ROW_CACHE_BUDGET, Row, RowCache},
    settings,
};
use std::{path::Path, time::UNIX_EPOCH};
fn args() -> GuiArgs {
    GuiArgs::parse_from(["gui", "--config", "config.toml"])
}
fn paths(root: &Path) -> Paths {
    Paths {
        home: root.join("home"),
        cwd: root.into(),
    }
}
async fn seed(root: &Path, keys: &[(&str, u64)]) {
    std::fs::write(root.join("config.toml"), "[index]\nlocation = 'data.db'\n").unwrap();
    let db = SqliteIndex::open(root.join("data.db")).await.unwrap();
    for (key, size) in keys {
        let mut m = ObjectMetadata::file(
            Location::Remote(ObjectId::parse_s3(&format!("s3://bucket/{key}")).unwrap()),
            *size,
        );
        m.queried_at = Some(UNIX_EPOCH);
        db.apply(MetadataPatch::full(m).unwrap()).await.unwrap();
    }
    db.close().await.unwrap();
}
async fn browser(root: &Path) -> Browser {
    Browser::new(
        args().prepare(paths(root)).unwrap(),
        BrowseControl::default(),
    )
    .await
    .unwrap()
}
async fn names(browser: &Browser, column: usize) -> Vec<String> {
    browser.columns[column]
        .listing
        .page(0, 256)
        .await
        .unwrap()
        .entries
        .iter()
        .map(|e| format!("{}:{}", e.name(), e.kind.as_str()))
        .collect()
}
#[tokio::test]
async fn columns_keep_file_prefix_identity_and_ten_local_details() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    seed(
        root,
        &[
            ("folder", 2),
            ("folder/a", 3),
            ("folder/z", 1),
            ("folder/sub/deep", 4),
        ],
    )
    .await;
    let mut app = browser(root).await;
    app.select(0, 0, BrowseControl::default()).await.unwrap();
    assert_eq!(names(&app, 1).await, ["folder:object", "folder:directory"]);
    app.select(1, 1, BrowseControl::default()).await.unwrap();
    assert_eq!(
        names(&app, 2).await,
        ["a:object", "sub:directory", "z:object"]
    );
    app.select(2, 0, BrowseControl::default()).await.unwrap();
    let detail = details(app.columns[2].selected.as_ref());
    assert_eq!(detail.len(), 10);
    assert_eq!(detail[0].1, "s3://bucket/folder/a");
    assert_eq!(detail[2].1, "3");
    assert_eq!(detail[4].1, "不明");
    app.refresh(
        None,
        Some("size-asc,name-desc".parse().unwrap()),
        BrowseControl::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        names(&app, 2).await,
        ["z:object", "a:object", "sub:directory"]
    );
    assert_eq!(app.columns[2].selected_row, Some(1));
    assert_eq!(
        app.columns[1].selected.as_ref().unwrap().kind,
        IndexEntryKind::Directory
    );
}
#[tokio::test]
async fn reload_keeps_uri_then_falls_back_to_parent_and_removes_last_bucket() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    seed(root, &[("folder/sub/a", 7), ("folder/keep", 2)]).await;
    let mut app = browser(root).await;
    app.select(0, 0, BrowseControl::default()).await.unwrap();
    app.select(1, 0, BrowseControl::default()).await.unwrap();
    app.select(2, 1, BrowseControl::default()).await.unwrap();
    app.select(3, 0, BrowseControl::default()).await.unwrap();
    app.reload(&args(), BrowseControl::default()).await.unwrap();
    assert_eq!(app.columns.len(), 4);
    assert_eq!(
        app.columns[3].selected.as_ref().unwrap().id.key,
        "folder/sub/a"
    );
    let db = SqliteIndex::open(root.join("data.db")).await.unwrap();
    db.remove(&ObjectId::parse_s3("s3://bucket/folder/sub/a").unwrap())
        .await
        .unwrap();
    db.close().await.unwrap();
    app.reload(&args(), BrowseControl::default()).await.unwrap();
    assert_eq!(app.columns.len(), 3);
    assert_eq!(app.columns[1].selected.as_ref().unwrap().id.key, "folder/");
    assert!(app.columns[2].selected.is_none());
    let db = SqliteIndex::open(root.join("data.db")).await.unwrap();
    db.remove(&ObjectId::parse_s3("s3://bucket/folder/keep").unwrap())
        .await
        .unwrap();
    db.close().await.unwrap();
    app.reload(&args(), BrowseControl::default()).await.unwrap();
    assert_eq!(app.columns.len(), 1);
    assert_eq!(app.columns[0].listing.summary().entries, 0);
}
#[tokio::test]
async fn ignore_origin_stays_at_bucket_and_location_changes_reset_navigation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    seed(
        root,
        &[
            ("folder/hidden/a", 7),
            ("folder/visible/a", 2),
            (".git/config", 1),
        ],
    )
    .await;
    std::fs::write(
        root.join("config.toml"),
        "ignore=['folder/hidden/**']\n[index]\nlocation='data.db'\n",
    )
    .unwrap();
    let mut app = browser(root).await;
    app.select(0, 0, BrowseControl::default()).await.unwrap();
    assert_eq!(names(&app, 1).await, [".git:directory", "folder:directory"]);
    app.select(1, 1, BrowseControl::default()).await.unwrap();
    assert_eq!(names(&app, 2).await, ["visible:directory"]);
    app.select(2, 0, BrowseControl::default()).await.unwrap();
    assert_eq!(names(&app, 3).await, ["a:object"]);
    std::fs::write(root.join("config.toml"), "[index]\nlocation='new.db'\n").unwrap();
    app.reload(&args(), BrowseControl::default()).await.unwrap();
    assert_eq!(app.columns.len(), 1);
    assert_eq!(app.columns[0].listing.summary().entries, 0);
    let db = SqliteIndex::open(root.join("data.db")).await.unwrap();
    assert!(
        db.get(&ObjectId::parse_s3("s3://bucket/folder/hidden/a").unwrap())
            .await
            .unwrap()
            .is_some()
    );
    db.close().await.unwrap();
}
#[test]
fn settings_use_raw_saved_values_and_validate_batches_before_atomic_save() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let p = paths(root);
    let args = args();
    let (exists, rows) = settings::read(&args, &p).unwrap();
    assert!(!exists);
    assert!(rows.iter().all(|r| r.saved.is_none()));
    assert!(!root.join("config.toml").exists());
    archive_config::init(&p, args.config.as_deref()).unwrap();
    settings::save(
        &args,
        &p,
        &[
            (ConfigKey::IndexLocation, Some("~/archive.db".into())),
            (ConfigKey::LogLevel, Some("warn".into())),
        ],
    )
    .unwrap();
    let (_, rows) = settings::read(&args, &p).unwrap();
    assert_eq!(rows[0].saved.as_deref(), Some("~/archive.db"));
    let before = std::fs::read(root.join("config.toml")).unwrap();
    assert!(
        settings::save(
            &args,
            &p,
            &[
                (ConfigKey::LogLevel, Some("debug".into())),
                (ConfigKey::DataClass, Some("bogus".into()))
            ]
        )
        .is_err()
    );
    assert_eq!(std::fs::read(root.join("config.toml")).unwrap(), before);
    settings::save(&args, &p, &[(ConfigKey::LogLevel, None)]).unwrap();
    let (_, rows) = settings::read(&args, &p).unwrap();
    assert!(rows[4].saved.is_none());
    assert_eq!(rows[4].effective, "\"info\"");
    let runtime = GuiArgs::parse_from(["gui", "--config", "config.toml", "--log-level", "trace"]);
    let (_, rows) = settings::read(&runtime, &p).unwrap();
    assert_eq!(rows[4].effective, "\"trace\"");
    assert!(rows[4].saved.is_none());
}
#[test]
fn row_cache_enforces_global_budget_and_lru_across_columns() {
    let mut cache = RowCache::default();
    for column in 0..50 {
        let rows = (0..256)
            .map(|_| Row {
                name: "n".repeat(2048),
                uri: "u".repeat(2048),
                kind: "object".into(),
                namespace: "aws".into(),
            })
            .collect();
        cache.insert(Page {
            generation: 1,
            column,
            start: 0,
            rows,
        });
        assert!(cache.bytes() <= ROW_CACHE_BUDGET);
    }
    assert!(cache.row(1, 0, 0).is_none());
    assert!(cache.row(1, 49, 0).is_some());
    assert!(cache.row(2, 49, 0).is_none());
    cache.clear();
    assert_eq!(cache.bytes(), 0);
}

#[tokio::test]
async fn invalid_new_location_drops_the_old_selection_and_can_be_recovered() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    seed(root, &[("p/a", 3)]).await;
    let mut app = browser(root).await;
    app.select(0, 0, BrowseControl::default()).await.unwrap();
    let bad = b"bad database bytes";
    std::fs::write(root.join("bad.db"), bad).unwrap();
    std::fs::write(root.join("config.toml"), "[index]\nlocation='bad.db'\n").unwrap();
    assert!(app.reload(&args(), BrowseControl::default()).await.is_err());
    assert!(app.columns.is_empty());
    assert_eq!(std::fs::read(root.join("bad.db")).unwrap(), bad);
    std::fs::write(root.join("config.toml"), "[index]\nlocation='data.db'\n").unwrap();
    app.reload(&args(), BrowseControl::default()).await.unwrap();
    assert_eq!(app.columns.len(), 1);
    assert_eq!(app.columns[0].listing.summary().entries, 1);
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn top_empty_foreign_namespace_and_precancel_make_zero_store_requests() {
    use archive_sync::CancelContext;
    use private_archive_gui::reconcile::{self, Target};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let args = GuiArgs::parse_from([
        "gui",
        "--config",
        root.join("config").to_str().unwrap(),
        "--index-location",
        root.join("db").to_str().unwrap(),
        "--test-endpoint",
        &endpoint,
    ]);
    let prepared = args.prepare(paths(root)).unwrap();
    let cancel = CancelContext::new();
    cancel.cancel();
    assert_eq!(
        reconcile::execute(&args, &prepared, Target::Top, None, &(), &cancel)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Cancelled
    );
    assert!(!root.join("db").exists());
    assert_eq!(
        reconcile::execute(
            &args,
            &prepared,
            Target::Top,
            None,
            &(),
            &CancelContext::new()
        )
        .await
        .unwrap_err()
        .kind,
        ErrorKind::Input
    );
    let entry = IndexEntry {
        id: ObjectId::parse_s3("s3://bucket/key").unwrap(),
        kind: IndexEntryKind::Object,
        metadata: None,
    };
    assert_eq!(
        reconcile::execute(
            &args,
            &prepared,
            Target::Selected(Box::new(entry)),
            None,
            &(),
            &CancelContext::new()
        )
        .await
        .unwrap_err()
        .kind,
        ErrorKind::Input
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
