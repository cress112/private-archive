use archive_domain::*;
use rusqlite::{Row, types::Type};
use serde::{Serialize, de::DeserializeOwned};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(super) fn json<T: Serialize>(value: &T) -> Result<String, OperationError> {
    serde_json::to_string(value).map_err(|e| super::error("index-codec", e))
}
fn decode<T: DeserializeOwned>(row: &Row<'_>, n: usize) -> rusqlite::Result<Option<T>> {
    row.get::<_, Option<String>>(n)?
        .map(|text| {
            serde_json::from_str(&text)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(n, Type::Text, Box::new(e)))
        })
        .transpose()
}
pub(super) fn split(
    time: Option<SystemTime>,
) -> Result<(Option<i64>, Option<i64>), OperationError> {
    let Some(time) = time else {
        return Ok((None, None));
    };
    let (sec, nanos) = match time.duration_since(UNIX_EPOCH) {
        Ok(d) => (
            i64::try_from(d.as_secs()).map_err(|e| super::error("index-time", e))?,
            i64::from(d.subsec_nanos()),
        ),
        Err(e) => {
            let d = e.duration();
            let sec = i64::try_from(d.as_secs()).map_err(|e| super::error("index-time", e))?;
            if d.subsec_nanos() == 0 {
                (-sec, 0)
            } else {
                (-sec - 1, 1_000_000_000 - i64::from(d.subsec_nanos()))
            }
        }
    };
    Ok((Some(sec), Some(nanos)))
}
fn time(
    row: &Row<'_>,
    seconds: usize,
    nanos: Option<usize>,
) -> rusqlite::Result<Option<SystemTime>> {
    let Some(sec) = row.get::<_, Option<i64>>(seconds)? else {
        return Ok(None);
    };
    let ns = match nanos {
        Some(n) => row.get::<_, Option<i64>>(n)?.unwrap_or(0),
        None => 0,
    };
    if !(0..1_000_000_000).contains(&ns) {
        return Err(rusqlite::Error::IntegralValueOutOfRange(seconds, ns));
    }
    let value = if sec >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(sec as u64, ns as u32))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(sec.unsigned_abs()))
            .and_then(|t| t.checked_add(Duration::from_nanos(ns as u64)))
    };
    value
        .map(Some)
        .ok_or(rusqlite::Error::IntegralValueOutOfRange(seconds, sec))
}
pub(super) fn row(row: &Row<'_>) -> rusqlite::Result<ObjectMetadata> {
    let key: Vec<u8> = row.get(3)?;
    let key = String::from_utf8(key)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, Type::Blob, Box::new(e)))?;
    let size: Option<i64> = row.get(4)?;
    if size.is_some_and(|s| s < 0) {
        return Err(rusqlite::Error::IntegralValueOutOfRange(4, size.unwrap()));
    }
    let checksum = match (decode(row, 17)?, decode(row, 18)?, decode(row, 19)?) {
        (Some(algorithm), Some(scope), Some(value)) => Some(Checksum {
            algorithm,
            scope,
            value,
        }),
        (None, None, None) => None,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let state: Option<String> = row.get(6)?;
    let restore_state = match state.as_deref() {
        Some("not-required") => RestoreState::NotRequired,
        Some("archived") => RestoreState::Archived,
        Some("restoring") => RestoreState::Restoring,
        Some("restored") => RestoreState::Restored {
            until: time(row, 7, None)?,
        },
        None => RestoreState::Unknown,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let mode: Option<i64> = row.get(12)?;
    let unix_mode = mode
        .map(|n| u32::try_from(n).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(12, n)))
        .transpose()?;
    Ok(ObjectMetadata {
        location: Location::Remote(ObjectId {
            namespace: row.get(0)?,
            scheme: row.get(1)?,
            authority: row.get(2)?,
            key,
        }),
        kind: EntryKind::File,
        size: size.map(|n| n as u64),
        data_class: decode(row, 5)?,
        restore_state,
        original_mtime: time(row, 8, Some(9))?,
        original_creation_time: time(row, 10, Some(11))?,
        unix_mode,
        queried_at: time(row, 13, Some(14))?,
        store_modified: time(row, 15, None)?,
        version_token: row.get(16)?,
        checksum,
    })
}
