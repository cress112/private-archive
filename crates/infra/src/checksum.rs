use archive_domain::{Checksum, ChecksumAlgorithm, ChecksumScope};
use sha2::Digest;

static CRC32: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
static CRC32C: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISCSI);
static CRC64: crc::Crc<u64> = crc::Crc::<u64>::new(&crc::CRC_64_NVME);

pub(crate) enum Checksummer {
    Crc32(crc::Digest<'static, u32>),
    Crc32c(crc::Digest<'static, u32>),
    Crc64(crc::Digest<'static, u64>),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
}
impl Checksummer {
    pub(crate) fn for_expected(expected: &Checksum) -> Option<Self> {
        if expected.scope != ChecksumScope::FullObject {
            return None;
        }
        Some(match &expected.algorithm {
            ChecksumAlgorithm::Crc32 => Self::Crc32(CRC32.digest()),
            ChecksumAlgorithm::Crc32c => Self::Crc32c(CRC32C.digest()),
            ChecksumAlgorithm::Crc64Nvme => Self::Crc64(CRC64.digest()),
            ChecksumAlgorithm::Sha1 => Self::Sha1(sha1::Sha1::new()),
            ChecksumAlgorithm::Sha256 => Self::Sha256(sha2::Sha256::new()),
            ChecksumAlgorithm::Other(_) => return None,
        })
    }
    pub(crate) fn update(&mut self, data: &[u8]) {
        match self {
            Self::Crc32(v) | Self::Crc32c(v) => v.update(data),
            Self::Crc64(v) => v.update(data),
            Self::Sha1(v) => v.update(data),
            Self::Sha256(v) => v.update(data),
        }
    }
    pub(crate) fn finish(self) -> Vec<u8> {
        match self {
            Self::Crc32(v) | Self::Crc32c(v) => v.finalize().to_be_bytes().to_vec(),
            Self::Crc64(v) => v.finalize().to_be_bytes().to_vec(),
            Self::Sha1(v) => v.finalize().to_vec(),
            Self::Sha256(v) => v.finalize().to_vec(),
        }
    }
}
