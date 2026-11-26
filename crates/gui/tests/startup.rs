use archive_config::{Origin, Paths};
use archive_domain::*;
use archive_index::{BrowseControl, IndexRead, IndexWrite, MetadataPatch};
use archive_infra::{SqliteIndex, index_working_path};
use clap::Parser;
use private_archive_gui::{GuiArgs, PAGE_SIZE, PreparedGui};
use std::{path::Path, time::UNIX_EPOCH};

fn prepared(root: &Path, args: &[&str]) -> PreparedGui {
    GuiArgs::try_parse_from(std::iter::once("private-archive-gui").chain(args.iter().copied()))
        .unwrap()
        .prepare(Paths {
            home: root.join("home"),
            cwd: root.into(),
        })
        .unwrap()
}

#[test]
fn shared_settings_preserve_all_origins_and_runtime_overrides_without_saving() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config = root.join("config.toml");
    let contents = "ignore = ['folder/**']\n[index]\nlocation = 'configured.db'\n[copy]\ndata-class = 'archived'\n[restore]\ntier = 'Standard'\n[logging]\nlevel = 'warn'\n";
    std::fs::write(&config, contents).unwrap();
    let file = prepared(root, &["--config", "config.toml"]);
    assert_eq!(
        file.settings.index_location.value,
        Location::Local(root.join("configured.db"))
    );
    assert_eq!(file.settings.index_location.origin, Origin::ConfigFile);
    assert_eq!(file.settings.data_class.value, DataClass::Archived);
    assert_eq!(file.settings.restore_tier.value, RestoreTier::Standard);
    assert_eq!(file.settings.ignore.value, ["folder/**"]);
    assert_eq!(file.settings.log_level.value, LogLevel::Warn);
    let runtime = prepared(
        root,
        &[
            "--config",
            "config.toml",
            "--index-location",
            "runtime.db",
            "--data-class",
            "standard",
            "--restore-tier",
            "Bulk",
            "--ignore",
            "",
            "--log-level",
            "trace",
        ],
    );
    assert_eq!(
        runtime.settings.index_location.value,
        Location::Local(root.join("runtime.db"))
    );
    assert_eq!(runtime.settings.index_location.origin, Origin::Runtime);
    assert_eq!(runtime.settings.data_class.value, DataClass::Standard);
    assert_eq!(runtime.settings.restore_tier.value, RestoreTier::Bulk);
    assert!(runtime.settings.ignore.value.is_empty());
    assert_eq!(runtime.settings.log_level.value, LogLevel::Trace);
    assert_eq!(std::fs::read_to_string(config).unwrap(), contents);
    assert!(!root.join("configured.db").exists());
    assert!(!root.join("runtime.db").exists());
}

#[test]
fn invalid_config_layer_is_rejected_even_when_runtime_would_override_it() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("config.toml"),
        "[logging]\nlevel = 'verbose'\n",
    )
    .unwrap();
    let args =
        GuiArgs::try_parse_from(["gui", "--config", "config.toml", "--log-level", "info"]).unwrap();
    let error = match args.prepare(Paths {
        home: temp.path().join("home"),
        cwd: temp.path().into(),
    }) {
        Ok(_) => panic!("invalid lower-priority layer was accepted"),
        Err(e) => e,
    };
    assert_eq!(error.exit_code, 2);
    assert!(error.message.contains("logging.level"));
}

#[test]
fn help_and_invalid_arguments_do_not_need_home_or_a_window() {
    let help = GuiArgs::try_parse_from(["gui", "--help"]).unwrap_err();
    assert_eq!(help.exit_code(), 0);
    for name in [
        "--config",
        "--index-location",
        "--data-class",
        "--restore-tier",
        "--ignore",
        "--log-level",
    ] {
        assert!(help.to_string().contains(name));
    }
    assert_eq!(
        GuiArgs::try_parse_from(["gui", "--data-class", "DeepArchive"])
            .unwrap_err()
            .exit_code(),
        2
    );
    assert_eq!(
        GuiArgs::try_parse_from(["gui", "--log-level", "verbose"])
            .unwrap_err()
            .exit_code(),
        2
    );
}

