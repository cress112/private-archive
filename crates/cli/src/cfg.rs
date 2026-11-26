use crate::session::cancellable_output;
use crate::{ConfigCommand, PreparedConfig, output::Console};
use archive_config::{Change, ConfigKey, ConfigValue};
use archive_domain::{ErrorKind, OperationError};
use std::time::Duration;

pub async fn execute(prepared: PreparedConfig) -> u8 {
    let session =
        match crate::session::Session::start("cfg", prepared.settings.log_level.value).await {
            Ok(s) => s,
            Err(_) => return 1,
        };
    let result = run(prepared, session.console.as_ref(), &session.cancel).await;
    session.finish(result).await
}

async fn run(
    prepared: PreparedConfig,
    console: &Console,
    cancel: &archive_sync::CancelContext,
) -> Result<(), OperationError> {
    cancel.check("cfg")?;
    let PreparedConfig {
        paths,
        settings,
        command,
    } = prepared;
    let command = match command {
        None => {
            for key in ConfigKey::ALL {
                let (value, origin) = settings.setting(key);
                let line = format!("{} = {} ({})", key.as_str(), value, origin.as_str());
                cancellable_output(console.result(&line), cancel).await?;
            }
            return Ok(());
        }
        Some(ConfigCommand::Get { key }) => {
            return cancellable_output(console.result(&settings.setting(key).0), cancel).await;
        }
        Some(command) => command,
    };
    let path = settings.config_path;
    let signal = cancel.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        signal.check("cfg")?;
        let result = (|| match command {
            ConfigCommand::Init => archive_config::init(&paths, Some(&path))
                .map(|path| format!("created {}", path.display())),
            ConfigCommand::Set { key, value } => {
                let value = ConfigValue::parse(key, &value)?;
                archive_config::apply_changes(&paths, Some(&path), &[Change::Set(value)])?;
                Ok(format!("set {}", key.as_str()))
            }
            ConfigCommand::Unset { key } => {
                archive_config::apply_changes(&paths, Some(&path), &[Change::Unset(key)])?;
                Ok(format!("unset {}", key.as_str()))
            }
            ConfigCommand::Get { .. } => unreachable!(),
        })()
        .map_err(config_error);
        if result.is_err() {
            signal.record_failure();
        }
        result
    });
    let notice=tokio::select! {
        result=&mut worker=>result,
        _=cancel.cancelled()=>tokio::time::timeout_at(cancel.cleanup_deadline()-Duration::from_secs(3),&mut worker).await.map_err(|_| {
            let mut e=OperationError::new(ErrorKind::Cancelled,"cfg-worker","configuration worker did not finish within cancellation budget; the change may have completed");
            e.outcome=archive_domain::OperationState::Unknown;e
        })?,
    }.map_err(|e| OperationError::new(ErrorKind::Io,"cfg-worker",e.to_string()))??;
    cancellable_output(console.result(&notice), cancel).await
}

fn config_error(e: archive_config::ConfigError) -> OperationError {
    OperationError::new(
        if e.is_invalid_input() {
            ErrorKind::Input
        } else {
            ErrorKind::Io
        },
        "cfg",
        e.to_string(),
    )
}
