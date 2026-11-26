use archive_domain::*;
use archive_sync::*;
use std::time::Duration;

/// One record, with deterministic delayed disposal for virtual-time budget tests.
pub struct DelayedPlan {
    object: Option<PlannedObject>,
    yielded: bool,
    delay: Duration,
    pub cleaned: bool,
}
impl DelayedPlan {
    pub fn new(delay: Duration) -> Self {
        Self {
            object: None,
            yielded: false,
            delay,
            cleaned: false,
        }
    }
}
impl ObjectPlan for DelayedPlan {
    fn append<'a>(
        &'a mut self,
        _: u64,
        object: PlannedObject,
        _: &'a CancelContext,
    ) -> PortFuture<'a, bool> {
        Box::pin(async move {
            assert!(self.object.is_none());
            self.object = Some(object);
            Ok(true)
        })
    }
    fn seal<'a>(&'a mut self, _: &'a CancelContext) -> PortFuture<'a, ObjectPlanSummary> {
        Box::pin(async move {
            Ok(ObjectPlanSummary {
                objects: u64::from(self.object.is_some()),
                total_bytes: self.object.as_ref().and_then(|o| o.metadata.size),
            })
        })
    }
    fn next<'a>(&'a mut self, _: &'a CancelContext) -> PortFuture<'a, Option<PlannedObject>> {
        Box::pin(async move {
            if self.yielded {
                return Ok(None);
            }
            self.yielded = true;
            Ok(self.object.clone())
        })
    }
    fn replace<'a>(
        &'a mut self,
        _: &'a ObjectId,
        object: PlannedObject,
        _: &'a CancelContext,
    ) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.object = Some(object);
            Ok(())
        })
    }
    fn rewind<'a>(&'a mut self, _: &'a CancelContext) -> PortFuture<'a, ()> {
        Box::pin(async move {
            self.yielded = false;
            Ok(())
        })
    }
    fn discard<'a>(&'a mut self) -> PortFuture<'a, ()> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            self.cleaned = true;
            Ok(())
        })
    }
}
