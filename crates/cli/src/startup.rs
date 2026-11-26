use crate::{
    PreparationError,
    session::{Session, cancellable_output},
};
use archive_domain::*;
use archive_sync::ancillary;
use std::time::Duration;

pub async fn display_parse_error(error: clap::Error) -> u8 {
    let session = match Session::start("arguments", LogLevel::Info).await {
        Ok(s) => s,
        Err(_) => return 1,
    };
    let code = error.exit_code() as u8;
    let text = error.to_string();
    let result = if error.use_stderr() {
        if code != 0 {
            session.cancel.record_failure();
        }
        let _ = ancillary(
            &session.cancel,
            Duration::from_millis(100),
            Duration::ZERO,
            session.console.raw_stderr(&text),
        )
        .await;
        Ok(())
    } else {
        cancellable_output(session.console.raw(&text), &session.cancel).await
    };
    session.finish_with_code(result, code).await
}
pub async fn display_preparation_error(error: PreparationError) -> u8 {
    let session = match Session::start("startup", LogLevel::Info).await {
        Ok(s) => s,
        Err(_) => return 1,
    };
    let kind = if error.exit_code == 2 {
        ErrorKind::Input
    } else {
        ErrorKind::Io
    };
    session
        .finish(Err::<(), _>(OperationError::new(
            kind,
            "startup",
            error.message,
        )))
        .await
}
