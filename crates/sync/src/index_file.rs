//! File transport is separate from database interpretation and data operations.
use crate::{CancelContext, PortFuture};
use archive_index::SnapshotFile;

pub trait IndexFileStore: Send + Sync {
    fn open<'a>(&'a self, cancel: &'a CancelContext) -> PortFuture<'a, Box<dyn IndexFileSession>>;
}
pub trait IndexFileSession: Send + Sync {
    /// Called once per run. Missing is distinct from failed/unknown.
    fn load<'a>(
        &'a mut self,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, Option<Box<dyn SnapshotFile>>>;
    /// One conditional save against this session's observation. No retry/merge.
    fn save<'a>(
        &'a mut self,
        snapshot: &'a dyn SnapshotFile,
        cancel: &'a CancelContext,
    ) -> PortFuture<'a, ()>;
    fn close<'a>(&'a mut self) -> PortFuture<'a, ()>;
}
