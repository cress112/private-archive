use std::process::Command;

#[test]
fn reconcile_help_and_invalid_scope_are_resolved_without_store_access() {
    let out = Command::new(env!("CARGO_BIN_EXE_private-archive"))
        .args(["reconcile", "--help"])
        .env_remove("HOME")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["reconcile"],
        vec!["reconcile", "local-path"],
        vec!["reconcile", "s3://bucket/"],
        vec!["reconcile", "s3://bucket/p", "--recursive-depth", "1"],
        vec![
            "reconcile",
            "s3://bucket/p",
            "--recursive",
            "--recursive-depth",
            "0",
        ],
        vec!["reconcile", "s3://bucket/p", "--json"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_private-archive"))
            .args(args)
            .env("HOME", dir.path().join("home"))
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
    }
    assert!(!dir.path().join("home").exists());
}
