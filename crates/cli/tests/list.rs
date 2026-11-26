use archive_domain::*;
use archive_index::{IndexRead, IndexWrite, MetadataPatch};
use archive_infra::{SqliteIndex, index_working_path};
use serde_json::Value;
use std::{
    net::TcpListener,
    path::Path,
    process::{Command, Output},
    time::{Duration, UNIX_EPOCH},
};

fn id(key: &str) -> ObjectId {
    ObjectId::parse_s3(&format!("s3://bucket/{key}")).unwrap()
}
fn meta(key: &str, size: u64) -> ObjectMetadata {
    let mut meta = ObjectMetadata::file(Location::Remote(id(key)), size);
    meta.queried_at = Some(UNIX_EPOCH + Duration::new(100, 123));
    meta
}
async fn seed(path: &Path, rows: Vec<ObjectMetadata>) {
    let index = SqliteIndex::open(path).await.unwrap();
    for m in rows {
        index.apply(MetadataPatch::full(m).unwrap()).await.unwrap();
    }
    index.close().await.unwrap();
}
fn run(root: &Path, location: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_private-archive"))
        .args(["--index-location", location])
        .args(args)
        .current_dir(root)
        .env("HOME", root.join("home"))
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("AWS_REGION", "us-east-1")
        .output()
        .unwrap()
}
fn array(out: &Output) -> Vec<Value> {
    serde_json::from_slice::<Vec<Value>>(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "not an array: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[tokio::test]
async fn normal_detail_json_and_exact_directory_root_views() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let mut known = meta("foo", 100_000_000_001);
    known.original_mtime = Some(UNIX_EPOCH + Duration::new(2, 789));
    known.original_creation_time = Some(UNIX_EPOCH - Duration::new(1, 1));
    known.unix_mode = Some(0o755);
    known.data_class = Some(StoredClass::Archived);
    known.restore_state = RestoreState::Restored {
        until: Some(UNIX_EPOCH + Duration::from_secs(1)),
    };
    seed(&path, vec![known, meta("foo/child", 3)]).await;
    let location = path.to_str().unwrap();
    let out = run(dir.path(), location, &["ls", "s3://bucket/foo"]);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"s3://bucket/foo\n");
    assert!(out.stderr.is_empty());
    let out = run(
        dir.path(),
        location,
        &["ls", "s3://bucket/foo", "--detail", "--json"],
    );
    assert!(out.status.success());
    let rows = array(&out);
    let row = &rows[0];
    assert_eq!(row.as_object().unwrap().len(), 10);
    assert_eq!(row["size_bytes"], 100_000_000_001u64);
    assert_eq!(row["unix_mode"], "0755");
    assert_eq!(row["original_mtime"], "1970-01-01T00:00:02.000000789Z");
    assert_eq!(
        row["original_creation_time"],
        "1969-12-31T23:59:58.999999999Z"
    );
    assert_eq!(row["queried_at"], "1970-01-01T00:01:40.000000123Z");
    assert_eq!(row["restore_state"], "restored");
    assert_eq!(row["restore_until"], "1970-01-01T00:00:01Z");
    let detail = run(dir.path(), location, &["ls", "s3://bucket/foo", "--detail"]);
    assert!(detail.status.success());
    let text = String::from_utf8(detail.stdout).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert!(text.contains("復元済み"));
    assert_eq!(text.lines().last().unwrap().split('\t').count(), 10);
    let out = run(dir.path(), location, &["ls", "--json"]);
    let rows = array(&out);
    assert_eq!(rows[0]["uri"], "s3://bucket");
    assert_eq!(rows[0]["kind"], "root");
    for key in [
        "size_bytes",
        "data_class",
        "restore_state",
        "restore_until",
        "original_mtime",
        "original_creation_time",
        "unix_mode",
        "queried_at",
    ] {
        assert!(rows[0][key].is_null());
    }
    let db = SqliteIndex::open(&path).await.unwrap();
    db.remove(&id("foo")).await.unwrap();
    db.close().await.unwrap();
    let out = run(dir.path(), location, &["ls", "s3://bucket/foo", "--json"]);
    let rows = array(&out);
    assert_eq!(rows[0]["uri"], "s3://bucket/foo/");
    assert_eq!(rows[0]["kind"], "directory");
    assert!(rows[0]["size_bytes"].is_null());
    assert!(rows[0]["queried_at"].is_null());
}

