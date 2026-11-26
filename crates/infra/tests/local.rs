use archive_domain::*;
use archive_infra::{LocalLoader, LocalSaver};
use archive_sync::*;
use sha2::Digest;
use std::{
    io::Write,
    path::Path,
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, ReadBuf};

#[derive(Default)]
struct Warnings(Mutex<Vec<Warning>>);
impl WarningSink for Warnings {
    fn warn<'a>(&'a self, warning: Warning) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.0.lock().unwrap().push(warning);
            Ok(())
        })
    }
}
fn local(path: &Path) -> Location {
    Location::Local(path.into())
}
fn metadata(size: u64, checksum: Option<Checksum>) -> ObjectMetadata {
    let mut meta = ObjectMetadata::file(local(Path::new("/source")), size);
    meta.checksum = checksum;
    meta
}
fn sha256(data: &[u8]) -> Checksum {
    Checksum {
        algorithm: ChecksumAlgorithm::Sha256,
        scope: ChecksumScope::FullObject,
        value: sha2::Sha256::digest(data).to_vec(),
    }
}
fn entries(path: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[tokio::test]
async fn real_files_stream_through_object_safe_ports_and_replace_after_verification() {
    let temp = tempfile::tempdir().unwrap();
    let source = local(&temp.path().join("source"));
    let destination = local(&temp.path().join("new/dir/output"));
    let mut file = std::fs::File::create(temp.path().join("source")).unwrap();
    let buffer: Vec<_> = (0..65536).map(|i| ((i * 17 + 13) % 251) as u8).collect();
    let mut expected = sha2::Sha256::new();
    for _ in 0..49 {
        file.write_all(&buffer).unwrap();
        expected.update(&buffer);
    }
    drop(file);
    let loader: &dyn Loader = &LocalLoader;
    let saver: &dyn Saver = &LocalSaver;
    let cancel = CancelContext::new();
    assert!(matches!(
        saver.probe(&destination, &cancel).await.unwrap(),
        DestinationObservation::Missing
    ));
    let mut opened = loader.open(&source, &cancel).await.unwrap();
    assert_eq!(opened.metadata.size, Some(49 * 65536));
    opened.metadata.checksum = Some(Checksum {
        algorithm: ChecksumAlgorithm::Sha256,
        scope: ChecksumScope::FullObject,
        value: expected.finalize().to_vec(),
    });
    let mut session = saver
        .begin(&destination, &opened.metadata, &cancel)
        .await
        .unwrap();
    assert!(!temp.path().join("new/dir/output").exists());
    let warnings = Warnings::default();
    let result = stream_to_session(&mut opened.body, session.as_mut(), &warnings, &cancel)
        .await
        .unwrap();
    assert_eq!(result.bytes, 49 * 65536);
    assert_eq!(result.verification, Verification::SizeAndChecksum);
    assert!(
        warnings
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|w| w.category == archive_domain::WarningCategory::Attributes)
    );
    // Compare incrementally rather than collecting either complete body.
    let mut src = std::fs::File::open(temp.path().join("source")).unwrap();
    let mut dst = std::fs::File::open(temp.path().join("new/dir/output")).unwrap();
    assert_eq!(
        std::io::copy(&mut src, &mut std::io::sink()).unwrap(),
        result.bytes
    );
    use std::io::Read;
    let mut hash = sha2::Sha256::new();
    let mut chunk = [0; 65536];
    loop {
        let n = dst.read(&mut chunk).unwrap();
        if n == 0 {
            break;
        }
        hash.update(&chunk[..n]);
    }
    assert_eq!(
        hash.finalize().to_vec(),
        opened.metadata.checksum.unwrap().value
    );
    assert_eq!(entries(&temp.path().join("new/dir")), vec!["output"]);
}

#[tokio::test]
async fn short_long_and_bad_checksum_bodies_preserve_existing_destination_and_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("output");
    let destination = local(&path);
    let cancel = CancelContext::new();
    for (expected_size, expected_hash, content) in [
        (4, Some(sha256(b"abcd")), b"abc".as_slice()),
        (2, Some(sha256(b"ab")), b"abc".as_slice()),
        (3, Some(sha256(b"xyz")), b"abc".as_slice()),
    ] {
        std::fs::write(&path, b"original").unwrap();
        let meta = metadata(expected_size, expected_hash);
        let mut session = LocalSaver
            .begin(&destination, &meta, &cancel)
            .await
            .unwrap();
        let mut reader: BodyReader = Box::pin(std::io::Cursor::new(content));
        let error = stream_to_session(&mut reader, session.as_mut(), &Warnings::default(), &cancel)
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::Integrity);
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(entries(temp.path()), vec!["output"]);
        assert!(
            session
                .finish(true, &Warnings::default(), &cancel)
                .await
                .is_err()
        );
        session.abort().await.unwrap();
    }
}

