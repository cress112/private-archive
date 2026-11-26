use crate::{PreparedReconcile, output::Console};
use archive_domain::*;
use archive_infra::{ManagedIndex, S3Store};
use archive_sync::*;

pub async fn execute(prepared: PreparedReconcile) -> u8 {
    let session = match crate::session::Session::start(
        "reconcile",
        prepared.settings.log_level.value,
    )
    .await
    {
        Ok(s) => s,
        Err(_) => return 1,
    };
    let result = run(&prepared, session.console.as_ref(), &session.cancel).await;
    session.finish(result).await
}

async fn run(
    prepared: &PreparedReconcile,
    console: &Console,
    cancel: &CancelContext,
) -> Result<(), OperationError> {
    cancel.check("reconcile")?;
    #[cfg(feature = "test-fixtures")]
    let store = match &prepared.test_endpoint {
        Some(endpoint) => S3Store::for_test(endpoint)?,
        None => S3Store::from_env(cancel).await?,
    };
    #[cfg(not(feature = "test-fixtures"))]
    let store = S3Store::from_env(cancel).await?;
    let index = ManagedIndex::open(
        &prepared.paths.home,
        &prepared.settings.index_location.value,
        &store,
        cancel,
    )
    .await?;
    let result = async {
        if let Some(issue) = index.issues().first() {
            return Err(issue.clone());
        }
        let loader = store.loader();
        ReconcileEngine {
            loader: &loader,
            index: index.index(),
        }
        .reconcile(&prepared.request, console, cancel)
        .await
    }
    .await;
    if result.is_err() {
        cancel.record_failure();
    }
    let mut issues = index.finish(cancel).await;
    if let Err(error) = result {
        for issue in issues {
            crate::run::warn_index(
                console,
                cancel,
                &issue,
                &prepared.settings.index_location.value,
            )
            .await;
        }
        return Err(error);
    }
    let summary = result?;
    if !issues.is_empty() {
        let first = issues.remove(0);
        for issue in issues {
            crate::run::warn_index(
                console,
                cancel,
                &issue,
                &prepared.settings.index_location.value,
            )
            .await;
        }
        return Err(first);
    }
    crate::session::cancellable_output(
        console.result(&format!(
            "reconcile observed={} removed={} ignored={}",
            summary.observed, summary.removed, summary.ignored
        )),
        cancel,
    )
    .await
}
