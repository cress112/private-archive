use crate::Location;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Special,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum StoredClass {
    Standard,
    Archived,
    Other(String),
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum RestoreState {
    NotRequired,
    Archived,
    Restoring,
    Restored {
        #[serde(with = "optional_time")]
        until: Option<std::time::SystemTime>,
    },
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ChecksumAlgorithm {
    Crc32,
    Crc32c,
    Crc64Nvme,
    Sha1,
    Sha256,
    Other(String),
}
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumScope {
    FullObject,
    Composite,
    Unknown,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    pub algorithm: ChecksumAlgorithm,
    pub scope: ChecksumScope,
    /// Digest bytes, in network byte order for CRC checksums (not an ETag).
    pub value: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ObjectMetadata {
    pub location: Location,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub checksum: Option<Checksum>,
    #[serde(with = "optional_time")]
    pub original_mtime: Option<std::time::SystemTime>,
    #[serde(with = "optional_time")]
    pub original_creation_time: Option<std::time::SystemTime>,
    pub unix_mode: Option<u32>,
    pub data_class: Option<StoredClass>,
    pub restore_state: RestoreState,
    #[serde(with = "optional_time")]
    pub queried_at: Option<std::time::SystemTime>,
    #[serde(with = "optional_time")]
    pub store_modified: Option<std::time::SystemTime>,
    pub version_token: Option<String>,
}

impl ObjectMetadata {
    pub fn file(location: Location, size: u64) -> Self {
        Self {
            location,
            kind: EntryKind::File,
            size: Some(size),
            checksum: None,
            original_mtime: None,
            original_creation_time: None,
            unix_mode: None,
            data_class: None,
            restore_state: RestoreState::Unknown,
            queried_at: None,
            store_modified: None,
            version_token: None,
        }
    }
}

mod optional_time {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    pub fn serialize<S: Serializer>(
        time: &Option<SystemTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let wire = time.map(|time| match time.duration_since(UNIX_EPOCH) {
            Ok(d) => (false, d.as_secs(), d.subsec_nanos()),
            Err(e) => {
                let d = e.duration();
                (true, d.as_secs(), d.subsec_nanos())
            }
        });
        wire.serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<SystemTime>, D::Error> {
        let wire = Option::<(bool, u64, u32)>::deserialize(deserializer)?;
        wire.map(|(negative, sec, nanos)| {
            if nanos >= 1_000_000_000 {
                return Err(serde::de::Error::custom("invalid subsecond timestamp"));
            }
            let d = Duration::new(sec, nanos);
            (if negative {
                UNIX_EPOCH.checked_sub(d)
            } else {
                UNIX_EPOCH.checked_add(d)
            })
            .ok_or_else(|| serde::de::Error::custom("timestamp out of range"))
        })
        .transpose()
    }
}
