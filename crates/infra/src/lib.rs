//! Concrete adapters. Store SDK and database types remain private to this crate.
mod checksum;
mod local;
mod s3;
pub use local::{LocalLoader, LocalSaver};
pub use s3::{
    CopyPlan, S3Loader, S3RemoteCopy, S3Remover, S3Restorer, S3Saver, S3Store, UploadPlan,
};

mod sqlite;
pub use sqlite::SqliteIndex;
mod index_location;
pub use index_location::{index_working_path, prepare_index_path};
mod managed_index;
pub use managed_index::ManagedIndex;

mod manifest;
pub use manifest::DiskPlan;
mod object_plan;
pub use object_plan::DiskObjects;

mod attributes;

mod parts;
