use super::*;
use archive_domain::*;
use archive_sync::*;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

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

struct Request {
    method: String,
    target: String,
    class: Option<String>,
    checksum: Option<String>,
    copy_source: Option<String>,
    copy_range: Option<String>,
}
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    declared_size: Option<usize>,
}
impl Response {
    fn new(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![],
            body: body.into(),
            declared_size: None,
        }
    }
    fn header(mut self, key: &str, value: &str) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }
}
struct Mock {
    endpoint: String,
    seen: Arc<Mutex<Vec<Request>>>,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}
impl Mock {
    async fn start(handler: impl Fn(usize, &Request) -> Response + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(vec![]));
        let requests = seen.clone();
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = tokio::select! { _ = &mut stopped => break, accepted = listener.accept() => accepted.unwrap() };
                let mut buffer = Vec::new();
                let headers_end = loop {
                    let mut chunk = [0; 4096];
                    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
                        .await
                        .unwrap()
                        .unwrap();
                    if n == 0 {
                        break None;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    if let Some(offset) = buffer.windows(4).position(|v| v == b"\r\n\r\n") {
                        break Some(offset + 4);
                    }
                    assert!(buffer.len() < 65536);
                };
                let Some(end) = headers_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&buffer[..end]);
                let mut words = headers.lines().next().unwrap().split_whitespace();
                let request = Request {
                    method: words.next().unwrap().into(),
                    target: words.next().unwrap().into(),
                    class: headers.lines().find_map(|line| {
                        line.split_once(':')
                            .filter(|(k, _)| k.eq_ignore_ascii_case("x-amz-storage-class"))
                            .map(|(_, v)| v.trim().to_owned())
                    }),
                    copy_source: headers.lines().find_map(|line| {
                        line.split_once(':')
                            .filter(|(k, _)| k.eq_ignore_ascii_case("x-amz-copy-source"))
                            .map(|(_, v)| v.trim().to_owned())
                    }),
                    copy_range: headers.lines().find_map(|line| {
                        line.split_once(':')
                            .filter(|(k, _)| k.eq_ignore_ascii_case("x-amz-copy-source-range"))
                            .map(|(_, v)| v.trim().to_owned())
                    }),
                    checksum: headers.lines().find_map(|line| {
                        line.split_once(':')
                            .filter(|(k, _)| k.eq_ignore_ascii_case("x-amz-checksum-crc32"))
                            .map(|(_, v)| v.trim().to_owned())
                    }),
                };
                if headers
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("expect: 100-continue"))
                {
                    stream
                        .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                        .await
                        .unwrap();
                }
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|s| s.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let mut received = buffer.len() - end;
                while received < length {
                    let mut chunk = [0; 8192];
                    let n = stream.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    received += n;
                }
                let index = requests.lock().unwrap().len();
                let response = handler(index, &request);
                requests.lock().unwrap().push(request);
                if response.status == 0 {
                    let _ = stream.shutdown().await;
                    continue;
                }
                let mut bytes = format!(
                    "HTTP/1.1 {} response\r\nConnection: close\r\nContent-Length: {}\r\nx-amz-request-id: test-request\r\n",
                    response.status,
                    response.declared_size.unwrap_or(response.body.len())
                );
                for (key, value) in response.headers {
                    bytes.push_str(&format!("{key}: {value}\r\n"));
                }
                bytes.push_str("\r\n");
                bytes.push_str(&response.body);
                let _ = stream.write_all(bytes.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        Self {
            endpoint,
            seen,
            stop,
            task,
        }
    }
    async fn finish(self) -> Vec<Request> {
        let _ = self.stop.send(());
        self.task.await.unwrap();
        Arc::try_unwrap(self.seen)
            .unwrap_or_else(|_| panic!("outstanding server references"))
            .into_inner()
            .unwrap()
    }
}

#[tokio::test]
async fn read_errors_distinguish_403_404_and_do_not_retry_500() {
    for (status, kind) in [
        (403, ErrorKind::Permission),
        (404, ErrorKind::NotFound),
        (500, ErrorKind::Unavailable),
    ] {
        let server = Mock::start(move |_, _| Response::new(status, "")).await;
        let store = S3Store::for_test(&server.endpoint).unwrap();
        let location = store.object("bucket", "key").unwrap();
        let error = store
            .loader()
            .stat(&location, &CancelContext::new())
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, kind);
        assert_eq!(error.context.request_id.as_deref(), Some("test-request"));
        let seen = server.finish().await;
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "HEAD");
    }
}