#[tokio::test]
async fn recursive_depth_opaque_keys_hidden_names_and_multiple_sort_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    seed(
        &path,
        vec![
            meta("p/x/same", 20),
            meta("p/y/same", 5),
            meta("p/Z", 20),
            meta("p/.DS_Store", 8),
            meta("p/.git/config", 4),
            meta("p/a%_雪", 6),
            meta("p/deep/a/b", 1),
            meta("prefix/outside", 0),
        ],
    )
    .await;
    let location = path.to_str().unwrap();
    let out = run(
        dir.path(),
        location,
        &[
            "ls",
            "s3://bucket/p",
            "--recursive",
            "--sort",
            "size-asc,name-desc",
            "--json",
        ],
    );
    assert!(out.status.success());
    let rows = array(&out);
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[0]["uri"], "s3://bucket/p/deep/a/b");
    assert_eq!(rows[6]["uri"], "s3://bucket/p/Z");
    assert!(rows.iter().any(|r| r["uri"] == "s3://bucket/p/.DS_Store"));
    assert!(rows.iter().any(|r| r["uri"] == "s3://bucket/p/.git/config"));
    assert!(rows.iter().any(|r| r["uri"] == "s3://bucket/p/a%_雪"));
    let out = run(
        dir.path(),
        location,
        &[
            "ls",
            "s3://bucket/p",
            "--recursive",
            "--recursive-depth",
            "1",
            "--json",
        ],
    );
    let rows = array(&out);
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter()
            .all(|r| !r["uri"].as_str().unwrap().contains("/same"))
    );
    let out = run(
        dir.path(),
        location,
        &[
            "ls",
            "s3://bucket/p",
            "--recursive",
            "--sort",
            "name-desc,size-asc",
            "--json",
        ],
    );
    let rows = array(&out);
    let same: Vec<_> = rows
        .iter()
        .filter(|r| r["uri"].as_str().unwrap().ends_with("/same"))
        .map(|r| r["size_bytes"].as_u64().unwrap())
        .collect();
    assert_eq!(same, [5, 20]);
    let out = run(
        dir.path(),
        location,
        &[
            "ls",
            "s3://bucket/p",
            "--recursive",
            "--ignore",
            ".git/,.DS_Store",
            "--json",
            "--log-level",
            "debug",
        ],
    );
    assert!(out.status.success());
    assert_eq!(array(&out).len(), 5);
    let logs = String::from_utf8_lossy(&out.stderr);
    assert!(logs.contains("2 observed exclusions"));
    assert!(logs.contains(".git/config"));
    assert!(logs.contains(".DS_Store"));
}

