use archive_domain::*;
use archive_sync::*;
use std::{
    fs::File,
    io::{self, IsTerminal, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd},
    os::unix::fs::FileTypeExt,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::{io::unix::AsyncFd, sync::Mutex};
struct Descriptor {
    file: File,
    flags: i32,
}
impl Descriptor {
    fn duplicate(fd: i32) -> io::Result<Self> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if copy < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            file: unsafe { File::from_raw_fd(copy) },
            flags,
        })
    }
}
impl AsFd for Descriptor {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}
impl AsRawFd for Descriptor {
    fn as_raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
}
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe { libc::fcntl(self.as_raw_fd(), libc::F_SETFL, self.flags) };
    }
}
enum Output {
    Regular(Descriptor),
    Async(AsyncFd<Descriptor>),
}
impl Output {
    fn new(fd: Descriptor) -> io::Result<Self> {
        let metadata = fd.file.metadata()?;
        // Non-terminal character sinks such as /dev/null are not pollable by
        // epoll/kqueue. Keep terminals and pipes on the cancellable async path.
        if metadata.is_file() || (metadata.file_type().is_char_device() && !fd.file.is_terminal()) {
            return Ok(Self::Regular(fd));
        }
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fd.flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Descriptor owns this duplicated fd, always returns that fd,
        // and closes it only when AsyncFd drops its owned Descriptor.
        Ok(Self::Async(unsafe {
            AsyncFd::register_with_interest(fd, tokio::io::Interest::WRITABLE)?
        }))
    }
    async fn line(&mut self, text: &str) -> io::Result<()> {
        let mut end = text.len().min(16 * 1024 - 32);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let truncated = end < text.len();
        let mut text = text[..end].to_owned();
        if truncated {
            text.push_str(" [truncated]");
        }
        text.push('\n');
        self.write(text.as_bytes()).await
    }
    async fn write(&mut self, text: &[u8]) -> io::Result<()> {
        match self {
            Self::Regular(fd) => fd.file.write_all(text),
            Self::Async(fd) => {
                let mut bytes = text;
                while !bytes.is_empty() {
                    let mut ready = fd.writable().await?;
                    match ready.try_io(|fd| {
                        let n = unsafe {
                            libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len())
                        };
                        if n < 0 {
                            Err(io::Error::last_os_error())
                        } else {
                            Ok(n as usize)
                        }
                    }) {
                        Ok(Ok(0)) => {
                            return Err(io::Error::new(io::ErrorKind::WriteZero, "output closed"));
                        }
                        Ok(Ok(n)) => bytes = &bytes[n..],
                        Ok(Err(e)) => return Err(e),
                        Err(_) => continue,
                    }
                }
                Ok(())
            }
        }
    }
}
pub struct Console {
    stdout: Mutex<Output>,
    stderr: Mutex<Output>,
    flags: (i32, i32, Option<i32>),
    files: AtomicU64,
    bytes: AtomicU64,
    log_level: LogLevel,
    stderr_terminal: bool,
}
fn output_error(error: io::Error) -> OperationError {
    OperationError::new(ErrorKind::Io, "cli-output", error.to_string())
}
impl Console {
    pub fn new(log_level: LogLevel) -> Result<Self, OperationError> {
        // Capture both original flags before modifying either descriptor (2>&1
        // can share an open file description).
        let input_flags = unsafe { libc::fcntl(0, libc::F_GETFL) };
        let out = Descriptor::duplicate(1).map_err(output_error)?;
        let err = Descriptor::duplicate(2).map_err(output_error)?;
        let flags = (
            out.flags,
            err.flags,
            (input_flags >= 0).then_some(input_flags),
        );
        Ok(Self {
            stdout: Mutex::new(Output::new(out).map_err(output_error)?),
            stderr: Mutex::new(Output::new(err).map_err(output_error)?),
            flags,
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            log_level,
            stderr_terminal: std::io::stderr().is_terminal(),
        })
    }
    pub fn restore_flags(&self) {
        unsafe {
            libc::fcntl(1, libc::F_SETFL, self.flags.0);
            libc::fcntl(2, libc::F_SETFL, self.flags.1);
            if let Some(flags) = self.flags.2 {
                libc::fcntl(0, libc::F_SETFL, flags);
            }
        }
    }
    pub async fn diagnostic(&self, text: &str) -> Result<(), OperationError> {
        self.log(LogLevel::Info, text).await
    }
    pub async fn result(&self, text: &str) -> Result<(), OperationError> {
        let mut out = self.stdout.lock().await;
        out.write(text.as_bytes()).await.map_err(output_error)?;
        out.write(b"\n").await.map_err(output_error)
    }
    pub async fn raw(&self, text: &str) -> Result<(), OperationError> {
        self.stdout
            .lock()
            .await
            .write(text.as_bytes())
            .await
            .map_err(output_error)
    }
    pub async fn raw_stderr(&self, text: &str) -> Result<(), OperationError> {
        self.stderr
            .lock()
            .await
            .write(text.as_bytes())
            .await
            .map_err(output_error)
    }
    pub async fn log(&self, level: LogLevel, text: &str) -> Result<(), OperationError> {
        if !self.log_level.allows(level) {
            return Ok(());
        }
        self.stderr
            .lock()
            .await
            .line(text)
            .await
            .map_err(output_error)
    }
    pub async fn error(&self, error: &OperationError) {
        let message = format!(
            "error: {error}; outcome={:?}; source={:?}; destination={:?}; code={:?}; request_id={:?}; {}",
            error.outcome,
            error.context.source_uri,
            error.context.destination_uri,
            error.code,
            error.context.request_id,
            error.context.causes.join("; ")
        );
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            self.log(LogLevel::Error, &message),
        )
        .await;
    }
}
impl WarningSink for Console {
    fn warn<'a>(&'a self, warning: Warning) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.log(
                LogLevel::Warn,
                &format!(
                    "warning [{:?}] {}: {:?}: {}",
                    warning.category, warning.stage, warning.uri, warning.message
                ),
            )
            .await
        })
    }
}
impl OperationObserver for Console {
    fn ignored<'a>(&'a self, warning: Warning, direct: bool) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.log(
                if direct {
                    LogLevel::Warn
                } else {
                    LogLevel::Debug
                },
                &format!("ignored {:?}", warning.uri),
            )
            .await
        })
    }
    fn ignored_summary<'a>(&'a self, observed: u64) -> PortFuture<'a, ()> {
        Box::pin(async move {
            if observed > 0 {
                self.log(
                    LogLevel::Info,
                    &format!("ignore: {observed} observed exclusions"),
                )
                .await?;
            }
            Ok(())
        })
    }
    fn copied<'a>(&'a self, outcome: CopyOutcome) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.stdout
                .lock()
                .await
                .line(&format!(
                    "copy {:?} -> {:?} ({} bytes)",
                    outcome.source.display(),
                    outcome.destination.display(),
                    outcome.bytes
                ))
                .await
                .map_err(output_error)?;
            let files = self.files.fetch_add(1, Ordering::Relaxed) + 1;
            let bytes = self.bytes.fetch_add(outcome.bytes, Ordering::Relaxed) + outcome.bytes;
            if self.stderr_terminal && self.log_level.allows(LogLevel::Info) {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    self.diagnostic(&format!("progress: {files} files, {bytes} bytes completed")),
                )
                .await;
            }
            Ok(())
        })
    }
}

