use std::{
    fs,
    net::TcpListener,
    process::{Command, Output},
};

fn run(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_private-archive"))
        .args(args)
        .current_dir(root)
        .env("HOME", root.join("home"))
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .output()
        .unwrap()
}

#[test]
fn configuration_lifecycle_and_effective_origins() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let output = run(root, &["cfg"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 5);
    assert!(stdout.contains("logging.level = \"info\" (built-in)"));
    assert!(!root.join("home").exists());
    let missing = run(root, &["cfg", "set", "logging.level", "warn"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("cfg init"));
    assert!(!root.join("home").exists());
    assert!(run(root, &["cfg", "init"]).status.success());
    assert_eq!(run(root, &["cfg", "init"]).status.code(), Some(1));
    for (key, value) in [
        ("logging.level", "warn"),
        ("copy.data-class", "archived"),
        ("restore.tier", "Standard"),
        ("ignore", r".DS_Store,a\,b"),
        ("index.location", "s3://bucket/index.db"),
    ] {
        let out = run(root, &["cfg", "set", key, value]);
        assert!(
            out.status.success(),
            "{key}: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = run(root, &["--log-level", "trace", "cfg"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("logging.level = \"trace\" (runtime)"));
    let file = root.join("home/.private-archive/config.toml");
    assert!(fs::read_to_string(&file).unwrap().contains("\"warn\""));
    assert_eq!(
        run(root, &["cfg", "get", "logging.level"]).stdout,
        b"\"warn\"\n"
    );
    assert!(
        run(root, &["cfg", "unset", "logging.level"])
            .status
            .success()
    );
    assert_eq!(
        run(root, &["cfg", "get", "logging.level"]).stdout,
        b"\"info\"\n"
    );
    for args in [
        vec!["cfg", "set", "restore.duration", "7d"],
        vec!["cfg", "set", "logging.level", "verbose"],
        vec!["--log-level", "invalid", "cfg"],
    ] {
        assert_eq!(run(root, &args).status.code(), Some(2));
    }
    assert!(!root.join("home/.private-archive/index.db").exists());
    assert!(!root.join("home/.private-archive/cache").exists());
}

#[test]
fn cfg_help_needs_no_home_and_config_edits_never_contact_a_store() {
    for args in [vec!["cfg", "--help"], vec!["cfg", "set", "--help"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_private-archive"))
            .args(args)
            .env_remove("HOME")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
    }
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let out = Command::new(env!("CARGO_BIN_EXE_private-archive"))
        .args(["--index-location", "s3://bucket/index.db", "cfg"])
        .env("HOME", temp.path().join("home"))
        .env("AWS_ENDPOINT_URL_S3", endpoint)
        .env("AWS_ACCESS_KEY_ID", "test-config")
        .env("AWS_SECRET_ACCESS_KEY", "test-config")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!temp.path().join("home").exists());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn locked_config_is_not_overwritten_by_another_process() {
    let temp = tempfile::tempdir().unwrap();
    assert!(run(temp.path(), &["cfg", "init"]).status.success());
    let path = temp.path().join("home/.private-archive/config.toml");
    let original = fs::read(&path).unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("toml.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let out = run(temp.path(), &["cfg", "set", "logging.level", "error"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("lock"));
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn config_io_failure_is_processing_failure_and_invalid_content_is_input_failure() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("not-a-file")).unwrap();
    let out = run(temp.path(), &["--config", "not-a-file", "cfg"]);
    assert_eq!(out.status.code(), Some(1));
    fs::write(temp.path().join("bad.toml"), "[logging]\nlevel='broken'\n").unwrap();
    let out = run(
        temp.path(),
        &["--config", "bad.toml", "--log-level", "trace", "cfg"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(!temp.path().join("home").exists());
}
