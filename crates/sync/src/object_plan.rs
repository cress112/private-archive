//! Temporary resolved targets, never the management index or a cross-run queue.
use crate::{CancelContext, PortFuture};
use archive_domain::{ObjectId, ObjectMetadata};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlannedObject {
    pub metadata: ObjectMetadata,
    pub full_observation: bool,
}
#[derive(Debug, Clone, Copy, Default)]
pub struct ObjectPlanSummary {
    pub objects: u64,
    pub total_bytes: Option<u64>,
}
pub trait ObjectPlan: Send + Sync {
    /// Returns false for duplicates. Keep the first URI's ordering, but retain
    /// the latest observation so deduplication cannot hide changed eligibility.
    fn append<'a>(
        &'a mut self,
        uri_order: u64,
        object: PlannedObject,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, bool>;
    fn seal<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, ObjectPlanSummary>;
    fn next<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, Option<PlannedObject>>;
    fn replace<'a>(
        &'a mut self,
        id: &'a ObjectId,
        object: PlannedObject,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ()>;
    fn rewind<'a>(&'a mut self, cancel: &'a CancelContext) -> PortFuture<'a, ()>;
    /// Wait for local teardown/shutdown within the caller's shared budget.
    /// Dropping this future or the plan must not begin an unbounded join;
    /// abandoned resources remain owned by their teardown worker.
    fn discard<'a>(&'a mut self) -> PortFuture<'a, ()>;
}
