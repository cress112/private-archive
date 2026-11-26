use crate::{PreparedRemove, output::Console, run::warn_index};
use archive_domain::*;
use archive_infra::{DiskObjects, ManagedIndex, S3Store};
use archive_sync::*;
use std::io::IsTerminal;

pub async fn execute(prepared: PreparedRemove) -> u8 {
    let session =
        match crate::session::Session::start("rm", prepared.settings.log_level.value).await {
            Ok(s) => s,
            Err(_) => return 1,
        };
    let result = run(&prepared, session.console.as_ref(), &session.cancel).await;
    session.finish(result).await
}

async fn run(
    prepared: &PreparedRemove,
    console: &Console,
    cancel: &CancelContext,
) -> Result<RemoveSummary, OperationError> {
    if !prepared.request.auto_approve && !std::io::stdin().is_terminal() {
        return Err(OperationError::new(
            ErrorKind::Io,
            "rm-input",
            "stdin must be a terminal; use --auto-approve to omit confirmation",
        ));
    }
    #[cfg(feature = "test-fixtures")]
    let store = match prepared.test_endpoint.as_deref() {
        Some(endpoint) => S3Store::for_test(endpoint)?,
        None => S3Store::from_env(cancel).await?,
    };
    #[cfg(not(feature = "test-fixtures"))]
    let store = S3Store::from_env(cancel).await?;
    let opened = ManagedIndex::open(
        &prepared.paths.home,
        &prepared.settings.index_location.value,
        &store,
        cancel,
    )
    .await;
    let mut request = prepared.request.clone();
    request.protected = crate::run::protected_index(&prepared.settings, &store);
    if let Ok(index) = &opened {
        for issue in index.issues() {
            warn_index(
                console,
                cancel,
                issue,
                &prepared.settings.index_location.value,
            )
            .await;
        }
    }
    let result = async {
        let mut plan = DiskObjects::new().await?;
        let loader = store.loader();
        let remote = store.remover();
        RemoveEngine {
            loader: &loader,
            remote: &remote,
            index: opened.as_ref().map(|i| i.index()).map_err(Clone::clone),
        }
        .remove(&request, &mut plan, console, cancel)
        .await
    }
    .await;
    if result.is_err() {
        cancel.record_failure();
    }
    if let Ok(index) = opened {
        for issue in index.finish(cancel).await {
            warn_index(
                console,
                cancel,
                &issue,
                &prepared.settings.index_location.value,
            )
            .await;
        }
    }
    result
}
