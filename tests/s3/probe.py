# /// script
# requires-python = ">=3.10"
# dependencies = ["boto3==1.44.3"]
# ///
"""Check the local SeaweedFS fixture; this does not test private-archive itself."""

import base64
import hashlib
import json
import time
import uuid
import zlib

import boto3
from botocore.config import Config
from botocore.exceptions import ClientError, EndpointConnectionError


BUCKET = "private-archive-test"
PREFIX = f"probe/{uuid.uuid4().hex}/"
ENDPOINT = "http://127.0.0.1:18333"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def crc32_b64(value):
    return base64.b64encode(value.to_bytes(4, "big")).decode("ascii")


def main():
    # Explicit local endpoint and dummy credentials prevent use of real AWS profiles.
    s3 = boto3.client(
        "s3",
        endpoint_url=ENDPOINT,
        region_name="us-east-1",
        aws_access_key_id="private-archive-test",
        aws_secret_access_key="private-archive-test-secret",
        config=Config(
            retries={"total_max_attempts": 1},
            connect_timeout=2,
            read_timeout=15,
            s3={"addressing_style": "path"},
            request_checksum_calculation="when_supported",
            response_checksum_validation="when_supported",
        ),
    )
    for attempt in range(30):
        try:
            s3.head_bucket(Bucket=BUCKET)
            break
        except (ClientError, EndpointConnectionError):
            if attempt == 29:
                raise
            time.sleep(1)

    active_uploads = {}
    passed = []

    def get_hash(key):
        response = s3.get_object(Bucket=BUCKET, Key=key, ChecksumMode="ENABLED")
        digest = hashlib.sha256()
        size = 0
        with response["Body"] as body:
            while chunk := body.read(1024 * 1024):
                digest.update(chunk)
                size += len(chunk)
        return size, digest.hexdigest(), response

    try:
        source = PREFIX + "source"
        single = b"private-archive fixture\x00\xff\n"
        checksum = crc32_b64(zlib.crc32(single))
        s3.put_object(
            Bucket=BUCKET,
            Key=source,
            Body=single,
            ChecksumAlgorithm="CRC32",
            ChecksumCRC32=checksum,
            Metadata={"private-archive-version": "1"},
        )
        size, digest, result = get_hash(source)
        require(size == len(single), "single GET size")
        require(digest == hashlib.sha256(single).hexdigest(), "single GET content")
        require(result.get("ChecksumCRC32") == checksum, "single GET CRC32")
        head = s3.head_object(Bucket=BUCKET, Key=source)
        require(head["Metadata"].get("private-archive-version") == "1", "metadata")
        passed.append("put/get/head/metadata/CRC32")

        copied = PREFIX + "copied"
        s3.copy_object(Bucket=BUCKET, Key=copied, CopySource={"Bucket": BUCKET, "Key": source})
        require(get_hash(copied)[:2] == (size, digest), "CopyObject content")
        passed.append("CopyObject")

        listed = s3.list_objects_v2(Bucket=BUCKET, Prefix=PREFIX, MaxKeys=1)
        require(listed.get("IsTruncated"), "LIST should have another page")
        require(listed.get("NextContinuationToken"), "LIST continuation token")
        second = s3.list_objects_v2(
            Bucket=BUCKET, Prefix=PREFIX, MaxKeys=1,
            ContinuationToken=listed["NextContinuationToken"],
        )
        require(listed["Contents"][0]["Key"] != second["Contents"][0]["Key"], "LIST pagination")
        passed.append("ListObjectsV2 pagination")

        multipart = PREFIX + "multipart"
        created = s3.create_multipart_upload(
            Bucket=BUCKET, Key=multipart, ChecksumAlgorithm="CRC32", ChecksumType="FULL_OBJECT",
        )
        upload_id = created["UploadId"]
        active_uploads[multipart] = upload_id
        parts = []
        expected_hash = hashlib.sha256()
        total_crc = 0
        part_size = 8 * 1024 * 1024
        for number in (1, 2):
            payload = bytes([number]) * part_size
            expected_hash.update(payload)
            total_crc = zlib.crc32(payload, total_crc)
            part_crc = crc32_b64(zlib.crc32(payload))
            uploaded = s3.upload_part(
                Bucket=BUCKET, Key=multipart, UploadId=upload_id, PartNumber=number,
                Body=payload, ChecksumCRC32=part_crc,
            )
            require(uploaded.get("ChecksumCRC32") == part_crc, "part CRC32 response")
            parts.append({"PartNumber": number, "ETag": uploaded["ETag"], "ChecksumCRC32": part_crc})
        s3.complete_multipart_upload(
            Bucket=BUCKET, Key=multipart, UploadId=upload_id,
            MultipartUpload={"Parts": parts},
            ChecksumType="FULL_OBJECT", ChecksumCRC32=crc32_b64(total_crc),
        )
        del active_uploads[multipart]
        size, digest, result = get_hash(multipart)
        require((size, digest) == (2 * part_size, expected_hash.hexdigest()), "multipart content")
        require(result.get("ChecksumType") == "FULL_OBJECT", "full-object checksum type")
        require(result.get("ChecksumCRC32") == crc32_b64(total_crc), "full-object CRC32 value")
        passed.append("multipart upload / FULL_OBJECT CRC32 / download")

        copied_multipart = PREFIX + "multipart-copy"
        upload_id = s3.create_multipart_upload(Bucket=BUCKET, Key=copied_multipart)["UploadId"]
        active_uploads[copied_multipart] = upload_id
        parts = []
        for number in (1, 2):
            response = s3.upload_part_copy(
                Bucket=BUCKET, Key=copied_multipart, UploadId=upload_id,
                PartNumber=number, CopySource={"Bucket": BUCKET, "Key": multipart},
                CopySourceRange=f"bytes={(number - 1) * part_size}-{number * part_size - 1}",
            )
            parts.append({"PartNumber": number, "ETag": response["CopyPartResult"]["ETag"]})
        s3.complete_multipart_upload(
            Bucket=BUCKET, Key=copied_multipart, UploadId=upload_id,
            MultipartUpload={"Parts": parts},
        )
        del active_uploads[copied_multipart]
        require(get_hash(copied_multipart)[:2] == (size, digest), "multipart copy content")
        passed.append("UploadPartCopy / CompleteMultipartUpload")

        aborted = PREFIX + "aborted"
        upload_id = s3.create_multipart_upload(Bucket=BUCKET, Key=aborted)["UploadId"]
        active_uploads[aborted] = upload_id
        s3.upload_part(Bucket=BUCKET, Key=aborted, UploadId=upload_id, PartNumber=1, Body=b"x" * part_size)
        s3.abort_multipart_upload(Bucket=BUCKET, Key=aborted, UploadId=upload_id)
        del active_uploads[aborted]
        remaining = s3.list_multipart_uploads(Bucket=BUCKET, Prefix=PREFIX)
        require(not remaining.get("Uploads"), "aborted upload still listed")
        passed.append("AbortMultipartUpload / ListMultipartUploads")

        try:
            s3.put_object(
                Bucket=BUCKET, Key=PREFIX + "bad-checksum", Body=b"bad",
                ChecksumAlgorithm="CRC32", ChecksumCRC32=crc32_b64(0),
            )
        except ClientError as error:
            require(error.response["Error"]["Code"] == "BadDigest", "checksum mismatch error code")
        else:
            raise RuntimeError("server accepted incorrect CRC32")
        passed.append("checksum mismatch rejected")

        try:
            s3.head_object(Bucket=BUCKET, Key=PREFIX + "missing")
        except ClientError as error:
            require(error.response["ResponseMetadata"]["HTTPStatusCode"] == 404, "missing HEAD status")
        else:
            raise RuntimeError("missing HEAD unexpectedly succeeded")
        passed.append("missing object HEAD 404")
    finally:
        # Only IDs and keys made by this run may be removed.
        for key, upload_id in active_uploads.items():
            s3.abort_multipart_upload(Bucket=BUCKET, Key=key, UploadId=upload_id)
        paginator = s3.get_paginator("list_objects_v2")
        for page in paginator.paginate(Bucket=BUCKET, Prefix=PREFIX):
            for entry in page.get("Contents", []):
                s3.delete_object(Bucket=BUCKET, Key=entry["Key"])
        require(s3.list_objects_v2(Bucket=BUCKET, Prefix=PREFIX).get("KeyCount", 0) == 0, "cleanup")

    print(json.dumps({"endpoint": ENDPOINT, "boto3": boto3.__version__, "passed": passed, "cleanup": "ok"}, indent=2))


if __name__ == "__main__":
    main()
