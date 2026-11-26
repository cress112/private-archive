//! Shared configuration resolution and explicit atomic editing for CLI and GUI.
use archive_domain::{DataClass, Location, LogLevel, RestoreTier, compile_ignore};
use serde::Deserialize;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    BuiltIn,
    ConfigFile,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective<T> {
    pub value: T,
    pub origin: Origin,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub config_path: PathBuf,
    pub index_location: Effective<Location>,
    pub data_class: Effective<DataClass>,
    pub restore_tier: Effective<RestoreTier>,
    pub ignore: Effective<Vec<String>>,
    pub log_level: Effective<LogLevel>,
}

#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub index_location: Option<std::ffi::OsString>,
    pub data_class: Option<String>,
    pub restore_tier: Option<String>,
    pub ignore: Option<Vec<String>>,
    pub log_level: Option<String>,
}

/// Explicit paths keep tests and embedding frontends independent of global env.
pub struct Paths {
    pub home: PathBuf,
    pub cwd: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self, ConfigError> {
        let home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| ConfigError::Invalid("HOME is not set".into()))?;
        let paths = Self {
            home: home.into(),
            cwd: std::env::current_dir().map_err(ConfigError::Environment)?,
        };
        if !paths.home.is_absolute() {
            return Err(ConfigError::Invalid("HOME must be absolute".into()));
        }
        Ok(paths)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot determine configuration paths: {0}")]
    Environment(std::io::Error),
    #[error("cannot read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("cannot save config {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot lock config {path}: {message}")]
    Lock { path: PathBuf, message: String },
}

impl ConfigError {
    pub fn is_invalid_input(&self) -> bool {
        matches!(self, Self::Invalid(_) | Self::Parse { .. })
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Document {
    index: Index,
    copy: Copy,
    restore: Restore,
    ignore: Option<Vec<String>>,
    logging: Logging,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Index {
    location: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Copy {
    #[serde(rename = "data-class")]
    data_class: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Restore {
    tier: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Logging {
    level: Option<String>,
}

fn resolve_path(value: &OsStr, base: &Path, home: &Path) -> Result<PathBuf, ConfigError> {
    let path = Path::new(value);
    if value.is_empty() {
        return Err(ConfigError::Invalid("path must not be empty".into()));
    }
    if let Some(text) = value.to_str() {
        if text == "~" {
            return Ok(home.into());
        }
        if let Some(rest) = text.strip_prefix("~/") {
            return Ok(home.join(rest));
        }
        if text.starts_with('~') {
            return Err(ConfigError::Invalid(
                "only ~ and ~/ paths are supported".into(),
            ));
        }
    }
    Ok(if path.is_absolute() {
        path.into()
    } else {
        base.join(path)
    })
}

fn index_location(value: &OsStr, base: &Path, home: &Path) -> Result<Location, ConfigError> {
    match Location::parse(value, base).map_err(ConfigError::Invalid)? {
        Location::Remote(id) if id.key.is_empty() || id.key.ends_with('/') => {
            Err(ConfigError::Invalid(
                "index.location requires an object key, not a bucket or prefix".into(),
            ))
        }
        remote @ Location::Remote(_) => Ok(remote),
        Location::Local(_) => Ok(Location::Local(resolve_path(value, base, home)?)),
    }
}

fn choose<T>(default: T, file: Option<T>, runtime: Option<T>) -> Effective<T> {
    if let Some(value) = runtime {
        Effective {
            value,
            origin: Origin::Runtime,
        }
    } else if let Some(value) = file {
        Effective {
            value,
            origin: Origin::ConfigFile,
        }
    } else {
        Effective {
            value: default,
            origin: Origin::BuiltIn,
        }
    }
}

pub fn config_path(paths: &Paths, config: Option<&Path>) -> Result<PathBuf, ConfigError> {
    Ok(match config {
        Some(p) => resolve_path(p.as_os_str(), &paths.cwd, &paths.home)?,
        None => paths.home.join(".private-archive/config.toml"),
    })
}

pub fn load(
    paths: &Paths,
    config: Option<&Path>,
    overrides: Overrides,
) -> Result<Settings, ConfigError> {
    let config_path = config_path(paths, config)?;
    let doc: Document = match std::fs::read_to_string(&config_path) {
        Ok(text) => toml::from_str(&text).map_err(|e: toml::de::Error| ConfigError::Parse {
            path: config_path.clone(),
            message: e.message().into(),
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Document::default(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: config_path,
                source,
            });
        }
    };
    resolve_document(paths, config_path, doc, overrides)
}

fn resolve_document(
    paths: &Paths,
    config_path: PathBuf,
    doc: Document,
    overrides: Overrides,
) -> Result<Settings, ConfigError> {
    let parent = config_path
        .parent()
        .expect("absolute config path has a parent");
    let file_index = doc
        .index
        .location
        .as_deref()
        .map(|v| index_location(OsStr::new(v), parent, &paths.home))
        .transpose()?;
    let runtime_index = overrides
        .index_location
        .as_deref()
        .map(|v| index_location(v, &paths.cwd, &paths.home))
        .transpose()?;
    let file_class = doc
        .copy
        .data_class
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(ConfigError::Invalid)?;
    let runtime_class = overrides
        .data_class
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(ConfigError::Invalid)?;
    let file_tier = doc
        .restore
        .tier
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(ConfigError::Invalid)?;
    let runtime_tier = overrides
        .restore_tier
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(ConfigError::Invalid)?;
    let file_level = doc
        .logging
        .level
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(ConfigError::Invalid)?;
    let runtime_level = overrides
        .log_level
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(ConfigError::Invalid)?;
    // Validate every supplied layer, even when a higher-priority layer overrides it.
    for patterns in [&doc.ignore, &overrides.ignore].into_iter().flatten() {
        compile_ignore(&paths.cwd, patterns).map_err(ConfigError::Invalid)?;
    }
    Ok(Settings {
        config_path,
        index_location: choose(
            Location::Local(paths.home.join(".private-archive/index.db")),
            file_index,
            runtime_index,
        ),
        data_class: choose(DataClass::Standard, file_class, runtime_class),
        restore_tier: choose(RestoreTier::Bulk, file_tier, runtime_tier),
        ignore: choose(vec![], doc.ignore, overrides.ignore),
        log_level: choose(LogLevel::Info, file_level, runtime_level),
    })
}

mod edit;
pub use edit::*;
