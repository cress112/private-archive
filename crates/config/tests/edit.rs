use archive_config::*;
use archive_domain::{DataClass, Location, LogLevel};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

fn fixture() -> (tempfile::TempDir, Paths) {
    let temp = tempfile::tempdir().unwrap();
    let paths = Paths {
        home: temp.path().join("home"),
        cwd: temp.path().to_path_buf(),
    };
    (temp, paths)
}

#[test]
fn explicit_init_is_private_and_never_overwrites() {
    let (_temp, paths) = fixture();
    let path = init(&paths, None).unwrap();
    let original = fs::read(&path).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(init(&paths, None).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    let settings = load(&paths, None, Overrides::default()).unwrap();
    for key in ConfigKey::ALL {
        assert_eq!(settings.setting(key).1, Origin::BuiltIn);
    }
    assert!(!paths.home.join(".private-archive/index.db").exists());
}

#[test]
fn edits_round_trip_all_keys_preserving_unedited_text_and_runtime_precedence() {
    let (_temp, paths) = fixture();
    let path = paths.cwd.join("config.toml");
    fs::write(&path, "# header\nignore = ['*.tmp'] # keep list\n\n[copy]\ndata-class = 'standard' # class note\n").unwrap();
    let changes = [
        Change::Set(ConfigValue::IndexLocation("cache/index.db".into())),
        Change::Set(ConfigValue::DataClass(DataClass::Archived)),
        Change::Set(ConfigValue::parse(ConfigKey::RestoreTier, "Standard").unwrap()),
        Change::Set(ConfigValue::parse(ConfigKey::Ignore, r"*.tmp,a\,b,\!literal").unwrap()),
        Change::Set(ConfigValue::LogLevel(LogLevel::Warn)),
    ];
    let settings = apply_changes(&paths, Some(&path), &changes).unwrap();
    assert_eq!(settings.data_class.value, DataClass::Archived);
    assert_eq!(settings.log_level.value, LogLevel::Warn);
    assert_eq!(settings.ignore.value, ["*.tmp", "a,b", r"\!literal"]);
    assert_eq!(
        settings.index_location.value,
        Location::Local(paths.cwd.join("cache/index.db"))
    );
    let text = fs::read_to_string(&path).unwrap();
    for comment in ["# header", "# keep list", "# class note"] {
        assert!(text.contains(comment), "lost {comment:?}: {text}");
    }
    let effective = load(
        &paths,
        Some(&path),
        Overrides {
            log_level: Some("trace".into()),
            ignore: Some(vec![]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(effective.log_level.value, LogLevel::Trace);
    assert_eq!(effective.log_level.origin, Origin::Runtime);
    assert!(effective.ignore.value.is_empty());
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
    let resets: Vec<_> = ConfigKey::ALL.into_iter().map(Change::Unset).collect();
    let settings = apply_changes(&paths, Some(&path), &resets).unwrap();
    for key in ConfigKey::ALL {
        assert_eq!(settings.setting(key).1, Origin::BuiltIn);
    }
    assert_eq!(settings.log_level.value, LogLevel::Info);
    assert!(!paths.cwd.join("cache").exists());
}

#[test]
fn invalid_or_missing_settings_are_not_repaired_or_created_by_set() {
    let (_temp, paths) = fixture();
    assert!(init(&paths, Some(Path::new("/"))).is_err());
    let path = paths.cwd.join("config.toml");
    let good = Change::Set(ConfigValue::LogLevel(LogLevel::Debug));
    let error = apply_changes(&paths, Some(&path), std::slice::from_ref(&good)).unwrap_err();
    assert!(error.to_string().contains("cfg init"));
    assert!(!path.exists());
    assert!(!path.with_extension("toml.lock").exists());
    assert!("restore.duration".parse::<ConfigKey>().is_err());
    for value in ["WARN", "verbose", ""] {
        assert!(ConfigValue::parse(ConfigKey::LogLevel, value).is_err());
    }
    fs::write(&path, "# keep\n[logging]\nlevel='warn'\n").unwrap();
    let original = fs::read(&path).unwrap();
    for change in [
        Change::Set(ConfigValue::IndexLocation("s3://bucket/".into())),
        Change::Set(ConfigValue::Ignore(vec!["[z-a]".into()])),
    ] {
        assert!(apply_changes(&paths, Some(&path), &[change]).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }
    for broken in ["[", "[copy]\ndata-class='bad'", "typo=true"] {
        fs::write(&path, broken).unwrap();
        assert!(apply_changes(&paths, Some(&path), std::slice::from_ref(&good)).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);
    }
}

#[test]
fn active_lock_fails_without_writing_and_stale_lock_file_is_harmless() {
    let (_temp, paths) = fixture();
    let path = init(&paths, Some(Path::new("config.toml"))).unwrap();
    let original = fs::read(&path).unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("toml.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let change = Change::Set(ConfigValue::LogLevel(LogLevel::Error));
    assert!(matches!(
        apply_changes(&paths, Some(&path), std::slice::from_ref(&change)),
        Err(ConfigError::Lock { .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), original);
    drop(lock);
    let settings = apply_changes(&paths, Some(&path), &[change]).unwrap();
    assert_eq!(settings.log_level.value, LogLevel::Error);
}

#[test]
fn symlink_edit_preserves_link_and_does_not_reset_unrelated_fields() {
    let (_temp, paths) = fixture();
    let target = paths.cwd.join("real.toml");
    fs::write(&target, "ignore=['*.tmp']\n").unwrap();
    let link = paths.cwd.join("config.toml");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    apply_changes(
        &paths,
        Some(&link),
        &[Change::Set(ConfigValue::LogLevel(LogLevel::Debug))],
    )
    .unwrap();
    assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
    assert_eq!(
        load(&paths, Some(&link), Overrides::default())
            .unwrap()
            .ignore
            .value,
        ["*.tmp"]
    );
    assert!(init(&paths, Some(&link)).is_err());
}

#[test]
fn log_levels_validate_all_layers_even_if_overridden() {
    let (_temp, paths) = fixture();
    let path = paths.cwd.join("config.toml");
    for level in ["error", "warn", "info", "debug", "trace"] {
        fs::write(&path, format!("[logging]\nlevel='{level}'\n")).unwrap();
        let settings = load(&paths, Some(&path), Overrides::default()).unwrap();
        assert_eq!(settings.log_level.value.to_string(), level);
        assert_eq!(settings.log_level.origin, Origin::ConfigFile);
    }
    fs::write(&path, "[logging]\nlevel='broken'\n").unwrap();
    assert!(
        load(
            &paths,
            Some(&path),
            Overrides {
                log_level: Some("debug".into()),
                ..Default::default()
            }
        )
        .is_err()
    );
}