struct FailingReader {
    sent: bool,
}
impl AsyncRead for FailingReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.sent {
            self.sent = true;
            output.put_slice(b"abc");
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(std::io::Error::other(
                "injected failure after expected bytes",
            )))
        }
    }
}

#[tokio::test]
async fn exact_size_is_insufficient_without_a_clean_body_end() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("output");
    std::fs::write(&path, b"original").unwrap();
    let cancel = CancelContext::new();
    let mut session = LocalSaver
        .begin(&local(&path), &metadata(3, Some(sha256(b"abc"))), &cancel)
        .await
        .unwrap();
    let mut body: BodyReader = Box::pin(FailingReader { sent: false });
    assert!(
        stream_to_session(&mut body, session.as_mut(), &Warnings::default(), &cancel)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert_eq!(entries(temp.path()), vec!["output"]);
    let mut session = LocalSaver
        .begin(&local(&path), &metadata(3, None), &cancel)
        .await
        .unwrap();
    session.write(b"abc", &cancel).await.unwrap();
    assert_eq!(
        session
            .finish(false, &Warnings::default(), &cancel)
            .await
            .err()
            .unwrap()
            .kind,
        ErrorKind::Integrity
    );
    session.abort().await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"original");
}

#[tokio::test]
async fn zero_length_and_unverifiable_checksums_have_explicit_results_and_warnings() {
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancelContext::new();
    for (i, checksum) in [
        None,
        Some(sha256(b"")),
        Some(Checksum {
            algorithm: ChecksumAlgorithm::Other("external".into()),
            scope: ChecksumScope::FullObject,
            value: vec![1],
        }),
        Some(Checksum {
            scope: ChecksumScope::Composite,
            ..sha256(b"")
        }),
    ]
    .into_iter()
    .enumerate()
    {
        let path = temp.path().join(format!("out{i}"));
        let mut session = LocalSaver
            .begin(&local(&path), &metadata(0, checksum), &cancel)
            .await
            .unwrap();
        let mut reader: BodyReader = Box::pin(tokio::io::empty());
        let warnings = Warnings::default();
        let result = stream_to_session(&mut reader, session.as_mut(), &warnings, &cancel)
            .await
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(
            warnings
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|w| w.category == archive_domain::WarningCategory::Verification)
                .count(),
            if i == 1 { 0 } else { 1 }
        );
        assert_eq!(
            result.verification,
            if i == 1 {
                Verification::SizeAndChecksum
            } else {
                Verification::SizeOnly
            }
        );
        session.abort().await.unwrap(); // must not remove committed destination
        assert!(path.exists());
    }
}

#[tokio::test]
async fn standard_checksum_vectors_are_verified_over_multiple_chunks() {
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancelContext::new();
    for (i, (algorithm, hex)) in [
        (ChecksumAlgorithm::Crc32, "cbf43926"),
        (ChecksumAlgorithm::Crc32c, "e3069283"),
        (ChecksumAlgorithm::Crc64Nvme, "ae8b14860a799888"),
        (
            ChecksumAlgorithm::Sha1,
            "f7c3bc1d808e04732adf679965ccc34ca7ae3441",
        ),
        (
            ChecksumAlgorithm::Sha256,
            "15e2b0d3c33891ebb0f1ef609ec419420c20e320ce94c65fbc8c3312448eb225",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let value = (0..hex.len())
            .step_by(2)
            .map(|j| u8::from_str_radix(&hex[j..j + 2], 16).unwrap())
            .collect();
        let expected = Checksum {
            algorithm,
            scope: ChecksumScope::FullObject,
            value,
        };
        let mut session = LocalSaver
            .begin(
                &local(&temp.path().join(format!("out{i}"))),
                &metadata(9, Some(expected)),
                &cancel,
            )
            .await
            .unwrap();
        session.write(b"123", &cancel).await.unwrap();
        session.write(b"456789", &cancel).await.unwrap();
        let result = session
            .finish(true, &Warnings::default(), &cancel)
            .await
            .unwrap();
        assert_eq!(result.verification, Verification::SizeAndChecksum);
    }
}

#[tokio::test]
async fn cancellation_before_and_during_transfer_preserves_destination() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("output");
    std::fs::write(&path, b"original").unwrap();
    let cancel = CancelContext::new();
    let mut session = LocalSaver
        .begin(&local(&path), &metadata(6, None), &cancel)
        .await
        .unwrap();
    session.write(b"abc", &cancel).await.unwrap();
    cancel.cancel();
    let mut reader: BodyReader = Box::pin(std::io::Cursor::new(b"def"));
    assert_eq!(
        stream_to_session(&mut reader, session.as_mut(), &Warnings::default(), &cancel)
            .await
            .err()
            .unwrap()
            .kind,
        ErrorKind::Cancelled
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert_eq!(entries(temp.path()), vec!["output"]);
    assert!(
        LocalSaver
            .begin(&local(&path), &metadata(0, None), &cancel)
            .await
            .is_err()
    );
    let cancel = CancelContext::new();
    let expired = cancel.with_deadline(tokio::time::Instant::now());
    assert!(LocalLoader.stat(&local(&path), &expired).await.is_err());
}

struct PendingReader;
impl AsyncRead for PendingReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
}

