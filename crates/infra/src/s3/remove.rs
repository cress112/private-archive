use super::{S3Store, sdk_error, upload::mutate};
use archive_domain::*;
use archive_sync::*;
pub struct S3Remover {
    pub(super) store: S3Store,
}
impl RemoteRemove for S3Remover {
    fn remove<'a>(&'a self, id: &'a ObjectId, cancel: &'a CancelContext) -> PortFuture<'a, ()> {
        Box::pin(async move {
            let location = Location::Remote(id.clone());
            let id = self.store.id(&location, true)?;
            mutate(
                async {
                    self.store
                        .client
                        .delete_object()
                        .bucket(&id.authority)
                        .key(&id.key)
                        .send()
                        .await
                        .map_err(|e| sdk_error("s3-delete", &location, e, true))
                },
                cancel,
                &location,
            )
            .await?;
            Ok(())
        })
    }
}