#[tokio::test]
async fn list_is_lazy_pages_once_and_filters_depth_and_ignore() {
    let server = Mock::start(|index, request| {
        assert!(request.target.contains("max-keys=1000"));
        let xml = if index == 0 { "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>second</NextContinuationToken><Contents><Key>foo/a</Key><Size>3</Size></Contents><Contents><Key>foo/skip.tmp</Key><Size>1</Size></Contents></ListBucketResult>" }
            else { assert!(request.target.contains("continuation-token=second")); "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>foo/sub/b</Key><Size>1</Size></Contents><Contents><Key>foo/z</Key><Size>2</Size></Contents></ListBucketResult>" };
        Response::new(200, xml).header("Content-Type", "application/xml")
    }).await;
    let store = S3Store::for_test(&server.endpoint).unwrap();
    let cancel = CancelContext::new();
    let root = store.object("bucket", "foo").unwrap();
    let selection = Selection::new(
        root,
        true,
        std::num::NonZeroU32::new(1),
        std::path::Path::new("/work"),
    )
    .unwrap();
    let ignore = IgnoreFilter::new(selection.ignore_base.clone(), &["*.tmp".into()]).unwrap();
    let mut cursor = store
        .loader()
        .enumerate(&selection, &ignore, &cancel)
        .await
        .unwrap();
    assert!(server.seen.lock().unwrap().is_empty());
    assert!(matches!(
        cursor.next(&cancel).await.unwrap(),
        Some(EnumerationItem::Entry(_))
    ));
    assert_eq!(server.seen.lock().unwrap().len(), 1);
    assert!(matches!(
        cursor.next(&cancel).await.unwrap(),
        Some(EnumerationItem::Skipped(_))
    ));
    assert_eq!(server.seen.lock().unwrap().len(), 1);
    let Some(EnumerationItem::Entry(meta)) = cursor.next(&cancel).await.unwrap() else {
        panic!()
    };
    assert!(meta.location.display().ends_with("foo/z"));
    assert!(meta.data_class.is_none());
    assert!(cursor.next(&cancel).await.unwrap().is_none());
    assert_eq!(server.finish().await.len(), 2);
}