#[tokio::test]
async fn first_local_startup_is_empty_and_does_not_create_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let app = prepared(temp.path(), &[]);
    assert_eq!(app.settings.log_level.value, LogLevel::Info);
    assert_eq!(app.settings.log_level.origin, Origin::BuiltIn);
    let listing = app.open_roots(BrowseControl::default()).await.unwrap();
    assert_eq!(listing.summary().entries, 0);
    assert!(listing.page(0).await.unwrap().entries.is_empty());
    assert!(!app.settings.config_path.exists());
    assert!(temp.path().join("home/.private-archive/index.db").exists());
}

async fn seed(path: &Path, bucket: &str, key: &str) {
    let index = SqliteIndex::open(path).await.unwrap();
    let id = ObjectId::parse_s3(&format!("s3://{bucket}/{key}")).unwrap();
    let mut metadata = ObjectMetadata::file(Location::Remote(id), 17);
    metadata.queried_at = Some(UNIX_EPOCH);
    index
        .apply(MetadataPatch::full(metadata).unwrap())
        .await
        .unwrap();
    index.close().await.unwrap();
}

#[tokio::test]
async fn s3_setting_uses_only_the_same_local_working_copy_and_regular_db_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let app = prepared(
        temp.path(),
        &["--index-location", "s3://index-bucket/archive.db"],
    );
    let path = index_working_path(&app.paths.home, &app.settings.index_location.value, "aws");
    assert_eq!(
        app.open_roots(BrowseControl::default())
            .await
            .unwrap()
            .summary()
            .entries,
        0
    );
    let local = temp.path().join("source.db");
    seed(&local, "bucket", ".DS_Store").await;
    std::fs::copy(&local, &path).unwrap();
    let listing = app.open_roots(BrowseControl::default()).await.unwrap();
    assert_eq!(listing.summary().entries, 1);
    assert_eq!(
        listing.page(0).await.unwrap().entries[0].uri(),
        "s3://bucket"
    );
    assert_eq!(app.index_label(), "s3://index-bucket/archive.db");
    assert!(!app.settings.config_path.exists());
}

#[tokio::test]
async fn all_ignored_is_a_natural_empty_view_and_preserves_records() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.db");
    seed(&path, "bucket", ".git/config").await;
    let app = prepared(
        temp.path(),
        &["--index-location", "index.db", "--ignore", "*"],
    );
    let listing = app.open_roots(BrowseControl::default()).await.unwrap();
    assert_eq!(listing.summary().entries, 0);
    assert!(listing.summary().ignored > 0);
    let app = prepared(temp.path(), &["--index-location", "index.db"]);
    assert_eq!(
        app.open_roots(BrowseControl::default())
            .await
            .unwrap()
            .summary()
            .entries,
        1
    );
    let db = SqliteIndex::open(path).await.unwrap();
    assert!(
        db.get(&ObjectId::parse_s3("s3://bucket/.git/config").unwrap())
            .await
            .unwrap()
            .is_some()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn corrupt_index_is_diagnosed_and_never_rebuilt_or_replaced() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.db");
    let original = b"broken index; keep these bytes";
    std::fs::write(&path, original).unwrap();
    let app = prepared(temp.path(), &["--index-location", "index.db"]);
    match app.open_roots(BrowseControl::default()).await {
        Ok(_) => panic!("corrupt index accepted"),
        Err(e) => assert!(e.to_string().contains("index-")),
    }
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[tokio::test]
async fn root_pages_are_bounded_and_query_can_be_cancelled() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.db");
    let index = SqliteIndex::open(&path).await.unwrap();
    for n in 0..=PAGE_SIZE {
        let id = ObjectId::parse_s3(&format!("s3://bucket-{n:04}/item")).unwrap();
        let mut metadata = ObjectMetadata::file(Location::Remote(id), 1);
        metadata.queried_at = Some(UNIX_EPOCH);
        index
            .apply(MetadataPatch::full(metadata).unwrap())
            .await
            .unwrap();
    }
    index.close().await.unwrap();
    let app = prepared(temp.path(), &["--index-location", "index.db"]);
    let listing = app.open_roots(BrowseControl::default()).await.unwrap();
    let first = listing.page(0).await.unwrap();
    assert_eq!(first.entries.len(), PAGE_SIZE);
    let second = listing.page(first.next.unwrap()).await.unwrap();
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.entries[0].name(), "bucket-0256");
    assert!(second.next.is_none());
    let control = BrowseControl::default();
    control.cancel();
    assert!(app.open_roots(control).await.is_err());
}
