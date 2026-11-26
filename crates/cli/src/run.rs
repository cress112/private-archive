use crate::{PreparedCommand, PreparedCopy, output::Console};
use archive_domain::*;
use archive_infra::{DiskPlan, LocalLoader, LocalSaver, ManagedIndex, S3Store};
use archive_sync::*;
use std::time::Duration;

pub async fn execute(prepared: PreparedCommand) -> u8 {
    match prepared {
        PreparedCommand::Copy(prepared) => execute_copy(prepared).await,
        PreparedCommand::Config(prepared) => crate::cfg::execute(prepared).await,
        PreparedCommand::List(prepared) => crate::ls::execute(prepared).await,
        PreparedCommand::Reconcile(prepared) => crate::reconcile::execute(prepared).await,
        PreparedCommand::Restore(prepared) => crate::restore::execute(prepared).await,
        PreparedCommand::Remove(prepared) => crate::remove::execute(prepared).await,
    }
}

async fn execute_copy(mut prepared: PreparedCopy) -> u8 {
    let session =
        match crate::session::Session::start("cp", prepared.settings.log_level.value).await {
            Ok(s) => s,
            Err(_) => return 1,
        };
    let result = transfer(&mut prepared, session.console.as_ref(), &session.cancel).await;
    session.finish(result).await
}
async fn transfer(
    prepared: &mut PreparedCopy,
    console: &Console,
    cancel: &CancelContext,
) -> Result<CopySummary, OperationError> {
    // Reject missing/unsupported local inputs before constructing a remote SDK
    // client or initializing the optional index.
    if matches!(prepared.selection.root, Location::Local(_)) {
        let _source = LocalLoader.stat(&prepared.selection.root, cancel).await?;
    }
    #[cfg(feature = "test-fixtures")]
    let store = if let Some(endpoint) = &prepared.test_endpoint {
        let store = S3Store::for_test(endpoint)?;
        fn convert(store: &S3Store, location: &Location) -> Result<Location, OperationError> {
            match location {
                Location::Remote(id) => store.object(&id.authority, &id.key),
                local => Ok(local.clone()),
            }
        }
        prepared.selection = Selection::new(
            convert(&store, &prepared.selection.root)?,
            prepared.selection.mode == SelectionMode::Descendants,
            prepared.selection.max_depth,
            &prepared.cwd,
        )?;
        prepared.destination = convert(&store, &prepared.destination)?;
        store
    } else {
        S3Store::from_env(cancel).await?
    };
    #[cfg(not(feature = "test-fixtures"))]
    let store = S3Store::from_env(cancel).await?;
    let opened = ManagedIndex::open(
        &prepared.home,
        &prepared.settings.index_location.value,
        &store,
        cancel,
    )
    .await;
    if let Ok(index) = &opened {
        for error in index.issues() {
            warn_index(
                console,
                cancel,
                error,
                &prepared.settings.index_location.value,
            )
            .await;
        }
    }
    let loader = store.loader();
    let saver = store.saver(prepared.settings.data_class.value);
    let copier = store.copier(prepared.settings.data_class.value);
    let index = opened
        .as_ref()
        .map(|index| index.index())
        .map_err(Clone::clone);
    let engine = CopyEngine {
        local_loader: &LocalLoader,
        local_saver: &LocalSaver,
        remote_loader: &loader,
        remote_saver: &saver,
        remote_copy: &copier,
        index,
    };
    let request = CopyRequest {
        protected: protected_index(&prepared.settings, &store),
        source: prepared.selection.clone(),
        destination: prepared.destination.clone(),
        cwd: prepared.cwd.clone(),
        ignore: prepared.settings.ignore.value.clone(),
    };
    let mut plan = match DiskPlan::new().await {
        Ok(plan) => plan,
        Err(e) => {
            cancel.record_failure();
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
            return Err(e);
        }
    };
    let _ = ancillary(
        cancel,
        Duration::from_millis(100),
        Duration::from_secs(3),
        console.diagnostic(&format!(
            "copying {:?} -> {:?}",
            request.source.root.display(),
            request.destination.display()
        )),
    )
    .await;
    let result = engine.copy(&request, &mut plan, console, cancel).await;
    drop(plan);
    drop(engine);
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

pub(crate) async fn warn_index(
    console: &Console,
    cancel: &CancelContext,
    error: &OperationError,
    location: &Location,
) {
    let _ = ancillary(
        cancel,
        Duration::from_millis(100),
        Duration::ZERO,
        console.warn(Warning {
            category: WarningCategory::Index,
            stage: error.stage,
            uri: location.display(),
            code: error.code.clone(),
            message: format!(
                "{error}; outcome={:?}; request_id={:?}; {}",
                error.outcome,
                error.context.request_id,
                error.context.causes.join("; ")
            ),
        }),
    )
    .await;
}

pub(crate) fn protected_index(
    settings: &archive_config::Settings,
    store: &S3Store,
) -> Option<ObjectId> {
    match &settings.index_location.value {
        Location::Remote(id) => {
            let mut id = id.clone();
            id.namespace = store.namespace().into();
            Some(id)
        }
        Location::Local(_) => None,
    }
}
