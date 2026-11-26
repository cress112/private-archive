use archive_domain::*;
use base64::Engine;

pub(super) fn timestamp(
    value: Option<&aws_smithy_types::DateTime>,
) -> Option<std::time::SystemTime> {
    value.and_then(|v| std::time::SystemTime::try_from(*v).ok())
}

pub(super) fn decorate(
    meta: &mut ObjectMetadata,
    class: Option<&str>,
    restore: Option<&str>,
    modified: Option<&aws_smithy_types::DateTime>,
    etag: Option<&str>,
) {
    meta.data_class = Some(match class.unwrap_or("STANDARD") {
        "STANDARD" => StoredClass::Standard,
        "DEEP_ARCHIVE" => StoredClass::Archived,
        other => StoredClass::Other(other.into()),
    });
    meta.restore_state = if let Some(restore) = restore {
        if restore.contains("ongoing-request=\"true\"") {
            RestoreState::Restoring
        } else if restore.contains("ongoing-request=\"false\"") {
            let until = restore
                .split("expiry-date=\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .and_then(|s| {
                    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc2822)
                        .ok()
                })
                .map(std::time::SystemTime::from);
            RestoreState::Restored { until }
        } else {
            RestoreState::Unknown
        }
    } else if matches!(class, Some("DEEP_ARCHIVE" | "GLACIER")) {
        RestoreState::Archived
    } else if class.is_none()
        || matches!(
            class,
            Some("STANDARD" | "STANDARD_IA" | "ONEZONE_IA" | "GLACIER_IR")
        )
    {
        RestoreState::NotRequired
    } else {
        RestoreState::Unknown
    };
    meta.queried_at = Some(std::time::SystemTime::now());
    meta.store_modified = timestamp(modified);
    meta.version_token = etag.map(str::to_owned);
}

pub(super) fn checksum(
    scope: Option<&str>,
    values: [Option<&str>; 5],
    location: &Location,
) -> Result<Option<Checksum>, OperationError> {
    let algorithms = [
        ChecksumAlgorithm::Sha256,
        ChecksumAlgorithm::Sha1,
        ChecksumAlgorithm::Crc64Nvme,
        ChecksumAlgorithm::Crc32c,
        ChecksumAlgorithm::Crc32,
    ];
    for (algorithm, encoded) in algorithms.into_iter().zip(values) {
        if let Some(encoded) = encoded {
            // Missing/unknown scope is deliberately not asserted to be a
            // FULL_OBJECT digest. The consumer reports verification unavailable.
            let scope = match scope {
                Some("FULL_OBJECT") => ChecksumScope::FullObject,
                Some("COMPOSITE") => ChecksumScope::Composite,
                _ => ChecksumScope::Unknown,
            };
            let value = if scope == ChecksumScope::FullObject {
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|_| {
                        OperationError::new(
                            ErrorKind::Integrity,
                            "s3-checksum",
                            "malformed FULL_OBJECT checksum",
                        )
                        .at(location)
                    })?
            } else {
                encoded.as_bytes().to_vec()
            };
            return Ok(Some(Checksum {
                algorithm,
                scope,
                value,
            }));
        }
    }
    Ok(None)
}

pub(super) fn size(value: Option<i64>, location: &Location) -> Result<u64, OperationError> {
    value.and_then(|n| n.try_into().ok()).ok_or_else(|| {
        OperationError::new(
            ErrorKind::Integrity,
            "s3-metadata",
            "missing or negative object size",
        )
        .at(location)
    })
}

pub(super) fn head(
    location: &Location,
    output: &aws_sdk_s3::operation::head_object::HeadObjectOutput,
) -> Result<ObjectMetadata, OperationError> {
    let mut meta = ObjectMetadata::file(location.clone(), size(output.content_length(), location)?);
    decorate(
        &mut meta,
        output.storage_class().map(|v| v.as_str()),
        output.restore(),
        output.last_modified(),
        output.e_tag(),
    );
    meta.checksum = checksum(
        output.checksum_type().map(|v| v.as_str()),
        [
            output.checksum_sha256(),
            output.checksum_sha1(),
            output.checksum_crc64_nvme(),
            output.checksum_crc32_c(),
            output.checksum_crc32(),
        ],
        location,
    )?;
    crate::attributes::decode(&mut meta, output.metadata());
    Ok(meta)
}

pub(super) fn get(
    location: &Location,
    output: &aws_sdk_s3::operation::get_object::GetObjectOutput,
) -> Result<ObjectMetadata, OperationError> {
    let mut meta = ObjectMetadata::file(location.clone(), size(output.content_length(), location)?);
    decorate(
        &mut meta,
        output.storage_class().map(|v| v.as_str()),
        output.restore(),
        output.last_modified(),
        output.e_tag(),
    );
    meta.checksum = checksum(
        output.checksum_type().map(|v| v.as_str()),
        [
            output.checksum_sha256(),
            output.checksum_sha1(),
            output.checksum_crc64_nvme(),
            output.checksum_crc32_c(),
            output.checksum_crc32(),
        ],
        location,
    )?;
    crate::attributes::decode(&mut meta, output.metadata());
    Ok(meta)
}
