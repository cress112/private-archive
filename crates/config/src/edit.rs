use crate::*;
use archive_domain::parse_ignore_list;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    str::FromStr,
};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKey {
    IndexLocation,
    DataClass,
    RestoreTier,
    Ignore,
    LogLevel,
}

impl ConfigKey {
    pub const ALL: [Self; 5] = [
        Self::IndexLocation,
        Self::DataClass,
        Self::RestoreTier,
        Self::Ignore,
        Self::LogLevel,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IndexLocation => "index.location",
            Self::DataClass => "copy.data-class",
            Self::RestoreTier => "restore.tier",
            Self::Ignore => "ignore",
            Self::LogLevel => "logging.level",
        }
    }
    fn parts(self) -> (Option<&'static str>, &'static str) {
        match self.as_str().split_once('.') {
            Some((table, key)) => (Some(table), key),
            None => (None, "ignore"),
        }
    }
}
impl FromStr for ConfigKey {
    type Err = ConfigError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL.into_iter().find(|k| k.as_str() == value).ok_or_else(|| ConfigError::Invalid(format!("unknown setting {value:?}; expected index.location, copy.data-class, restore.tier, ignore or logging.level")))
    }
}

/// Saved values are independent from runtime overrides and effective defaults.
#[derive(Debug, Clone)]
pub enum ConfigValue {
    IndexLocation(String),
    DataClass(DataClass),
    RestoreTier(RestoreTier),
    Ignore(Vec<String>),
    LogLevel(LogLevel),
}
impl ConfigValue {
    /// Text accepted by set/edit, without TOML quoting or resolved path expansion.
    pub fn input_text(&self) -> String {
        match self {
            Self::IndexLocation(v) => v.clone(),
            Self::DataClass(DataClass::Standard) => "standard".into(),
            Self::DataClass(DataClass::Archived) => "archived".into(),
            Self::RestoreTier(RestoreTier::Bulk) => "Bulk".into(),
            Self::RestoreTier(RestoreTier::Standard) => "Standard".into(),
            Self::LogLevel(v) => v.to_string(),
            Self::Ignore(v) => v
                .iter()
                .map(|p| p.replace('\\', "\\\\").replace(',', "\\,"))
                .collect::<Vec<_>>()
                .join(","),
        }
    }
    pub fn parse(key: ConfigKey, value: &str) -> Result<Self, ConfigError> {
        Ok(match key {
            ConfigKey::IndexLocation => Self::IndexLocation(value.into()),
            ConfigKey::DataClass => Self::DataClass(value.parse().map_err(ConfigError::Invalid)?),
            ConfigKey::RestoreTier => {
                Self::RestoreTier(value.parse().map_err(ConfigError::Invalid)?)
            }
            ConfigKey::Ignore => Self::Ignore(parse_ignore_list(value)),
            ConfigKey::LogLevel => Self::LogLevel(value.parse().map_err(ConfigError::Invalid)?),
        })
    }
    pub fn key(&self) -> ConfigKey {
        match self {
            Self::IndexLocation(_) => ConfigKey::IndexLocation,
            Self::DataClass(_) => ConfigKey::DataClass,
            Self::RestoreTier(_) => ConfigKey::RestoreTier,
            Self::Ignore(_) => ConfigKey::Ignore,
            Self::LogLevel(_) => ConfigKey::LogLevel,
        }
    }
    fn value(&self) -> Value {
        match self {
            Self::IndexLocation(v) => Value::from(v.as_str()),
            Self::DataClass(DataClass::Standard) => Value::from("standard"),
            Self::DataClass(DataClass::Archived) => Value::from("archived"),
            Self::RestoreTier(RestoreTier::Bulk) => Value::from("Bulk"),
            Self::RestoreTier(RestoreTier::Standard) => Value::from("Standard"),
            Self::LogLevel(v) => Value::from(v.as_str()),
            Self::Ignore(patterns) => {
                let mut array = Array::new();
                for p in patterns {
                    array.push(p.as_str());
                }
                Value::Array(array)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Change {
    Set(ConfigValue),
    Unset(ConfigKey),
}

/// Saved explicit values for editors. Opening this document never creates it.
pub fn read_document(
    paths: &Paths,
    config: Option<&Path>,
) -> Result<Vec<ConfigValue>, ConfigError> {
    let path = config_path(paths, config)?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    let doc: Document = toml::from_str(&text).map_err(|e: toml::de::Error| ConfigError::Parse {
        path: path.clone(),
        message: e.message().into(),
    })?;
    // Keep the same validation as normal settings resolution, including values
    // that are hidden by runtime overrides.
    let checked: Document =
        toml::from_str(&text).map_err(|e: toml::de::Error| ConfigError::Parse {
            path: path.clone(),
            message: e.message().into(),
        })?;
    resolve_document(paths, path, checked, Overrides::default())?;
    let mut values = vec![];
    if let Some(v) = doc.index.location {
        values.push(ConfigValue::IndexLocation(v));
    }
    if let Some(v) = doc.copy.data_class {
        values.push(ConfigValue::parse(ConfigKey::DataClass, &v)?);
    }
    if let Some(v) = doc.restore.tier {
        values.push(ConfigValue::parse(ConfigKey::RestoreTier, &v)?);
    }
    if let Some(v) = doc.ignore {
        values.push(ConfigValue::Ignore(v));
    }
    if let Some(v) = doc.logging.level {
        values.push(ConfigValue::parse(ConfigKey::LogLevel, &v)?);
    }
    Ok(values)
}

/// Validate a GUI/CLI edit without writing files or touching the index/store.
pub fn validate_change(paths: &Paths, path: &Path, change: &Change) -> Result<(), ConfigError> {
    match change {
        Change::Set(ConfigValue::IndexLocation(v)) => {
            index_location(
                OsStr::new(v),
                path.parent()
                    .ok_or_else(|| ConfigError::Invalid("config path must have a parent".into()))?,
                &paths.home,
            )?;
        }
        Change::Set(ConfigValue::Ignore(patterns)) => {
            compile_ignore(&paths.cwd, patterns).map_err(ConfigError::Invalid)?;
        }
        _ => (),
    }
    Ok(())
}

fn write_error(path: &Path, source: io::Error) -> ConfigError {
    ConfigError::Write {
        path: path.into(),
        source,
    }
}

/// Stable adjacent lock file. Its existence has no meaning; the OS owns the lock.
fn lock(path: &Path) -> Result<File, ConfigError> {
    let mut name = path
        .file_name()
        .ok_or_else(|| ConfigError::Invalid("config must be a file path".into()))?
        .to_os_string();
    name.push(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path.with_file_name(name))
        .map_err(|e| write_error(path, e))?;
    file.try_lock().map_err(|e| ConfigError::Lock {
        path: path.into(),
        message: e.to_string(),
    })?;
    Ok(file)
}

fn publish(
    path: &Path,
    text: &str,
    create: bool,
    permissions: Option<fs::Permissions>,
) -> Result<(), ConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::Invalid("config must have a parent".into()))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|e| write_error(path, e))?;
    temporary
        .write_all(text.as_bytes())
        .map_err(|e| write_error(path, e))?;
    if let Some(permissions) = permissions {
        temporary
            .as_file()
            .set_permissions(permissions)
            .map_err(|e| write_error(path, e))?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|e| write_error(path, e))?;
    let result = if create {
        temporary.persist_noclobber(path)
    } else {
        temporary.persist(path)
    };
    result.map_err(|e| write_error(path, e.error))?;
    File::open(parent)
        .and_then(|p| p.sync_all())
        .map_err(|e| write_error(path, e))?;
    Ok(())
}

const TEMPLATE: &str = "# Private Archive shared configuration. Uncomment values to override defaults.\n# ignore = []\n\n[index]\n# location = \"~/.private-archive/index.db\"\n\n[copy]\n# data-class = \"standard\"\n\n[restore]\n# tier = \"Bulk\"\n\n[logging]\n# level = \"info\"\n";

/// Explicit creation only; never truncates an existing file, including symlinks.
pub fn init(paths: &Paths, config: Option<&Path>) -> Result<PathBuf, ConfigError> {
    let path = config_path(paths, config)?;
    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::Invalid("config must be a file path".into()))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|e| write_error(&path, e))?;
    let _lock = lock(&path)?;
    publish(&path, TEMPLATE, true, None)?;
    Ok(path)
}

