use archive_domain::{EntryKind, Location};
use archive_sync::*;
use std::{ffi::OsStr, num::NonZeroU32, path::Path};

fn location(value: &str) -> Location {
    Location::parse(OsStr::new(value), Path::new("/work")).unwrap()
}
fn selection(value: &str, recursive: bool, depth: Option<u32>) -> Selection {
    Selection::new(
        location(value),
        recursive,
        depth.map(|n| NonZeroU32::new(n).unwrap()),
        Path::new("/work"),
    )
    .unwrap()
}
fn layout(
    source: &str,
    dest: &str,
    recursive: bool,
    patterns: &[&str],
    is_dir: bool,
) -> CopyLayout {
    CopyLayout::new(
        selection(source, recursive, None),
        location(dest),
        is_dir,
        Path::new("/work"),
        &patterns.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
    )
    .unwrap()
}
fn destination(candidate: Candidate) -> String {
    match candidate {
        Candidate::Copy(
            CopyRoute::Upload {
                remote_destination, ..
            }
            | CopyRoute::Remote {
                remote_destination, ..
            },
        ) => remote_destination.uri(),
        Candidate::Copy(CopyRoute::Download {
            local_destination, ..
        }) => local_destination.display().to_string(),
        _ => panic!("expected a copy"),
    }
}

#[test]
fn opaque_s3_keys_and_exact_vs_descendants() {
    let Location::Remote(id) = location("s3://bucket/a//./../%20?#") else {
        panic!()
    };
    assert_eq!(id.key, "a//./../%20?#");
    for root in ["s3://bucket/foo", "s3://bucket/foo/"] {
        let s = selection(root, true, Some(1));
        assert!(s.contains(&location("s3://bucket/foo/a")));
        for outside in [
            "s3://bucket/foo",
            "s3://bucket/foobar/a",
            "s3://bucket/foo/sub/a",
            "s3://other/foo/a",
        ] {
            assert!(!s.contains(&location(outside)), "included {outside}");
        }
        assert!(selection(root, true, Some(2)).contains(&location("s3://bucket/foo/sub/a")));
    }
    assert!(selection("s3://bucket/foo", false, None).contains(&location("s3://bucket/foo")));
    assert!(
        Selection::new(
            location("source"),
            false,
            NonZeroU32::new(1),
            Path::new("/work")
        )
        .is_err()
    );
}

#[test]
fn aws_compatible_single_file_destinations() {
    for (src, dst, directory, expected) in [
        ("file", "s3://bucket/name", false, "s3://bucket/name"),
        (
            "file",
            "s3://bucket/prefix/",
            false,
            "s3://bucket/prefix/file",
        ),
        ("file", "s3://bucket", false, "s3://bucket/file"),
        (
            "s3://bucket/path/",
            "s3://other/exact",
            false,
            "s3://other/exact",
        ),
        ("s3://bucket/path/file", "out", false, "/work/out"),
        ("s3://bucket/path/file", "out", true, "/work/out/file"),
        ("s3://bucket/path/file", "new/", false, "/work/new/file"),
        (
            "s3://bucket/path/file",
            "s3://other/new/",
            false,
            "s3://other/new/file",
        ),
    ] {
        assert_eq!(
            destination(
                layout(src, dst, false, &[], directory)
                    .resolve(&location(src), EntryKind::File)
                    .unwrap()
            ),
            expected
        );
    }
}

#[test]
fn recursive_copy_keeps_contents_without_adding_source_directory_name() {
    for source in ["photos", "photos/"] {
        assert_eq!(
            destination(
                layout(source, "s3://bucket/backup", true, &[], false)
                    .resolve(&location("photos/sub/a"), EntryKind::File)
                    .unwrap()
            ),
            "s3://bucket/backup/sub/a"
        );
    }
    for source in ["s3://bucket/photos", "s3://bucket/photos/"] {
        assert_eq!(
            destination(
                layout(source, "out", true, &[], false)
                    .resolve(&location("s3://bucket/photos/sub/a"), EntryKind::File)
                    .unwrap()
            ),
            "/work/out/sub/a"
        );
    }
}

#[test]
fn ignore_uses_both_sides_and_runs_before_special_file_rejection() {
    let l = layout("source", "s3://bucket/target", false, &["/source"], false);
    assert!(matches!(
        l.resolve(&location("source"), EntryKind::Symlink).unwrap(),
        Candidate::Skip(_)
    ));
    let l = layout("source", "s3://bucket/target", false, &["/target"], false);
    assert!(matches!(
        l.resolve(&location("source"), EntryKind::Special).unwrap(),
        Candidate::Skip(_)
    ));
    let l = layout("source", "s3://bucket/target", false, &[], false);
    assert!(l.resolve(&location("source"), EntryKind::Symlink).is_err());
    assert!(l.resolve(&location("source"), EntryKind::Special).is_err());
    assert!(matches!(
        layout(
            "s3://bucket/parent/file.tmp",
            "out",
            false,
            &["/file.tmp"],
            false
        )
        .resolve(&location("s3://bucket/parent/file.tmp"), EntryKind::File)
        .unwrap(),
        Candidate::Skip(_)
    ));
}

#[test]
fn gitignore_negation_parent_exclusion_anchors_and_no_implicit_git_files() {
    let f = IgnoreFilter::new(
        location("."),
        &[
            "*.tmp",
            "!keep.tmp",
            "/root.txt",
            "excluded/",
            "!excluded/keep.txt",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>(),
    )
    .unwrap();
    for path in [
        "x.tmp",
        "sub/x.tmp",
        "root.txt",
        "excluded/keep.txt",
        "excluded/sub/file",
    ] {
        assert!(
            f.is_ignored(&location(path), false).unwrap(),
            "not ignored: {path}"
        );
    }
    for path in [
        "keep.tmp",
        "sub/keep.tmp",
        "sub/root.txt",
        ".git",
        ".DS_Store",
    ] {
        assert!(
            !f.is_ignored(&location(path), false).unwrap(),
            "ignored: {path}"
        );
    }
}

#[test]
fn unsafe_remote_to_local_names_fail_but_remote_to_remote_preserves_them() {
    for key in ["../escape", "a/../../escape", "/absolute", "a//b", "./name"] {
        let src = format!("s3://bucket/root/{key}");
        assert!(
            layout("s3://bucket/root", "out", true, &[], false)
                .resolve(&location(&src), EntryKind::File)
                .is_err(),
            "accepted {key}"
        );
        assert_eq!(
            destination(
                layout("s3://bucket/root", "s3://other/out", true, &[], false)
                    .resolve(&location(&src), EntryKind::File)
                    .unwrap()
            ),
            format!("s3://other/out/{key}")
        );
    }
}

#[test]
fn comma_list_escaping_preserves_gitignore_escapes() {
    assert!(parse_ignore_list("").is_empty());
    assert_eq!(
        parse_ignore_list(r"a\,b,*.tmp,\\name,\!literal,\#hash,tail\"),
        vec!["a,b", "*.tmp", r"\name", r"\!literal", r"\#hash", r"tail\"]
    );
}

#[test]
fn non_utf8_local_name_is_never_lossily_used_as_an_s3_key() {
    use std::os::unix::ffi::OsStrExt;
    let source = Location::Local(Path::new("/work/root").join(OsStr::from_bytes(b"\xff")));
    assert!(
        layout("root", "s3://bucket/out", true, &[], false)
            .resolve(&source, EntryKind::File)
            .is_err()
    );
}