#[tokio::test]
async fn no_targets_empty_initial_copy_all_ignored_and_corruption_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let location = path.to_str().unwrap();
    let out = run(dir.path(), location, &["ls", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"[]\n");
    assert!(path.exists());
    assert!(
        !dir.path()
            .join("home/.private-archive/config.toml")
            .exists()
    );
    seed(&path, vec![meta("foo", 1)]).await;
    let out = run(
        dir.path(),
        location,
        &[
            "ls",
            "s3://bucket/foo",
            "--ignore",
            "foo",
            "--log-level",
            "error",
            "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(array(&out).is_empty());
    let logs = String::from_utf8_lossy(&out.stderr);
    assert!(logs.contains("no targets"));
    assert!(!logs.contains("ignored explicit"));
    let out = run(
        dir.path(),
        location,
        &["ls", "s3://bucket/foo", "--ignore", "foo", "--json"],
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("ignored explicit target"));
    let db = SqliteIndex::open(&path).await.unwrap();
    assert!(db.get(&id("foo")).await.unwrap().is_some());
    db.close().await.unwrap();
    let corrupt = dir.path().join("bad.db");
    std::fs::write(&corrupt, b"not a database; preserve me").unwrap();
    let original = std::fs::read(&corrupt).unwrap();
    let out = run(dir.path(), corrupt.to_str().unwrap(), &["ls", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(array(&out).is_empty());
    assert_eq!(std::fs::read(&corrupt).unwrap(), original);
}

#[tokio::test]
async fn first_s3_copy_and_cached_s3_views_make_zero_network_requests() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let uri = "s3://index-bucket/index.db";
    let location = Location::Remote(ObjectId::parse_s3(uri).unwrap());
    let path = index_working_path(&home, &location, "aws");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let call = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_private-archive"))
            .args(["--index-location", uri])
            .args(args)
            .current_dir(dir.path())
            .env("HOME", &home)
            .env("AWS_REGION", "us-east-1")
            .env("AWS_ENDPOINT_URL", &endpoint)
            .env("AWS_ENDPOINT_URL_S3", &endpoint)
            .env("AWS_ACCESS_KEY_ID", "test-list")
            .env("AWS_SECRET_ACCESS_KEY", "test-list")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .output()
            .unwrap()
    };
    let out = call(&["ls", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(array(&out).is_empty());
    assert!(path.exists());
    seed(&path, vec![meta("foo/child", 1)]).await;
    for args in [
        vec!["ls", "--json"],
        vec!["ls", "s3://bucket/foo", "--detail"],
        vec!["ls", "s3://bucket/foo", "--recursive", "--json"],
        vec![
            "ls",
            "s3://bucket/foo",
            "--recursive",
            "--sort",
            "size-desc,name-asc",
            "--json",
        ],
    ] {
        let out = call(&args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(!home.join(".private-archive/config.toml").exists());
}

#[tokio::test]
async fn failure_during_json_formatting_closes_the_valid_partial_array() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let mut bad = meta("B-bad", 2);
    bad.original_mtime = Some(UNIX_EPOCH + Duration::from_secs(253_402_300_800));
    seed(&path, vec![meta("A-good", 1), bad]).await;
    let out = run(
        dir.path(),
        path.to_str().unwrap(),
        &["ls", "s3://bucket/", "--recursive", "--json"],
    );
    assert_eq!(out.status.code(), Some(1));
    let rows = array(&out);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["uri"], "s3://bucket/A-good");
    assert!(String::from_utf8_lossy(&out.stderr).contains("ls-time"));
}

#[test]
fn list_help_and_invalid_arguments_are_handled_before_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_private-archive"))
        .args(["ls", "--help"])
        .env_remove("HOME")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
    for args in [
        vec!["ls", "local-path"],
        vec!["ls", "s3://bucket/p", "--recursive-depth", "1"],
        vec![
            "ls",
            "s3://bucket/p",
            "--recursive",
            "--recursive-depth",
            "0",
        ],
        vec!["ls", "--sort", "bogus"],
        vec!["ls", "--sort", ""],
        vec!["ls", "--sort", "size-asc,size-desc"],
    ] {
        let out = run(dir.path(), "index.db", &args);
        assert_eq!(out.status.code(), Some(2));
    }
    assert!(!dir.path().join("index.db").exists());
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn fixture_namespaces_use_their_own_cached_records_without_connecting() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let namespace = archive_infra::S3Store::fixture_namespace(&endpoint).unwrap();
    let local = dir.path().join("fixture.db");
    for location in [local.to_str().unwrap(), "s3://index-bucket/fixture.db"] {
        let configured = Location::parse(std::ffi::OsStr::new(location), dir.path()).unwrap();
        let path = index_working_path(&home, &configured, &namespace);
        let mut m = meta("p/file", 1);
        let Location::Remote(id) = &mut m.location else {
            unreachable!()
        };
        id.namespace = namespace.clone();
        seed(&path, vec![m]).await;
        let out = Command::new(env!("CARGO_BIN_EXE_private-archive"))
            .args([
                "--test-endpoint",
                &endpoint,
                "--config",
                "fixture-config.toml",
                "--index-location",
                location,
                "ls",
                "s3://bucket/p",
                "--recursive",
                "--json",
            ])
            .current_dir(dir.path())
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(array(&out)[0]["uri"], "s3://bucket/p/file");
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let out = Command::new(env!("CARGO_BIN_EXE_private-archive"))
        .args([
            "--test-endpoint",
            "http://203.0.113.1:8333",
            "--config",
            "fixture-config.toml",
            "--index-location",
            "untouched.db",
            "ls",
        ])
        .current_dir(dir.path())
        .env("HOME", &home)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!dir.path().join("untouched.db").exists());
}
