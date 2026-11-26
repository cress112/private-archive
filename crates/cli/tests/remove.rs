use std::process::Command;
#[test]
fn remove_help_and_invalid_arguments_need_no_home_or_configuration() {
    let binary = env!("CARGO_BIN_EXE_private-archive");
    let help = Command::new(binary)
        .args(["rm", "--help"])
        .env_remove("HOME")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--auto-approve"));
    for args in [
        vec!["rm"],
        vec!["rm", "s3://bucket/p", "--recursive-depth", "1"],
        vec!["rm", "s3://bucket/p", "-r", "--recursive-depth", "0"],
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