struct PendingWarnings;
impl WarningSink for PendingWarnings {
    fn warn<'a>(&'a self, _warning: Warning) -> PortFuture<'a, ()> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn cancellation_while_delivering_a_warning_still_preserves_the_destination() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("output");
    std::fs::write(&path, b"original").unwrap();
    let cancel = CancelContext::new();
    let mut session = LocalSaver
        .begin(&local(&path), &metadata(0, None), &cancel)
        .await
        .unwrap();
    let timed =
        cancel.with_deadline(tokio::time::Instant::now() + std::time::Duration::from_millis(20));
    let mut reader: BodyReader = Box::pin(tokio::io::empty());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream_to_session(&mut reader, session.as_mut(), &PendingWarnings, &timed),
    )
    .await
    .unwrap();
    assert_eq!(result.err().unwrap().kind, ErrorKind::Cancelled);
    assert_eq!(std::fs::read(path).unwrap(), b"original");
    assert_eq!(entries(temp.path()), vec!["output"]);
}

#[tokio::test]
async fn deadline_interrupts_a_pending_body_and_cleans_the_session() {
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancelContext::new();
    let mut session = LocalSaver
        .begin(
            &local(&temp.path().join("output")),
            &metadata(1, None),
            &cancel,
        )
        .await
        .unwrap();
    let timed =
        cancel.with_deadline(tokio::time::Instant::now() + std::time::Duration::from_millis(20));
    let mut reader: BodyReader = Box::pin(PendingReader);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream_to_session(&mut reader, session.as_mut(), &Warnings::default(), &timed),
    )
    .await
    .unwrap();
    assert_eq!(result.err().unwrap().kind, ErrorKind::Cancelled);
    assert!(entries(temp.path()).is_empty());
}

#[tokio::test]
async fn symlinks_and_special_files_are_not_opened_and_ignored_ones_are_pruned() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancelContext::new();
    std::fs::write(temp.path().join("regular"), b"abc").unwrap();
    symlink("regular", temp.path().join("link")).unwrap();
    symlink("missing", temp.path().join("dangling")).unwrap();
    std::fs::create_dir(temp.path().join("directory")).unwrap();
    symlink("directory", temp.path().join("dirlink")).unwrap();
    let fifo = temp.path().join("fifo");
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: CString is a live, NUL-terminated path; mkfifo retains no pointers.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    for name in ["link", "dangling", "fifo", "directory"] {
        let path = local(&temp.path().join(name));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                LocalLoader.open(&path, &cancel)
            )
            .await
            .unwrap()
            .is_err()
        );
        assert!(
            LocalSaver
                .begin(&path, &metadata(0, None), &cancel)
                .await
                .is_err()
        );
    }
    let link_root = local(&temp.path().join("dirlink/"));
    let selection = Selection::new(link_root, true, None, temp.path()).unwrap();
    let filter = IgnoreFilter::new(selection.ignore_base.clone(), &[]).unwrap();
    assert!(
        LocalLoader
            .enumerate(&selection, &filter, &cancel)
            .await
            .is_err()
    );
    let filter = IgnoreFilter::new(selection.ignore_base.clone(), &["dirlink".into()]).unwrap();
    let mut cursor = LocalLoader
        .enumerate(&selection, &filter, &cancel)
        .await
        .unwrap();
    assert!(matches!(
        cursor.next(&cancel).await.unwrap(),
        Some(EnumerationItem::Skipped(_))
    ));
    assert!(cursor.next(&cancel).await.unwrap().is_none());
}

