//! Database-independent local index contracts. No store requests are made here.
use archive_domain::*;
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    time::SystemTime,
};

pub type IndexFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, OperationError>> + Send + 'a>>;
pub const MAX_PAGE_ENTRIES: usize = 256;
pub const MAX_PAGE_BYTES: usize = 512 * 1024;

mod browse;
pub use browse::*;

#[derive(Debug, Clone)]
pub struct IndexScope {
    pub root: ObjectId,
    pub recursive: bool,
    pub max_depth: Option<std::num::NonZeroU32>,
    pub ignore: Vec<String>,
}
#[derive(Debug, Default)]
pub struct IndexPage {
    pub entries: Vec<ObjectMetadata>,
    /// Last examined raw key, including filtered rows. No OFFSET pagination.
    pub next: Option<Vec<u8>>,
}
pub struct RootPage {
    pub roots: Vec<ObjectId>,
    pub next: Option<ObjectId>,
}

/// Unobserved and observed-but-unknown have different merge semantics.
#[derive(Debug, Clone, Default)]
pub enum FieldPatch<T> {
    #[default]
    Unobserved,
    Observed(Option<T>),
}
#[derive(Debug, Clone)]
pub struct MetadataPatch {
    pub id: ObjectId,
    pub size: FieldPatch<u64>,
    pub checksum: FieldPatch<Checksum>,
    pub original_mtime: FieldPatch<SystemTime>,
    pub original_creation_time: FieldPatch<SystemTime>,
    pub unix_mode: FieldPatch<u32>,
    pub data_class: FieldPatch<StoredClass>,
    pub restore_state: FieldPatch<RestoreState>,
    pub store_modified: FieldPatch<SystemTime>,
    pub version_token: FieldPatch<String>,
    pub queried_at: SystemTime,
}
impl MetadataPatch {
    pub fn full(meta: ObjectMetadata) -> Result<Self, OperationError> {
        let Location::Remote(id) = meta.location else {
            return Err(OperationError::new(
                ErrorKind::Input,
                "index-patch",
                "index only stores remote objects",
            ));
        };
        if id.key.is_empty() || meta.kind != EntryKind::File {
            return Err(OperationError::new(
                ErrorKind::Input,
                "index-patch",
                "a remote file is required",
            ));
        }
        let queried_at = meta.queried_at.ok_or_else(|| {
            OperationError::new(
                ErrorKind::Input,
                "index-patch",
                "real observation time is required",
            )
        })?;
        Ok(Self {
            id,
            size: FieldPatch::Observed(meta.size),
            checksum: FieldPatch::Observed(meta.checksum),
            original_mtime: FieldPatch::Observed(meta.original_mtime),
            original_creation_time: FieldPatch::Observed(meta.original_creation_time),
            unix_mode: FieldPatch::Observed(meta.unix_mode),
            data_class: FieldPatch::Observed(meta.data_class),
            restore_state: FieldPatch::Observed(Some(meta.restore_state)),
            store_modified: FieldPatch::Observed(meta.store_modified),
            version_token: FieldPatch::Observed(meta.version_token),
            queried_at,
        })
    }
    pub fn listing(meta: ObjectMetadata) -> Result<Self, OperationError> {
        let mut patch = Self::full(meta)?;
        patch.checksum = FieldPatch::Unobserved;
        patch.original_mtime = FieldPatch::Unobserved;
        patch.original_creation_time = FieldPatch::Unobserved;
        patch.unix_mode = FieldPatch::Unobserved;
        Ok(patch)
    }
    pub fn merge(self, old: Option<ObjectMetadata>) -> ObjectMetadata {
        fn known<T>(value: &FieldPatch<T>) -> Option<&T> {
            match value {
                FieldPatch::Observed(Some(v)) => Some(v),
                _ => None,
            }
        }
        let same_version = old.as_ref().is_some_and(|old| {
            known(&self.size).is_some_and(|n| Some(*n) == old.size)
                && (known(&self.version_token)
                    .is_some_and(|v| Some(v) == old.version_token.as_ref())
                    || known(&self.version_token).is_none()
                        && old.version_token.is_none()
                        && known(&self.store_modified)
                            .is_some_and(|v| Some(*v) == old.store_modified))
        });
        let mut merged = old
            .filter(|_| same_version)
            .unwrap_or_else(|| ObjectMetadata::file(Location::Remote(self.id.clone()), 0));
        if !same_version {
            merged.size = None;
        }
        macro_rules! apply {
            ($field:ident) => {
                if let FieldPatch::Observed(value) = self.$field {
                    merged.$field = value;
                }
            };
        }
        apply!(size);
        apply!(checksum);
        apply!(original_mtime);
        apply!(original_creation_time);
        apply!(unix_mode);
        apply!(data_class);
        apply!(store_modified);
        apply!(version_token);
        if let FieldPatch::Observed(value) = self.restore_state {
            merged.restore_state = value.unwrap_or(RestoreState::Unknown);
        }
        merged.location = Location::Remote(self.id);
        merged.queried_at = Some(self.queried_at);
        merged
    }
}