impl RestoreObserver for Console {
    fn ignored<'a>(&'a self, warning: Warning, direct: bool) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.log(
                if direct {
                    LogLevel::Warn
                } else {
                    LogLevel::Debug
                },
                &format!("ignored {:?}", warning.uri),
            )
            .await
        })
    }
    fn accepted<'a>(
        &'a self,
        location: Location,
        days: RestoreDays,
        tier: RestoreTier,
    ) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.result(&format!(
                "restore accepted {:?} days={} tier={tier:?}",
                location.display(),
                days.get()
            ))
            .await
        })
    }
    fn finished<'a>(&'a self, summary: RestoreSummary) -> PortFuture<'a, ()> {
        Box::pin(async move {
            if summary.ignored > 0 {
                self.log(
                    LogLevel::Info,
                    &format!("ignore: {} observed exclusions", summary.ignored),
                )
                .await?;
            }
            Ok(())
        })
    }
}

impl RemoveObserver for Console {
    fn ignored<'a>(&'a self, warning: Warning, direct: bool) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.log(
                if direct {
                    LogLevel::Warn
                } else {
                    LogLevel::Debug
                },
                &format!("ignored {:?}", warning.uri),
            )
            .await
        })
    }
    fn confirm<'a>(
        &'a self,
        root: Location,
        summary: ObjectPlanSummary,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, bool> {
        Box::pin(async move {
            let prompt = format!(
                "remove {:?}: {} objects, {} bytes. Type yes to continue:",
                root.display(),
                summary.objects,
                summary
                    .total_bytes
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unknown".into())
            );
            tokio::select! {
                _=cancel.cancelled()=>{cancel.check("rm-confirm")?;unreachable!()},
                result=async {self.stderr.lock().await.line(&prompt).await.map_err(output_error)}=>result?,
            }
            let fd = Descriptor::duplicate(0).map_err(output_error)?;
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fd.flags | libc::O_NONBLOCK) }
                < 0
            {
                return Err(output_error(io::Error::last_os_error()));
            }
            let fd = unsafe { AsyncFd::register_with_interest(fd, tokio::io::Interest::READABLE) }
                .map_err(|e| output_error(e.into()))?;
            let mut answer = Vec::with_capacity(4);
            loop {
                let mut ready = tokio::select! {
                    _=cancel.cancelled()=>{cancel.check("rm-confirm")?;unreachable!()},
                    result=fd.readable()=>result.map_err(output_error)?,
                };
                match ready.try_io(|fd| {
                    let mut byte = [0u8; 1];
                    let n = unsafe { libc::read(fd.as_raw_fd(), byte.as_mut_ptr().cast(), 1) };
                    if n < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok((n, byte[0]))
                    }
                }) {
                    Ok(Ok((0, _))) => return Ok(false),
                    Ok(Ok((_, b'\n' | b'\r'))) => return Ok(answer == b"yes"),
                    Ok(Ok((_, byte))) => {
                        // Consume exactly one complete line even when invalid,
                        // keeping a bounded prefix instead of a growing input.
                        if answer.len() < 4 {
                            answer.push(byte);
                        }
                    }
                    Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Ok(Err(e)) => return Err(output_error(e)),
                    Err(_) => continue,
                }
            }
        })
    }
    fn removed<'a>(&'a self, location: Location) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.result(&format!("remove {:?}", location.display()))
                .await
        })
    }
    fn finished<'a>(&'a self, summary: RemoveSummary) -> PortFuture<'a, ()> {
        Box::pin(async move {
            if summary.ignored > 0 {
                self.log(
                    LogLevel::Info,
                    &format!("ignore: {} observed exclusions", summary.ignored),
                )
                .await?;
            }
            Ok(())
        })
    }
}

