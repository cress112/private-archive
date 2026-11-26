use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// A remote root has an empty key. Use require_object at object-only boundaries.
/// S3 keys are opaque UTF-8: no URL decoding or path normalization is performed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectId {
    pub namespace: String,
    pub scheme: String,
    pub authority: String,
    pub key: String,
}

impl ObjectId {
    pub fn parse_s3(uri: &str) -> Result<Self, String> {
        let suffix = uri
            .strip_prefix("s3://")
            .ok_or("expected s3://bucket/key")?;
        let (bucket, key) = suffix.split_once('/').unwrap_or((suffix, ""));
        if bucket.is_empty()
            || !bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        {
            return Err(
                "invalid S3 bucket; use s3://bucket/key (ARNs and access points are unsupported)"
                    .into(),
            );
        }
        if key.contains('\0') {
            return Err("S3 key must not contain NUL".into());
        }
        Ok(Self {
            namespace: "aws".into(),
            scheme: "s3".into(),
            authority: bucket.into(),
            key: key.into(),
        })
    }

    pub fn require_object(self) -> Result<RemoteObject, String> {
        if self.key.is_empty() {
            Err("an object key is required".into())
        } else {
            Ok(RemoteObject(self))
        }
    }

    pub fn uri(&self) -> String {
        format!("{}://{}/{}", self.scheme, self.authority, self.key)
    }

    pub fn with_key(&self, key: String) -> Self {
        Self {
            key,
            ..self.clone()
        }
    }
}

/// Object-only ports/routes cannot contain a bucket root. The inner address is
/// immutable so callers cannot invalidate the nonempty-key invariant.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RemoteObject(ObjectId);
impl RemoteObject {
    pub fn id(&self) -> &ObjectId {
        &self.0
    }
    pub fn into_id(self) -> ObjectId {
        self.0
    }
    pub fn uri(&self) -> String {
        self.0.uri()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub enum Location {
    Local(#[serde(with = "path_bytes")] PathBuf),
    Remote(ObjectId),
}

impl Location {
    pub fn parse(value: &OsStr, cwd: &Path) -> Result<Self, String> {
        if let Some(text) = value.to_str() {
            if text.starts_with("s3://") {
                return Ok(Self::Remote(ObjectId::parse_s3(text)?));
            }
            if text.split_once("://").is_some_and(|(scheme, _)| {
                !scheme.is_empty()
                    && scheme
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.')
            }) {
                return Err("unsupported URI scheme; expected a local path or s3:// URI".into());
            }
        }
        if value.is_empty() {
            return Err("path must not be empty".into());
        }
        if value.as_encoded_bytes().contains(&0) {
            return Err("local path must not contain NUL".into());
        }
        let path = Path::new(value);
        Ok(Self::Local(if path.is_absolute() {
            path.into()
        } else {
            cwd.join(path)
        }))
    }

    pub fn display(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::Remote(id) => id.uri(),
        }
    }
}

// Local paths are opaque OS bytes in private disk records.
pub mod path_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::{
        ffi::OsString,
        os::unix::ffi::{OsStrExt, OsStringExt},
        path::PathBuf,
    };
    pub fn serialize<S: Serializer>(
        path: &std::path::Path,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        path.as_os_str().as_bytes().serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        if bytes.contains(&0) {
            return Err(serde::de::Error::custom("NUL in local path"));
        }
        Ok(OsString::from_vec(bytes).into())
    }
}