#[tokio::test]
async fn enumeration_is_lazy_prunes_ignore_and_respects_depth_without_git_integration() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
    std::fs::create_dir_all(root.join("ignored")).unwrap();
    std::fs::write(root.join("a"), b"a").unwrap();
    std::fs::write(root.join(".gitignore"), b"*\n").unwrap();
    std::fs::write(root.join("sub/b"), b"b").unwrap();
    std::fs::write(root.join("sub/deeper/c"), b"c").unwrap();
    std::os::unix::fs::symlink("missing", root.join("ignored/link")).unwrap();
    let cancel = CancelContext::new();
    for (depth, expected) in [
        (Some(1), vec![".gitignore", "a"]),
        (Some(2), vec![".gitignore", "a", "sub/b"]),
        (None, vec![".gitignore", "a", "sub/b", "sub/deeper/c"]),
    ] {
        let selection = Selection::new(
            local(&root),
            true,
            depth.and_then(std::num::NonZeroU32::new),
            temp.path(),
        )
        .unwrap();
        let filter =
            IgnoreFilter::new(selection.ignore_base.clone(), &["ignored/".into()]).unwrap();
        let mut cursor = LocalLoader
            .enumerate(&selection, &filter, &cancel)
            .await
            .unwrap();
        let mut observed = vec![];
        let mut warnings = 0;
        while let Some(item) = cursor.next(&cancel).await.unwrap() {
            match item {
                EnumerationItem::Entry(meta) => {
                    let Location::Local(path) = meta.location else {
                        panic!()
                    };
                    observed.push(path.strip_prefix(&root).unwrap().display().to_string());
                }
                EnumerationItem::Skipped(_) => warnings += 1,
            }
        }
        observed.sort();
        assert_eq!(observed, expected);
        assert_eq!(warnings, 1);
    }
    let selection = Selection::new(local(&root), true, None, temp.path()).unwrap();
    let filter = IgnoreFilter::new(selection.ignore_base.clone(), &[]).unwrap();
    let mut cursor = LocalLoader
        .enumerate(&selection, &filter, &cancel)
        .await
        .unwrap();
    std::fs::rename(&root, temp.path().join("moved")).unwrap();
    assert!(cursor.next(&cancel).await.is_err()); // no eager tree collected by enumerate
    assert!(cursor.next(&cancel).await.unwrap().is_none()); // error is terminal
}

struct GeneratedReader {
    remaining: u64,
    max_buffer: std::sync::Arc<AtomicUsize>,
}
impl AsyncRead for GeneratedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.max_buffer
            .fetch_max(output.remaining(), Ordering::Relaxed);
        let count = self.remaining.min(output.remaining() as u64) as usize;
        let bytes = output.initialize_unfilled_to(count);
        bytes.fill(0x5a);
        output.advance(count);
        self.remaining -= count as u64;
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn body_larger_than_buffer_is_pulled_in_bounded_chunks_and_not_collected() {
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancelContext::new();
    let size = 8 * LOCAL_BUFFER_BYTES as u64 + 17;
    let max_buffer = std::sync::Arc::new(AtomicUsize::new(0));
    let mut body: BodyReader = Box::pin(GeneratedReader {
        remaining: size,
        max_buffer: max_buffer.clone(),
    });
    let path = temp.path().join("output");
    let mut session = LocalSaver
        .begin(&local(&path), &metadata(size, None), &cancel)
        .await
        .unwrap();
    let result = stream_to_session(&mut body, session.as_mut(), &Warnings::default(), &cancel)
        .await
        .unwrap();
    assert_eq!(result.bytes, size);
    assert_eq!(std::fs::metadata(path).unwrap().len(), size);
    assert!(max_buffer.load(Ordering::Relaxed) <= LOCAL_BUFFER_BYTES);
}

#[tokio::test]
async fn attributes_are_restored_and_attribute_failure_does_not_fail_verified_body() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("attributes");
    let cancel = CancelContext::new();
    let mut source = metadata(0, Some(sha256(b"")));
    source.original_mtime =
        Some(std::time::UNIX_EPOCH + std::time::Duration::new(1_300_000_000, 123456789));
    source.original_creation_time =
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_200_000_000));
    source.unix_mode = Some(0o640);
    let mut session = LocalSaver
        .begin(&local(&path), &source, &cancel)
        .await
        .unwrap();
    let mut body: BodyReader = Box::pin(tokio::io::empty());
    let warnings = Warnings::default();
    stream_to_session(&mut body, session.as_mut(), &warnings, &cancel)
        .await
        .unwrap();
    let restored = std::fs::metadata(&path).unwrap();
    assert_eq!(restored.modified().unwrap(), source.original_mtime.unwrap());
    assert_eq!(restored.permissions().mode() & 0o7777, 0o640);
    #[cfg(target_vendor = "apple")]
    assert_eq!(
        restored.created().unwrap(),
        source.original_creation_time.unwrap()
    );
    source.unix_mode = Some(0xffff);
    let mut session = LocalSaver
        .begin(&local(&path), &source, &cancel)
        .await
        .unwrap();
    let mut body: BodyReader = Box::pin(tokio::io::empty());
    let warnings = Warnings::default();
    let result = stream_to_session(&mut body, session.as_mut(), &warnings, &cancel)
        .await
        .unwrap();
    assert_eq!(result.verification, Verification::SizeAndChecksum);
    assert!(
        warnings
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|w| w.category == archive_domain::WarningCategory::Attributes)
    );
    assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
}
