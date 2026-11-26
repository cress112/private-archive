use archive_domain::*;
use std::{ffi::OsStr, path::Path};

#[test]
fn local_names_and_remote_objects_are_validated_without_url_normalization() {
    let cwd = Path::new("/work");
    let local = Location::parse(OsStr::new("/work/colon://file"), cwd).unwrap();
    assert_eq!(local, Location::Local("/work/colon://file".into()));
    assert!(Location::parse(OsStr::new("https://bucket/key"), cwd).is_err());
    assert!(Location::parse(OsStr::new("s3://bucket:123/key"), cwd).is_err());
    assert!(Location::parse(OsStr::new("s3:///key"), cwd).is_err());
    assert!(Location::parse(OsStr::new("name\0suffix"), cwd).is_err());
    assert!(
        ObjectId::parse_s3("s3://bucket")
            .unwrap()
            .require_object()
            .is_err()
    );
    let object = ObjectId::parse_s3("s3://bucket/a//./../%20?#")
        .unwrap()
        .require_object()
        .unwrap();
    assert_eq!(object.id().key, "a//./../%20?#");
}
