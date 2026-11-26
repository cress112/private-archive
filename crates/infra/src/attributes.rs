//! The portable attributes have their own versioned S3 metadata schema.
use archive_domain::*;
use std::{
    collections::HashMap,
    fs::{File, FileTimes},
    os::unix::fs::PermissionsExt,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub(crate) fn encode(source: &ObjectMetadata) -> HashMap<String, String> {
    let mut values = HashMap::from([("private-archive-version".into(), "1".into())]);
    for (name, value) in [
        ("private-archive-mtime", source.original_mtime),
        ("private-archive-created", source.original_creation_time),
    ] {
        if let Some(value) = value
            && let Some(text) = timestamp_text(value)
        {
            values.insert(name.into(), text);
        }
    }
    if let Some(mode) = source.unix_mode.filter(|m| *m <= 0o7777) {
        values.insert("private-archive-mode".into(), format!("{mode:o}"));
    }
    values
}
pub(crate) fn decode(target: &mut ObjectMetadata, values: Option<&HashMap<String, String>>) {
    let Some(values) =
        values.filter(|v| v.get("private-archive-version").is_some_and(|s| s == "1"))
    else {
        return;
    };
    let timestamp = |name: &str| {
        values
            .get(name)
            .and_then(|v| OffsetDateTime::parse(v, &Rfc3339).ok())
            .map(std::time::SystemTime::from)
    };
    target.original_mtime = timestamp("private-archive-mtime");
    target.original_creation_time = timestamp("private-archive-created");
    target.unix_mode = values
        .get("private-archive-mode")
        .filter(|s| !s.is_empty() && s.len() <= 4 && s.bytes().all(|b| (b'0'..=b'7').contains(&b)))
        .and_then(|s| u32::from_str_radix(s, 8).ok());
}
pub(crate) fn warning(location: &Location, message: impl Into<String>) -> Warning {
    Warning {
        category: WarningCategory::Attributes,
        stage: "attributes",
        uri: location.display(),
        code: None,
        message: message.into(),
    }
}
pub(crate) fn restore(
    file: &File,
    source: &ObjectMetadata,
    destination: &Location,
) -> Vec<Warning> {
    let mut notices = vec![];
    if let Some(time) = source.original_mtime
        && let Err(e) = file.set_times(FileTimes::new().set_modified(time))
    {
        notices.push(warning(
            destination,
            format!("cannot restore modification time: {e}"),
        ));
    }
    if let Some(time) = source.original_creation_time {
        #[cfg(target_vendor = "apple")]
        {
            use std::os::darwin::fs::FileTimesExt;
            if let Err(e) = file.set_times(FileTimes::new().set_created(time)) {
                notices.push(warning(
                    destination,
                    format!("cannot restore creation time: {e}"),
                ));
            }
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            let _ = time;
            notices.push(warning(
                destination,
                "creation time restoration is unavailable on this platform",
            ));
        }
    }
    if let Some(mode) = source.unix_mode {
        if mode > 0o7777 {
            notices.push(warning(
                destination,
                "invalid UNIX permission bits; permissions were not restored",
            ));
        } else if let Err(e) = file.set_permissions(std::fs::Permissions::from_mode(mode)) {
            notices.push(warning(
                destination,
                format!("cannot restore UNIX permissions: {e}"),
            ));
        }
    }
    if source.original_mtime.is_none()
        || source.original_creation_time.is_none()
        || source.unix_mode.is_none()
    {
        notices.push(warning(destination,"some original timestamps or UNIX permissions are unknown; available attributes were restored"));
    }
    notices
}

fn timestamp_text(value: std::time::SystemTime) -> Option<String> {
    let nanos = match value.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i128::try_from(d.as_nanos()).ok()?,
        Err(e) => -i128::try_from(e.duration().as_nanos()).ok()?,
    };
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()?
        .format(&Rfc3339)
        .ok()
}
pub(crate) fn notices(source: &ObjectMetadata, destination: &Location) -> Vec<Warning> {
    let encoded = encode(source);
    let incomplete = source.original_mtime.is_none()
        || source.original_creation_time.is_none()
        || source.unix_mode.is_none();
    let invalid = source.original_mtime.is_some() && !encoded.contains_key("private-archive-mtime")
        || source.original_creation_time.is_some()
            && !encoded.contains_key("private-archive-created")
        || source.unix_mode.is_some() && !encoded.contains_key("private-archive-mode");
    if incomplete || invalid {
        vec![warning(
            destination,
            "some original timestamps or permissions are unknown or unrepresentable; available attributes were preserved",
        )]
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_schema_roundtrips_subseconds_before_epoch_and_permissions_without_owner() {
        let mut source = ObjectMetadata::file(Location::Local("/source".into()), 0);
        source.original_mtime = Some(std::time::UNIX_EPOCH - std::time::Duration::new(1, 123));
        source.original_creation_time =
            Some(std::time::UNIX_EPOCH + std::time::Duration::new(1, 789));
        source.unix_mode = Some(0o640);
        let values = encode(&source);
        assert_eq!(values.len(), 4);
        assert_eq!(values["private-archive-mode"], "640");
        assert!(!values.keys().any(|k| k.contains("owner")));
        let mut decoded = ObjectMetadata::file(source.location.clone(), 0);
        decode(&mut decoded, Some(&values));
        assert_eq!(decoded.original_mtime, source.original_mtime);
        assert_eq!(
            decoded.original_creation_time,
            source.original_creation_time
        );
        assert_eq!(decoded.unix_mode, source.unix_mode);
        let mut invalid = values;
        invalid.insert("private-archive-mode".into(), "9999".into());
        invalid.insert("private-archive-mtime".into(), "not-a-time".into());
        decode(&mut decoded, Some(&invalid));
        assert!(decoded.original_mtime.is_none());
        assert!(decoded.unix_mode.is_none());
        assert_eq!(notices(&decoded, &source.location).len(), 1);
        invalid.insert("private-archive-version".into(), "future".into());
        let mut future = ObjectMetadata::file(source.location, 0);
        decode(&mut future, Some(&invalid));
        assert!(future.original_creation_time.is_none());
    }
}