pub trait IndexRead: Send + Sync {
    fn get<'a>(&'a self, id: &'a ObjectId) -> IndexFuture<'a, Option<ObjectMetadata>>;
    fn query<'a>(
        &'a self,
        scope: &'a IndexScope,
        cursor: Option<Vec<u8>>,
        limit: usize,
    ) -> IndexFuture<'a, IndexPage>;
    fn roots<'a>(&'a self, cursor: Option<ObjectId>, limit: usize) -> IndexFuture<'a, RootPage>;
}
pub trait IndexWrite: Send + Sync {
    fn apply<'a>(&'a self, patch: MetadataPatch) -> IndexFuture<'a, ()>;
    fn remove<'a>(&'a self, id: &'a ObjectId) -> IndexFuture<'a, ()>;
    fn begin_reconcile<'a>(&'a self, scope: IndexScope) -> IndexFuture<'a, Box<dyn Reconcile>>;
}
/// May be signalled from another task while a reconciliation finish is running.
#[derive(Clone, Default)]
pub struct ReconcileControl(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl ReconcileControl {
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn check(&self) -> Result<(), OperationError> {
        if self.0.load(std::sync::atomic::Ordering::Acquire) {
            Err(OperationError::new(
                ErrorKind::Cancelled,
                "index-reconcile",
                "reconciliation cancelled",
            ))
        } else {
            Ok(())
        }
    }
}
pub trait Reconcile: Send + Sync {
    fn control(&self) -> ReconcileControl;
    fn observe<'a>(&'a mut self, patch: MetadataPatch) -> IndexFuture<'a, ()>;
    /// Call only after the entire requested range has been fetched successfully.
    fn finish<'a>(&'a mut self) -> IndexFuture<'a, u64>;
    fn cancel<'a>(&'a mut self) -> IndexFuture<'a, ()>;
}
pub trait SnapshotFile: Send + Sync {
    fn path(&self) -> &Path;
}
pub trait IndexSnapshot: Send + Sync {
    /// Dropping this future must request cancellation of queued/running work;
    /// the database worker must not outlive the caller's stop budget doing an
    /// abandoned snapshot. The returned file has its own independent lifetime.
    fn snapshot<'a>(&'a self) -> IndexFuture<'a, Box<dyn SnapshotFile>>;
    /// Validate an incoming file without replacing an open working database.
    fn inspect_snapshot<'a>(&'a self, path: PathBuf) -> IndexFuture<'a, ()>;
    /// Capture the working revision before fetching an incoming file.
    fn begin_import<'a>(&'a self) -> IndexFuture<'a, Box<dyn SnapshotImport>>;
}

#[derive(Clone, Default)]
pub struct ImportControl(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl ImportControl {
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn check(&self) -> Result<(), OperationError> {
        if self.0.load(std::sync::atomic::Ordering::Acquire) {
            Err(OperationError::new(
                ErrorKind::Cancelled,
                "index-import",
                "import cancelled",
            ))
        } else {
            Ok(())
        }
    }
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub imported: u64,
    pub removed: u64,
}
pub trait SnapshotImport: Send + Sync {
    fn control(&self) -> ImportControl;
    /// Validate, apply bounded pages, then remove absent old rows only on success.
    /// Incoming values (including unknowns and query times) are copied exactly.
    /// None is a confirmed missing file, equivalent to an empty incoming copy.
    fn load<'a>(&'a mut self, path: Option<PathBuf>) -> IndexFuture<'a, ImportSummary>;
    /// Drop temporary state; confirmed page commits remain.
    fn cancel<'a>(&'a mut self) -> IndexFuture<'a, ()>;
}

/// Lifecycle/change tracking for frontend composition, independent of DB format.
pub trait IndexDatabase: IndexWrite + IndexSnapshot {
    fn revision<'a>(&'a self) -> IndexFuture<'a, u64>;
    fn close<'a>(&'a self) -> IndexFuture<'a, ()>;
}
