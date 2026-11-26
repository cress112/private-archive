use crate::Location;

/// Maximum diagnostic verbosity; operation results are independent of this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn allows(self, severity: Self) -> bool {
        severity <= self
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

impl std::str::FromStr for LogLevel {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "error" => Ok(Self::Error),
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            _ => Err("logging.level must be error, warn, info, debug or trace".into()),
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Input,
    NotFound,
    Permission,
    Unavailable,
    Integrity,
    Io,
    Cancelled,
    Unsupported,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationState {
    NotStarted,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Default)]
pub struct ErrorContext {
    pub source_uri: Option<String>,
    pub destination_uri: Option<String>,
    pub request_id: Option<String>,
    pub causes: Vec<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{stage}: {message}")]
pub struct OperationError {
    pub kind: ErrorKind,
    pub stage: &'static str,
    pub code: Option<String>,
    pub message: String,
    pub outcome: OperationState,
    pub context: Box<ErrorContext>,
}

impl OperationError {
    pub fn new(kind: ErrorKind, stage: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            stage,
            code: None,
            message: message.into(),
            outcome: OperationState::NotStarted,
            context: Box::default(),
        }
    }
    pub fn at(mut self, location: &Location) -> Self {
        self.context.source_uri = Some(location.display());
        self
    }
    pub fn io(stage: &'static str, location: &Location, error: std::io::Error) -> Self {
        let kind = match error.kind() {
            std::io::ErrorKind::NotFound => ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied => ErrorKind::Permission,
            _ => ErrorKind::Io,
        };
        let mut result = Self::new(kind, stage, error.to_string()).at(location);
        result.code = error.raw_os_error().map(|code| code.to_string());
        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningCategory {
    Index,
    Attributes,
    Verification,
    Cleanup,
    Ignored,
}
#[derive(Debug, Clone)]
pub struct Warning {
    pub category: WarningCategory,
    pub stage: &'static str,
    pub uri: String,
    pub code: Option<String>,
    pub message: String,
}
