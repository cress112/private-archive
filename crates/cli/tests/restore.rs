use std::process::Command;
#[test]
fn restore_help_and_invalid_grammar_require_no_home_configuration_or_store() {
    let binary = env!("CARGO_BIN_EXE_private-archive");
    let help = Command::new(binary)
        .args(["restore", "--help"])
        .env_remove("HOME")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--restore-duration"));
    for args in [
        vec!["restore"],
        vec!["restore", "--restore-duration", "0d", "s3://bucket/a"],
        vec!["restore", "--restore-duration", "32h", "s3://bucket/a"],
        vec![
            "restore",
            "--restore-duration",
            "7d",
            "s3://bucket/p",
            "--recursive-depth",
            "1",
        ],
        vec![
            "restore",
            "--restore-duration",
            "7d",
            "s3://bucket/p",
            "--log-level",
            "debug",
        ],
    ] {
        let result = Command::new(binary)
            .args(args)
            .env_remove("HOME")
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&result.stderr).contains("HOME is not set"));
    }
}
