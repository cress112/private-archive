//! One signal/exit/diagnostic/fd lifetime for every CLI command.
use crate::output::Console;
use archive_domain::*;
use archive_sync::{CancelContext, ancillary};
use std::{sync::Arc, time::Duration};

pub(crate) struct Session {
    pub console: Arc<Console>,
    pub cancel: CancelContext,
    signals: tokio::task::JoinHandle<()>,
    command: &'static str,
}
impl Session {
    pub async fn start(command: &'static str, level: LogLevel) -> Result<Self, OperationError> {
        let console = Arc::new(Console::new(level)?);
        // Register synchronously before starting command I/O.
        let mut interrupts =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .map_err(|e| OperationError::new(ErrorKind::Io, "cli-signal", e.to_string()))?;
        let cancel = CancelContext::new();
        let signal = cancel.clone();
        let display = console.clone();
        let signals = tokio::spawn(async move {
            if interrupts.recv().await.is_some() {
                signal.cancel();
            }
            if interrupts.recv().await.is_some() {
                display.restore_flags();
                std::process::exit(130);
            }
        });
        let session = Self {
            console,
            cancel,
            signals,
            command,
        };
        let _ = ancillary(
            &session.cancel,
            Duration::from_millis(100),
            Duration::from_secs(3),
            session
                .console
                .log(LogLevel::Trace, &format!("trace: {command} start")),
        )
        .await;
        Ok(session)
    }
    pub async fn finish<T>(self, result: Result<T, OperationError>) -> u8 {
        self.finish_with_code(result, 0).await
    }
    pub async fn finish_with_code<T>(
        mut self,
        result: Result<T, OperationError>,
        printed_code: u8,
    ) -> u8 {
        if result.is_err() {
            self.cancel.record_failure();
        }
        if let Err(e) = &result {
            let _ = ancillary(
                &self.cancel,
                Duration::from_millis(100),
                Duration::ZERO,
                self.console.error(e),
            )
            .await;
        }
        let _ = ancillary(
            &self.cancel,
            Duration::from_millis(100),
            Duration::ZERO,
            self.console.log(
                LogLevel::Trace,
                &format!("trace: {} finished", self.command),
            ),
        )
        .await;
        self.signals.abort();
        let _ = (&mut self.signals).await;
        let code = status(result.as_ref().err(), &self.cancel);
        self.console.restore_flags();
        if code == 0 { printed_code } else { code }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.signals.abort();
        self.console.restore_flags();
    }
}
fn status(error: Option<&OperationError>, cancel: &CancelContext) -> u8 {
    let cancelled_first = cancel
        .cancelled_at()
        .is_some_and(|t| cancel.stopped_at() == Some(t));
    if cancelled_first {
        return 130;
    }
    match error {
        None => 0,
        Some(e) if e.kind == ErrorKind::Input => 2,
        Some(e) if e.kind == ErrorKind::Cancelled && cancel.stopped_at().is_none() => 130,
        Some(_) => 1,
    }
}
/// Plain text/config output may be interrupted immediately. JSON elements use
/// output_work to finish the current record within the shared stop budget.
pub(crate) async fn cancellable_output<T>(
    future: impl std::future::Future<Output = Result<T, OperationError>>,
    cancel: &CancelContext,
) -> Result<T, OperationError> {
    tokio::select! {biased;
        result=future=>result,
        _=cancel.cancelled()=>{cancel.check("cli-output")?;unreachable!()},
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_stop_cause_is_shared_by_all_commands_without_changing_unknown_outcome() {
        let mut error = OperationError::new(ErrorKind::Unavailable, "mutation", "response lost");
        error.outcome = OperationState::Unknown;
        let cancelled = CancelContext::new();
        cancelled.cancel();
        assert_eq!(status(Some(&error), &cancelled), 130);
        assert_eq!(status(None, &cancelled), 130);
        assert_eq!(error.outcome, OperationState::Unknown);
        let failed = CancelContext::new();
        failed.record_failure();
        std::thread::sleep(Duration::from_millis(1));
        failed.cancel();
        assert_eq!(status(Some(&error), &failed), 1);
        assert_eq!(status(None, &CancelContext::new()), 0);
        assert_eq!(status(Some(&error), &CancelContext::new()), 1);
        assert_eq!(
            status(
                Some(&OperationError::new(ErrorKind::Input, "input", "bad")),
                &CancelContext::new()
            ),
            2
        );
    }
}