/// Re-read under a short lock, modify only explicit edits and atomically publish.
/// No effective runtime values are ever serialized into the document.
pub fn apply_changes(
    paths: &Paths,
    config: Option<&Path>,
    changes: &[Change],
) -> Result<Settings, ConfigError> {
    let path = config_path(paths, config)?;
    for change in changes {
        validate_change(paths, &path, change)?;
    }
    // Resolving an existing symlink preserves the user's link during replacement.
    let target = fs::canonicalize(&path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            write_error(
                &path,
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "configuration does not exist; run cfg init first",
                ),
            )
        } else {
            write_error(&path, e)
        }
    })?;
    let _lock = lock(&target)?;
    let original = fs::read_to_string(&target).map_err(|e| ConfigError::Read {
        path: path.clone(),
        source: e,
    })?;
    // Preserve invalid-file diagnostics; an edit is never an implicit repair.
    let parsed = toml::from_str(&original).map_err(|e: toml::de::Error| ConfigError::Parse {
        path: path.clone(),
        message: e.message().into(),
    })?;
    resolve_document(paths, path.clone(), parsed, Overrides::default())?;
    let mut doc = original
        .parse::<DocumentMut>()
        .map_err(|e| ConfigError::Parse {
            path: path.clone(),
            message: e.to_string(),
        })?;
    for change in changes {
        let key = match change {
            Change::Set(v) => v.key(),
            Change::Unset(k) => *k,
        };
        let (table, leaf) = key.parts();
        if matches!(change, Change::Unset(_)) {
            if let Some(table) = table {
                if let Some(item) = doc.get_mut(table)
                    && let Some(t) = item.as_table_like_mut()
                {
                    t.remove(leaf);
                }
            } else {
                doc.remove(leaf);
            }
            continue;
        }
        let Change::Set(value) = change else {
            unreachable!()
        };
        let mut value = value.value();
        let table = if let Some(name) = table {
            if !doc.contains_key(name) {
                doc[name] = Item::Table(Table::new());
            }
            doc.get_mut(name)
                .and_then(Item::as_table_like_mut)
                .ok_or_else(|| ConfigError::Invalid(format!("{name} must be a table")))?
        } else {
            doc.as_table_mut() as &mut dyn toml_edit::TableLike
        };
        if let Some(old) = table.get(leaf).and_then(Item::as_value) {
            *value.decor_mut() = old.decor().clone();
        }
        // insert() reformats an existing key and removes its leading comments.
        // Mutate the value in place to preserve the original key decorations.
        if let Some(item) = table.get_mut(leaf) {
            *item = Item::Value(value);
        } else {
            table.insert(leaf, Item::Value(value));
        }
    }
    let text = doc.to_string();
    let parsed = toml::from_str(&text).map_err(|e: toml::de::Error| ConfigError::Parse {
        path: path.clone(),
        message: e.message().into(),
    })?;
    let settings = resolve_document(paths, path.clone(), parsed, Overrides::default())?;
    if text != original {
        let permissions = fs::metadata(&target)
            .map_err(|e| write_error(&path, e))?
            .permissions();
        publish(&target, &text, false, Some(permissions))?;
    }
    Ok(settings)
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::ConfigFile => "config-file",
            Self::Runtime => "runtime",
        }
    }
}

impl Settings {
    /// Value and provenance for display. Paths are rendered with TOML quoting.
    pub fn setting(&self, key: ConfigKey) -> (String, Origin) {
        let (value, origin) = match key {
            ConfigKey::IndexLocation => (
                ConfigValue::IndexLocation(self.index_location.value.display()),
                self.index_location.origin,
            ),
            ConfigKey::DataClass => (
                ConfigValue::DataClass(self.data_class.value),
                self.data_class.origin,
            ),
            ConfigKey::RestoreTier => (
                ConfigValue::RestoreTier(self.restore_tier.value),
                self.restore_tier.origin,
            ),
            ConfigKey::Ignore => (
                ConfigValue::Ignore(self.ignore.value.clone()),
                self.ignore.origin,
            ),
            ConfigKey::LogLevel => (
                ConfigValue::LogLevel(self.log_level.value),
                self.log_level.origin,
            ),
        };
        (value.value().to_string(), origin)
    }
}
