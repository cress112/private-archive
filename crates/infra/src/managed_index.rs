//! Shared frontend composition. SQLite interprets files; transport does not.
use crate::{S3Store, SqliteIndex, prepare_index_path};
use archive_domain::*;
use archive_index::*;
use archive_sync::*;
use std::{path::Path, time::Duration};

pub struct ManagedIndex {
    index: Box<dyn IndexDatabase>,
    files: Option<Box<dyn IndexFileSession>>,
    revision: u64,
    issues: Vec<OperationError>,
}
fn error(message: &str) -> OperationError {
    OperationError::new(ErrorKind::Io, "index-session", message)
}
impl ManagedIndex {
    /// Explicit update paths only. Local browsing opens the working path alone.
    /// Initialization issues are inspectable: data operations warn and continue;
    /// reconciliation requires all initialization steps to have succeeded.
    pub async fn open(
        home: &Path,
        location: &Location,
        store: &S3Store,
        cancel: &CancelContext,
    ) -> Result<Self, OperationError> {
        let path = prepare_index_path(home, location, store.namespace())?;
        let index = SqliteIndex::open(&path).await?;
        let mut result = Self {
            index: Box::new(index),
            files: None,
            revision: 0,
            issues: vec![],
        };
        if let Location::Remote(id) = location {
            let location = store.object(&id.authority, &id.key)?;
            let prepared = async {
                let files = store.index_files(&location, path.parent().unwrap().into())?;
                files.open(cancel).await
            }
            .await;
            match prepared {
                Err(e) => result.issues.push(e),
                Ok(mut files) => {
                    // Capture before the fetch so other processes can safely
                    // commit observations while network I/O is outstanding.
                    let fetched = async {
                        let mut import = result.index.begin_import().await?;
                        let control = import.control();
                        let signal = cancel.clone();
                        let bridge = tokio::spawn(async move {
                            signal.cancelled().await;
                            control.cancel();
                        });
                        let loaded = async {
                            let file = files.load(cancel).await?;
                            import.load(file.as_ref().map(|f| f.path().into())).await?;
                            Ok(())
                        }
                        .await;
                        if loaded.is_err() {
                            let _ = ancillary(
                                cancel,
                                Duration::from_secs(3),
                                Duration::from_secs(3),
                                import.cancel(),
                            )
                            .await;
                        }
                        bridge.abort();
                        let _ = bridge.await;
                        loaded
                    }
                    .await;
                    if let Err(e) = fetched {
                        result.issues.push(e);
                    }
                    result.files = Some(files);
                }
            }
        }
        result.revision = result.index.revision().await?;
        Ok(result)
    }
    pub fn index(&self) -> &dyn IndexWrite {
        self.index.as_ref()
    }
    pub fn issues(&self) -> &[OperationError] {
        &self.issues
    }
    /// At most one logical save, also after a failed/cancelled data operation.
    /// The supplied context bounds all ancillary work to the shared stop budget.
    pub async fn finish(mut self, cancel: &CancelContext) -> Vec<OperationError> {
        let mut issues = vec![];
        if let Some(files) = &mut self.files {
            let cleanup = CancelContext::new().with_deadline(
                cancel.ancillary_deadline(Duration::from_secs(900), Duration::from_secs(3)),
            );
            let saved = ancillary(
                cancel,
                Duration::from_secs(900),
                Duration::from_secs(3),
                async {
                    if self.index.revision().await? != self.revision {
                        if !self.issues.is_empty() {
                            return Err(error("index acquisition/import failed; save refused"));
                        }
                        let snapshot = self.index.snapshot().await?;
                        files.save(snapshot.as_ref(), &cleanup).await?;
                    }
                    Ok::<_, OperationError>(())
                },
            )
            .await;
            match saved {
                Some(Ok(())) => {}
                Some(Err(e)) => issues.push(e),
                None => issues.push(error("index save exceeded the stop budget")),
            }
            if let Some(Err(e)) = ancillary(
                cancel,
                Duration::from_secs(1),
                Duration::from_secs(3),
                files.close(),
            )
            .await
            {
                issues.push(e);
            }
        }
        match ancillary(
            cancel,
            Duration::from_secs(3),
            Duration::ZERO,
            self.index.close(),
        )
        .await
        {
            Some(Ok(())) => {}
            Some(Err(e)) => issues.push(e),
            None => issues.push(error("index worker did not finish")),
        }
        issues
    }
}
