use archive_config::{Origin, Overrides, Paths, load};
use archive_domain::{DataClass, Location, RestoreTier};

fn fixture() -> (tempfile::TempDir, Paths) {
    let temp = tempfile::tempdir().unwrap();
    let paths = Paths {
        home: temp.path().join("home"),
        cwd: temp.path().join("work"),
    };
    std::fs::create_dir_all(&paths.cwd).unwrap();
    (temp, paths)
}

#[test]
fn missing_config_has_defaults_without_creating_anything() {
    let (_temp, paths) = fixture();
    let settings = load(&paths, None, Overrides::default()).unwrap();
    assert_eq!(settings.data_class.value, DataClass::Standard);
    assert_eq!(settings.data_class.origin, Origin::BuiltIn);
    assert_eq!(settings.restore_tier.value, RestoreTier::Bulk);
    assert!(settings.ignore.value.is_empty());
    assert_eq!(
        settings.index_location.value,
        Location::Local(paths.home.join(".private-archive/index.db"))
    );
    assert!(!paths.home.exists());
}

#[test]
fn resolves_each_layer_relative_to_its_own_base_and_tracks_origin() {
    let (_temp, paths) = fixture();
    let config_dir = paths.cwd.join("settings");
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::write(config_dir.join("custom.toml"), "ignore = ['*.tmp', 'a,b']\n[index]\nlocation='cache/index.db'\n[copy]\ndata-class='archived'\n[restore]\ntier='Standard'\n").unwrap();
    let settings = load(
        &paths,
        Some(std::path::Path::new("settings/custom.toml")),
        Overrides::default(),
    )
    .unwrap();
    assert_eq!(
        settings.index_location.value,
        Location::Local(config_dir.join("cache/index.db"))
    );
    assert_eq!(settings.data_class.value, DataClass::Archived);
    assert_eq!(settings.restore_tier.origin, Origin::ConfigFile);
    let settings = load(
        &paths,
        Some(std::path::Path::new("settings/custom.toml")),
        Overrides {
            index_location: Some("runtime.db".into()),
            data_class: Some("standard".into()),
            restore_tier: Some("Bulk".into()),
            ignore: Some(vec![]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        settings.index_location.value,
        Location::Local(paths.cwd.join("runtime.db"))
    );
    assert_eq!(settings.data_class.value, DataClass::Standard);
    assert_eq!(settings.data_class.origin, Origin::Runtime);
    assert_eq!(settings.ignore.origin, Origin::Runtime);
    assert!(settings.ignore.value.is_empty());
}

#[test]
fn expands_home_and_preserves_remote_keys() {
    let (_temp, paths) = fixture();
    let settings = load(
        &paths,
        None,
        Overrides {
            index_location: Some("~/custom.db".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        settings.index_location.value,
        Location::Local(paths.home.join("custom.db"))
    );
    let settings = load(
        &paths,
        None,
        Overrides {
            index_location: Some("s3://bucket/a//../%20?#.db".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let Location::Remote(id) = settings.index_location.value else {
        panic!()
    };
    assert_eq!(id.key, "a//../%20?#.db");
}

#[test]
fn rejects_invalid_configuration_instead_of_silently_using_defaults() {
    let (_temp, paths) = fixture();
    let path = paths.cwd.join("bad.toml");
    for text in [
        "[",
        "typo = true",
        "[restore]\nduration='7d'",
        "[copy]\ndata-class='glacier'",
        "[index]\nlocation='s3://bucket/'",
        "ignore=[\"bad\\nline\"]",
        "ignore=1",
        "ignore=['[z-a]']",
    ] {
        std::fs::write(&path, text).unwrap();
        assert!(
            load(&paths, Some(&path), Overrides::default()).is_err(),
            "accepted {text}"
        );
    }
    assert!(load(&paths, Some(&paths.cwd), Overrides::default()).is_err());
    std::fs::write(&path, "[copy]\ndata-class='invalid'").unwrap();
    assert!(
        load(
            &paths,
            Some(&path),
            Overrides {
                data_class: Some("standard".into()),
                ..Default::default()
            }
        )
        .is_err()
    );
}