#[tokio::test]
async fn get_corruption_and_truncation_preserve_local_destination() {
    for truncated in [false, true] {
        let server = Mock::start(move |_, _| {
            let mut response = Response::new(200, if truncated { "ab" } else { "abc" })
                .header("x-amz-checksum-type", "FULL_OBJECT")
                .header("x-amz-checksum-crc32", "AAAAAA==");
            response.declared_size = Some(3);
            response
        })
        .await;
        let store = S3Store::for_test(&server.endpoint).unwrap();
        let cancel = CancelContext::new();
        let source = store.object("bucket", "key").unwrap();
        let mut opened = store.loader().open(&source, &cancel).await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("output");
        std::fs::write(&path, b"original").unwrap();
        let mut session = crate::LocalSaver
            .begin(&Location::Local(path.clone()), &opened.metadata, &cancel)
            .await
            .unwrap();
        assert!(
            stream_to_session(
                &mut opened.body,
                session.as_mut(),
                &Warnings::default(),
                &cancel
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
        assert_eq!(server.finish().await.len(), 1);
    }
}

#[test]
fn upload_planning_is_64_bit_and_checks_the_fixed_capacity() {
    for (size, parts) in [(100_000_000_000, 5961), (100 * 1024 * 1024 * 1024, 6400)] {
        let plan = UploadPlan::new(size).unwrap();
        assert_eq!(plan.parts, parts);
        let mut end = 0;
        for n in 1..=parts {
            let (offset, length) = plan.part(n).unwrap();
            assert_eq!(offset, end);
            end += length;
        }
        assert_eq!(end, size);
    }
    assert!(UploadPlan::new(16 * 1024 * 1024 * 10_000 + 1).is_err());
    assert_eq!(UploadPlan::new(0).unwrap().parts, 0);
}

struct GeneratedBody {
    remaining: u64,
    pending_at_end: bool,
}

#[tokio::test]
async fn sdk_preserves_opaque_keys_on_the_wire_and_checks_namespaces_before_io() {
    let server = Mock::start(|_, request| {
        assert!(
            request
                .target
                .contains("/bucket/a//./../literal%2520%3F%23"),
            "{}",
            request.target
        );
        Response::new(200, "")
    })
    .await;
    let store = S3Store::for_test(&server.endpoint).unwrap();
    let location = store.object("bucket", "a//./../literal%20?#").unwrap();
    let error = store
        .loader()
        .stat(
            &Location::Remote(ObjectId::parse_s3("s3://bucket/key").unwrap()),
            &CancelContext::new(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind, ErrorKind::Input);
    assert!(server.seen.lock().unwrap().is_empty());
    store
        .loader()
        .stat(&location, &CancelContext::new())
        .await
        .unwrap();
    assert_eq!(server.finish().await.len(), 1);
}
impl tokio::io::AsyncRead for GeneratedBody {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        output: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.remaining == 0 && self.pending_at_end {
            return std::task::Poll::Pending;
        }
        let n = self.remaining.min(output.remaining() as u64) as usize;
        output.initialize_unfilled_to(n).fill(0x5a);
        output.advance(n);
        self.remaining -= n as u64;
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn small_put_uses_selected_class_crc_and_loss_is_unknown() {
    for lost in [false, true] {
        let server = Mock::start(move |_, request| {
            assert_eq!(request.method, "PUT");
            assert_eq!(request.class.as_deref(), Some("DEEP_ARCHIVE"));
            assert_eq!(request.checksum.as_deref(), Some("AAAAAA=="));
            Response::new(if lost { 0 } else { 200 }, "").header("x-amz-checksum-crc32", "AAAAAA==")
        })
        .await;
        let store = S3Store::for_test(&server.endpoint).unwrap();
        let cancel = CancelContext::new();
        let location = store.object("bucket", "key").unwrap();
        let meta = ObjectMetadata::file(Location::Local("/source".into()), 0);
        let mut session = store
            .saver(DataClass::Archived)
            .begin(&location, &meta, &cancel)
            .await
            .unwrap();
        let mut body: BodyReader = Box::pin(tokio::io::empty());
        let result =
            stream_to_session(&mut body, session.as_mut(), &Warnings::default(), &cancel).await;
        if lost {
            assert_eq!(result.err().unwrap().outcome, OperationState::Unknown);
        } else {
            assert_eq!(result.unwrap().verification, Verification::SizeAndChecksum);
        }
        assert_eq!(server.finish().await.len(), 1);
    }
}

#[tokio::test]
async fn part_failure_wakes_a_pending_source_and_abort_is_never_retried() {
    for abort_fails in [false, true] {
        let server = Mock::start(move |_, request| {
            if request.method == "POST" { assert!(request.target.contains("uploads")); Response::new(200, "<InitiateMultipartUploadResult><UploadId>own-upload</UploadId></InitiateMultipartUploadResult>") }
            else if request.method == "PUT" { Response::new(500, "<Error><Code>InternalError</Code><Message>part failed</Message></Error>") }
            else { assert_eq!(request.method, "DELETE"); assert!(request.target.contains("uploadId=own-upload")); Response::new(if abort_fails { 500 } else { 204 }, "") }
        }).await;
        let store = S3Store::for_test(&server.endpoint).unwrap();
        let cancel = CancelContext::new();
        let location = store.object("bucket", "key").unwrap();
        let meta = ObjectMetadata::file(Location::Local("/source".into()), 32 * 1024 * 1024 + 1);
        let mut session = store
            .saver(DataClass::Standard)
            .begin(&location, &meta, &cancel)
            .await
            .unwrap();
        let mut body: BodyReader = Box::pin(GeneratedBody {
            remaining: 16 * 1024 * 1024,
            pending_at_end: true,
        });
        let warnings = Warnings::default();
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            stream_to_session(&mut body, session.as_mut(), &warnings, &cancel),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert_eq!(warnings.0.lock().unwrap().len(), usize::from(abort_fails));
        if abort_fails {
            assert!(session.abort().await.is_err());
        } else {
            session.abort().await.unwrap();
        }
        let requests = server.finish().await;
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].method, "DELETE");
    }
}

#[tokio::test]
async fn complete_http_200_embedded_error_is_not_success() {
    let server = Mock::start(|_, request| {
        if request.method == "POST" && request.target.contains("uploads") { Response::new(200, "<InitiateMultipartUploadResult><UploadId>own-upload</UploadId></InitiateMultipartUploadResult>") }
        else if request.method == "PUT" { Response::new(200, "").header("ETag", "\"part-etag\"") }
        else if request.method == "POST" { Response::new(200, "<Error><Code>InternalError</Code><Message>complete failed</Message></Error>").header("Content-Type", "application/xml") }
        else { assert_eq!(request.method, "DELETE"); Response::new(204, "") }
    }).await;
    let store = S3Store::for_test(&server.endpoint).unwrap();
    let cancel = CancelContext::new();
    let location = store.object("bucket", "key").unwrap();
    let size = 16 * 1024 * 1024 + 1;
    let meta = ObjectMetadata::file(Location::Local("/source".into()), size);
    let mut session = store
        .saver(DataClass::Standard)
        .begin(&location, &meta, &cancel)
        .await
        .unwrap();
    let mut body: BodyReader = Box::pin(GeneratedBody {
        remaining: size,
        pending_at_end: false,
    });
    assert!(
        stream_to_session(&mut body, session.as_mut(), &Warnings::default(), &cancel)
            .await
            .is_err()
    );
    let requests = server.finish().await;
    assert_eq!(requests.iter().filter(|r| r.method == "PUT").count(), 2);
    assert_eq!(requests.iter().filter(|r| r.method == "POST").count(), 2);
    assert_eq!(requests.iter().filter(|r| r.method == "DELETE").count(), 1);
}

#[tokio::test]
#[ignore = "requires Docker SeaweedFS on 127.0.0.1:18333"]
async fn docker_upload_download_boundaries_overwrite_and_checksum() {
    use futures_util::FutureExt;
    use sha2::Digest;
    use std::io::{Read, Write};
    let store = S3Store::for_test("http://127.0.0.1:18333").unwrap();
    let cancel = CancelContext::new();
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temp = tempfile::tempdir().unwrap();
    for (index, size) in [
        0_u64,
        1024 * 1024,
        16 * 1024 * 1024,
        16 * 1024 * 1024 + 1,
        32 * 1024 * 1024 + 17,
    ]
    .into_iter()
    .enumerate()
    {
        let key = format!("rust-adapter-{run}/case-{index}/literal%20?#");
        let remote = store.object("private-archive-test", &key).unwrap();
        let result = std::panic::AssertUnwindSafe(async {
            let input = temp.path().join("source");
            let output = temp.path().join("output");
            let mut file = std::fs::File::create(&input).unwrap();
            let mut hash = sha2::Sha256::new();
            let block: Vec<_> = (0..65536)
                .map(|i| ((i * 17 + index * 13) % 251) as u8)
                .collect();
            let mut remaining = size;
            while remaining > 0 {
                let n = remaining.min(block.len() as u64) as usize;
                file.write_all(&block[..n]).unwrap();
                hash.update(&block[..n]);
                remaining -= n as u64;
            }
            drop(file);
            let expected = hash.finalize().to_vec();
            let mut source = crate::LocalLoader
                .open(&Location::Local(input), &cancel)
                .await?;
            let mut upload = store
                .saver(DataClass::Standard)
                .begin(&remote, &source.metadata, &cancel)
                .await?;
            let saved = stream_to_session(
                &mut source.body,
                upload.as_mut(),
                &Warnings::default(),
                &cancel,
            )
            .await?;
            assert_eq!(saved.bytes, size);
            let observed = store.loader().stat(&remote, &cancel).await?;
            assert_eq!(observed.size, Some(size));
            let mut opened = store.loader().open(&remote, &cancel).await?;
            let full_checksum = opened.metadata.checksum.as_ref().is_some_and(|c| c.scope == ChecksumScope::FullObject);
            std::fs::write(&output, b"existing destination").unwrap();
            let mut download = crate::LocalSaver
                .begin(&Location::Local(output.clone()), &opened.metadata, &cancel)
                .await?;
            let warnings = Warnings::default();
            let saved =
                stream_to_session(&mut opened.body, download.as_mut(), &warnings, &cancel).await?;
            assert_eq!(saved.bytes, size);
            assert_eq!(saved.verification, if full_checksum { Verification::SizeAndChecksum } else { Verification::SizeOnly });
            {
                let observed_warnings = warnings.0.lock().unwrap();
                assert_eq!(observed_warnings.len(), usize::from(!full_checksum));
                if !full_checksum { assert_eq!(observed_warnings[0].category, WarningCategory::Verification); }
            }
            let mut hash = sha2::Sha256::new();
            let mut buffer = [0; 65536];
            let mut file = std::fs::File::open(output).unwrap();
            loop {
                let n = file.read(&mut buffer).unwrap();
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            assert_eq!(hash.finalize().to_vec(), expected);
            eprintln!(
                "Docker adapter: {size} bytes, {} multipart parts, SHA256 verified, FULL_OBJECT CRC32 verified={full_checksum}",
                UploadPlan::new(size).unwrap().parts
            );
            if index == 1 {
                assert!(matches!(store.saver(DataClass::Standard).probe(&remote, &cancel).await?, DestinationObservation::Existing(_)));
                let replacement = b"replacement";
                let meta = ObjectMetadata::file(Location::Local("/source".into()), replacement.len() as u64);
                let mut session = store.saver(DataClass::Standard).begin(&remote, &meta, &cancel).await?;
                let mut body: BodyReader = Box::pin(std::io::Cursor::new(replacement));
                stream_to_session(&mut body, session.as_mut(), &Warnings::default(), &cancel).await?;
                let mut opened = store.loader().open(&remote, &cancel).await?;
                let mut bytes = vec![]; opened.body.read_to_end(&mut bytes).await.unwrap();
                assert_eq!(bytes, replacement);
            }
            Ok::<(), OperationError>(())
        }).catch_unwind().await;
        let cleanup = store
            .client
            .delete_object()
            .bucket("private-archive-test")
            .key(&key)
            .send()
            .await;
        assert!(cleanup.is_ok());
        assert_eq!(
            store
                .loader()
                .stat(&remote, &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::NotFound
        );
        match result {
            Ok(result) => result.unwrap(),
            Err(panic) => std::panic::resume_unwind(panic),
        };
    }
}

#[tokio::test]
async fn profile_config_child() {
    use aws_credential_types::provider::ProvideCredentials;
    if std::env::var_os("ARCHIVE_AUTH_PROBE").is_none() {
        return;
    }
    let result = S3Store::from_env(&CancelContext::new()).await;
    if std::env::var_os("ARCHIVE_ENDPOINT_PROBE").is_some() {
        assert_eq!(result.err().unwrap().kind, ErrorKind::Input);
        return;
    }
    let store = result.unwrap();
    let config = store.client.config();
    assert_eq!(config.region().unwrap().as_ref(), "us-west-2");
    assert_eq!(config.retry_config().unwrap().max_attempts(), 1);
    let shared = standard_sdk_config(&CancelContext::new()).await.unwrap();
    assert_eq!(shared.retry_config().unwrap().max_attempts(), 1);
    let credentials = shared
        .credentials_provider()
        .unwrap()
        .provide_credentials()
        .await
        .unwrap();
    assert_eq!(credentials.access_key_id(), "dummy-chosen-profile");
    assert_eq!(store.namespace, "aws");
}

#[test]
fn standard_profile_selection_and_retry_overrides_are_isolated_from_host_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config");
    let credentials = temp.path().join("credentials");
    std::fs::write(
        &config,
        "[profile chosen]\nregion=us-west-2\nmax_attempts=12\n",
    )
    .unwrap();
    std::fs::write(&credentials, "[chosen]\naws_access_key_id=dummy-chosen-profile\naws_secret_access_key=dummy-secret\n[default]\naws_access_key_id=wrong-default\naws_secret_access_key=wrong-secret\n").unwrap();
    for endpoint in [
        None,
        Some("AWS_ENDPOINT_URL_S3"),
        Some("AWS_ENDPOINT_URL_STS"),
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "s3::tests::profile_config_child", "--nocapture"])
            .env_clear()
            .env("HOME", temp.path())
            .env("AWS_CONFIG_FILE", &config)
            .env("AWS_SHARED_CREDENTIALS_FILE", &credentials)
            .env("AWS_PROFILE", "chosen")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("AWS_MAX_ATTEMPTS", "9")
            .env("ARCHIVE_AUTH_PROBE", "1");
        if let Some(key) = endpoint {
            command
                .env("ARCHIVE_ENDPOINT_PROBE", "1")
                .env(key, "http://127.0.0.1:1");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn copy_plan_100gb_offsets_and_capacity_without_body() {
    for (size, parts) in [(100_000_000_000, 746), (100 * 1024 * 1024 * 1024, 800)] {
        let plan = CopyPlan::new(size).unwrap();
        assert_eq!(plan.parts, parts);
        let mut end = 0;
        for number in 1..=parts {
            let (offset, length) = plan.part(number).unwrap();
            assert_eq!(offset, end);
            end += length;
        }
        assert_eq!(end, size);
    }
    assert_eq!(CopyPlan::new(16 * 1024 * 1024).unwrap().parts, 0);
    assert_eq!(CopyPlan::new(16 * 1024 * 1024 + 1).unwrap().parts, 1);
    assert!(CopyPlan::new(128 * 1024 * 1024 * 10_000 + 1).is_err());
}

#[tokio::test]
async fn copy_uses_encoded_source_and_detects_embedded_errors_and_response_loss() {
    for status in [200, 403, 500, 0, 201] {
        let server = Mock::start(move |_, request| {
            if request.method == "HEAD" {
                return Response::new(200, "");
            }
            assert_eq!(request.method, "PUT");
            assert_eq!(
                request.copy_source.as_deref(),
                Some("bucket/a//./../literal%2520%3F%23")
            );
            assert_eq!(request.class.as_deref(), Some("DEEP_ARCHIVE"));
            match status {
                200 => Response::new(
                    200,
                    "<CopyObjectResult><ETag>\"copy\"</ETag></CopyObjectResult>",
                ),
                201 => Response::new(
                    200,
                    "<Error><Code>InternalError</Code><Message>embedded failure</Message></Error>",
                ),
                other => Response::new(
                    other,
                    "<Error><Code>AccessDenied</Code><Message>copy failed</Message></Error>",
                ),
            }
        })
        .await;
        let store = S3Store::for_test(&server.endpoint).unwrap();
        let source =
            ObjectMetadata::file(store.object("bucket", "a//./../literal%20?#").unwrap(), 0);
        let warnings = Warnings::default();
        let result = store
            .copier(DataClass::Archived)
            .copy(
                &source,
                &store.object("bucket", "dest").unwrap(),
                &warnings,
                &CancelContext::new(),
            )
            .await;
        if status == 200 {
            assert_eq!(result.unwrap().verification, Verification::SizeOnly);
            assert_eq!(
                warnings
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|w| w.category == WarningCategory::Verification)
                    .count(),
                1
            );
        } else {
            let error = result.err().unwrap();
            assert!(error.context.source_uri.is_some());
            if status == 0 {
                assert_eq!(error.outcome, OperationState::Unknown);
            }
        }
        let seen = server.finish().await;
        assert_eq!(seen.iter().filter(|r| r.method == "PUT").count(), 1);
        assert_eq!(seen.iter().filter(|r| r.method == "GET").count(), 0);
    }
}

#[tokio::test]
async fn multipart_copy_ranges_complete_and_abort_own_upload_on_failure() {
    for fail in [false, true] {
        let size = 128 * 1024 * 1024 + 17;
        let server = Mock::start(move |_, request| {
            if request.method == "HEAD" {
                let mut response = Response::new(200, ""); response.declared_size = Some(size as usize); return response;
            }
            if request.method == "POST" && request.target.contains("uploads") {
                return Response::new(200, "<InitiateMultipartUploadResult><UploadId>copy-own-id</UploadId></InitiateMultipartUploadResult>");
            }
            assert!(request.target.contains("uploadId=copy-own-id"));
            if request.method == "DELETE" { return Response::new(204, ""); }
            if request.method == "PUT" {
                assert!(matches!(request.copy_range.as_deref(), Some("bytes=0-134217727" | "bytes=134217728-134217744")));
                if fail { return Response::new(200, "<Error><Code>InternalError</Code><Message>part failed</Message></Error>"); }
                return Response::new(200, "<CopyPartResult><ETag>\"part\"</ETag></CopyPartResult>");
            }
            Response::new(200, "<CompleteMultipartUploadResult><ETag>\"complete\"</ETag></CompleteMultipartUploadResult>")
        }).await;
        let store = S3Store::for_test(&server.endpoint).unwrap();
        let source = ObjectMetadata::file(store.object("bucket", "source").unwrap(), size);
        let result = store
            .copier(DataClass::Standard)
            .copy(
                &source,
                &store.object("bucket", "dest").unwrap(),
                &Warnings::default(),
                &CancelContext::new(),
            )
            .await;
        assert_eq!(result.is_err(), fail);
        let seen = server.finish().await;
        assert_eq!(
            seen.iter().filter(|r| r.method == "DELETE").count(),
            usize::from(fail)
        );
        assert_eq!(seen.iter().filter(|r| r.method == "GET").count(), 0);
        if !fail {
            assert_eq!(seen.iter().filter(|r| r.method == "PUT").count(), 2);
        }
    }
}

#[tokio::test]
async fn copy_cancellation_keeps_confirmed_single_success_and_aborts_multipart() {
    for multipart in [false, true] {
        let cancel = CancelContext::new();
        let signal = cancel.clone();
        let server = Mock::start(move |_, request| {
            if request.method == "HEAD" { return Response::new(200, ""); }
            if request.method == "POST" { return Response::new(200, "<InitiateMultipartUploadResult><UploadId>cancel-own-id</UploadId></InitiateMultipartUploadResult>"); }
            if request.method == "DELETE" { assert!(request.target.contains("cancel-own-id")); return Response::new(204, ""); }
            signal.cancel();
            Response::new(200, if multipart { "<CopyPartResult><ETag>\"part\"</ETag></CopyPartResult>" } else { "<CopyObjectResult><ETag>\"copy\"</ETag></CopyObjectResult>" })
        }).await;
        let store = S3Store::for_test(&server.endpoint).unwrap();
        let source = ObjectMetadata::file(
            store.object("bucket", "source").unwrap(),
            if multipart { 128 * 1024 * 1024 + 1 } else { 0 },
        );
        let result = store
            .copier(DataClass::Standard)
            .copy(
                &source,
                &store.object("bucket", "dest").unwrap(),
                &Warnings::default(),
                &cancel,
            )
            .await;
        if multipart {
            assert_eq!(result.err().unwrap().kind, ErrorKind::Cancelled);
        } else {
            result.unwrap();
        }
        let seen = server.finish().await;
        assert_eq!(
            seen.iter().filter(|r| r.method == "DELETE").count(),
            usize::from(multipart)
        );
        assert_eq!(
            seen.iter().filter(|r| r.method == "POST").count(),
            usize::from(multipart)
        );
    }
}

#[tokio::test]
#[ignore = "requires Docker SeaweedFS on 127.0.0.1:18333"]
async fn docker_server_copy_single_and_multipart_sha256() {
    use sha2::{Digest, Sha256};
    let store = S3Store::for_test("http://127.0.0.1:18333").unwrap();
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let cancel = CancelContext::new();
    for size in [0, 1024 * 1024, 16 * 1024 * 1024 + 1, 128 * 1024 * 1024 + 17] {
        let source_key = format!("rust-copy-{run}/{size}/source");
        let dest_key = format!("rust-copy-{run}/{size}/dest");
        let source = store.object("private-archive-test", &source_key).unwrap();
        let destination = store.object("private-archive-test", &dest_key).unwrap();
        let operation = async {
            let meta = ObjectMetadata::file(Location::Local("/generated".into()), size);
            let mut session = store
                .saver(DataClass::Standard)
                .begin(&source, &meta, &cancel)
                .await?;
            let mut body: BodyReader = Box::pin(GeneratedBody {
                remaining: size,
                pending_at_end: false,
            });
            stream_to_session(&mut body, session.as_mut(), &Warnings::default(), &cancel).await?;
            let meta = store.loader().stat(&source, &cancel).await?;
            let saved = store
                .copier(DataClass::Standard)
                .copy(&meta, &destination, &Warnings::default(), &cancel)
                .await?;
            assert_eq!(saved.bytes, size);
            let mut expected = Sha256::new();
            let block = [0x5a; 65536];
            let mut remaining = size;
            while remaining > 0 {
                let n = remaining.min(block.len() as u64) as usize;
                expected.update(&block[..n]);
                remaining -= n as u64;
            }
            let mut actual = Sha256::new();
            let mut opened = store.loader().open(&destination, &cancel).await?;
            let mut block = [0; 65536];
            loop {
                let n = opened.body.read(&mut block).await.unwrap();
                if n == 0 {
                    break;
                }
                actual.update(&block[..n]);
            }
            assert_eq!(actual.finalize(), expected.finalize());
            eprintln!(
                "Docker server copy: {size} bytes, {} copy parts, SHA256 verified",
                CopyPlan::new(size).unwrap().parts
            );
            Ok::<_, OperationError>(())
        }
        .await;
        for key in [&source_key, &dest_key] {
            store
                .client
                .delete_object()
                .bucket("private-archive-test")
                .key(key)
                .send()
                .await
                .unwrap();
        }
        operation.unwrap();
    }
}

struct CoreObserver {
    warnings: Warnings,
    copies: Mutex<Vec<CopyOutcome>>,
}
impl WarningSink for CoreObserver {
    fn warn<'a>(&'a self, warning: Warning) -> PortFuture<'a, ()> {
        self.warnings.warn(warning)
    }
}
impl OperationObserver for CoreObserver {
    fn copied<'a>(&'a self, outcome: CopyOutcome) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.copies.lock().unwrap().push(outcome);
            Ok(())
        })
    }
}
#[tokio::test]
#[ignore = "requires Docker SeaweedFS on 127.0.0.1:18333"]
async fn docker_copy_engine_three_routes_with_disk_manifest_and_real_index() {
    use archive_index::IndexRead;
    use sha2::{Digest, Sha256};
    let store = S3Store::for_test("http://127.0.0.1:18333").unwrap();
    let cancel = CancelContext::new();
    let temp = tempfile::tempdir().unwrap();
    let index = crate::SqliteIndex::open(temp.path().join("index.db"))
        .await
        .unwrap();
    let loader = store.loader();
    let saver = store.saver(DataClass::Standard);
    let copier = store.copier(DataClass::Standard);
    let engine = CopyEngine {
        local_loader: &crate::LocalLoader,
        local_saver: &crate::LocalSaver,
        remote_loader: &loader,
        remote_saver: &saver,
        remote_copy: &copier,
        index: Ok(&index),
    };
    let observer = CoreObserver {
        warnings: Warnings::default(),
        copies: Mutex::new(vec![]),
    };
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prefix = format!("rust-core-{run}");
    let source_key = format!("{prefix}/src/a");
    let dest_key = format!("{prefix}/dest/a");
    let source = store.object("private-archive-test", &source_key).unwrap();
    let destination = store.object("private-archive-test", &dest_key).unwrap();
    let data: Vec<_> = (0..1024 * 1024 + 17)
        .map(|n| ((n * 19) % 251) as u8)
        .collect();
    let input = temp.path().join("input");
    std::fs::write(&input, &data).unwrap();
    let output = temp.path().join("output");
    std::fs::create_dir(&output).unwrap();
    std::fs::write(output.join("a"), b"previous data").unwrap();
    let operation=async {
        let requests=[(Location::Local(input),source.clone(),false),(store.object("private-archive-test",&format!("{prefix}/src")).unwrap(),store.object("private-archive-test",&format!("{prefix}/dest")).unwrap(),true),(store.object("private-archive-test",&format!("{prefix}/dest")).unwrap(),Location::Local(output.clone()),true)];
        for(source,destination,recursive)in requests{let request=CopyRequest{protected:None,source:Selection::new(source,recursive,None,temp.path()).unwrap(),destination,cwd:temp.path().into(),ignore:vec![]};let mut plan=crate::DiskPlan::new().await?;let summary=engine.copy(&request,&mut plan,&observer,&cancel).await?;assert_eq!(summary.files,1);assert_eq!(summary.bytes,data.len()as u64);}
        assert_eq!(Sha256::digest(std::fs::read(output.join("a")).unwrap()),Sha256::digest(&data));
        for remote in [&source,&destination]{let Location::Remote(id)=remote else{panic!()};let meta=index.get(id).await?.unwrap();assert_eq!(meta.size,Some(data.len()as u64));assert!(meta.queried_at.is_some());}
        assert_eq!(observer.copies.lock().unwrap().len(),3);
        eprintln!("Docker CopyEngine: 3 routes, recursive manifest, overwrite, SQLite observations and SHA256 verified ({} bytes each)",data.len());
        Ok::<_,OperationError>(())
    }.await;
    for key in [&source_key, &dest_key] {
        store
            .client
            .delete_object()
            .bucket("private-archive-test")
            .key(key)
            .send()
            .await
            .unwrap();
    }
    index.close().await.unwrap();
    operation.unwrap();
}