impl ReconcileObserver for Console {
    fn ignored<'a>(&'a self, warning: Warning, direct: bool) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.log(
                if direct {
                    LogLevel::Warn
                } else {
                    LogLevel::Debug
                },
                &format!("ignored {:?}: {}", warning.uri, warning.message),
            )
            .await
        })
    }
    fn progress<'a>(&'a self, current: Location, summary: ReconcileSummary) -> PortFuture<'a, ()> {
        Box::pin(async move {
            if self.stderr_terminal
                && self.log_level.allows(LogLevel::Info)
                && (summary.observed == 1 || summary.observed.is_multiple_of(1000))
            {
                self.diagnostic(&format!(
                    "reconcile: observed={}, ignored={}, current={:?}",
                    summary.observed,
                    summary.ignored,
                    current.display()
                ))
                .await?;
            }
            Ok(())
        })
    }
    fn finished<'a>(&'a self, summary: ReconcileSummary, _succeeded: bool) -> PortFuture<'a, ()> {
        Box::pin(async move {
            if summary.ignored > 0 {
                self.log(
                    LogLevel::Info,
                    &format!("ignore: {} observed exclusions", summary.ignored),
                )
                .await?;
            }
            Ok(())
        })
    }
}

pub(crate) async fn output_work<T>(
    future: impl std::future::Future<Output = Result<T, OperationError>>,
    cancel: &CancelContext,
) -> Result<T, OperationError> {
    tokio::pin!(future);
    tokio::select! {biased;
        result=&mut future=>result,
        _=cancel.cancelled()=>tokio::time::timeout_at(cancel.cleanup_deadline()-std::time::Duration::from_secs(3),&mut future).await.unwrap_or_else(|_|Err(OperationError::new(ErrorKind::Cancelled,"cli-output","output did not finish within cancellation budget"))),
    }
}
