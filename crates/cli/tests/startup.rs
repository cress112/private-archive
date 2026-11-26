use std::process::Command;

#[test]
fn help_and_version_need_no_configuration_and_write_to_stdout() {
    for arg in ["--help", "--version", "cp --help"] {
        let output = Command::new(env!("CARGO_BIN_EXE_private-archive"))
            .args(arg.split_whitespace())
            .env_remove("HOME")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn startup_validates_config_and_missing_input_without_remote_access() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_private-archive"))
            .args(args)
            .current_dir(temp.path())
            .env("HOME", &home)
            .output()
            .unwrap()
    };
    let output = run(&["cp", "file", "s3://bucket/object"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("stat"));
    assert!(!home.exists());
    std::fs::write(temp.path().join("bad.toml"), "[copy]\ndata-class='invalid'").unwrap();
    let output = run(&["--config", "bad.toml", "cp", "file", "s3://bucket/object"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("data-class"));
    assert_eq!(run(&["cp", "a", "b"]).status.code(), Some(2));
    assert_eq!(
        run(&["cp", "file", "s3://bucket/object", "--recursive-depth", "1"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        run(&[
            "cp",
            "file",
            "s3://bucket/object",
            "--recursive",
            "--recursive-depth",
            "0"
        ])
        .status
        .code(),
        Some(2)
    );
}