#[tokio::test]
async fn server_copy_verifies_destination_size_and_comparable_full_checksum() {
    for case in 0..4 {
        let server = Mock::start(move |_, request| {
            if request.method == "PUT" {
                return Response::new(
                    200,
                    "<CopyObjectResult><ETag>\"copy\"</ETag></CopyObjectResult>",
                );
            }
            assert_eq!(request.method, "HEAD");
            let mut response = Response::new(200, "")
                .header(
                    "x-amz-checksum-type",
                    if case == 3 {
                        "COMPOSITE"
                    } else {
                        "FULL_OBJECT"
                    },
                )
                .header(
                    "x-amz-checksum-crc32",
                    if case == 1 { "AAAAAQ==" } else { "AAAAAA==" },
                );
            if case == 2 {
                response.declared_size = Some(1);
            }
            response
        })
        .await;
        let store = S3Store::for_test(&server.endpoint).unwrap();
        let mut source = ObjectMetadata::file(store.object("bucket", "source").unwrap(), 0);
        source.checksum = Some(Checksum {
            algorithm: ChecksumAlgorithm::Crc32,
            scope: ChecksumScope::FullObject,
            value: vec![0; 4],
        });
        let warnings = Warnings::default();
        let result = store
            .copier(DataClass::Standard)
            .copy(
                &source,
                &store.object("bucket", "dest").unwrap(),
                &warnings,
                &CancelContext::new(),
            )
            .await;
        if case == 1 || case == 2 {
            assert_eq!(result.err().unwrap().kind, ErrorKind::Integrity);
        } else {
            assert_eq!(
                result.unwrap().verification,
                if case == 3 {
                    Verification::SizeOnly
                } else {
                    Verification::SizeAndChecksum
                }
            );
            assert_eq!(
                warnings
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|w| w.category == WarningCategory::Verification)
                    .count(),
                usize::from(case == 3)
            );
        }
        assert_eq!(server.finish().await.len(), 2);
    }
}
