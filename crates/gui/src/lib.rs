//! GUI startup and local browsing. UI types stay outside the shared core.
use archive_config::{Overrides, Paths, Settings};
use archive_domain::{Location, LogLevel, OperationError};
use archive_index::{
    BrowseControl, BrowseProjection, BrowseRequest, BrowseTarget, IndexBrowse, IndexListing,
    ListingPage, ListingSummary, SortSpec,
};
use archive_infra::{SqliteIndex, prepare_index_path};
use clap::Parser;
use std::{ffi::OsString, path::PathBuf};

pub const PAGE_SIZE: usize = 256;
pub mod browser;
pub mod cache;
pub mod reconcile;
pub mod settings;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "private-archive-gui",
    version,
    about = "Browse the saved private archive index",
    after_help = "Uses the same TOML settings and index as private-archive. Priority: built-in defaults, configuration file, runtime overrides. Runtime overrides are not saved. Startup reads the local copy only, including when index.location is an S3 URI; no authentication, remote fetch or bucket discovery is performed. Hidden names are included unless explicitly ignored."
)]
pub struct GuiArgs {
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[arg(long, value_name = "PATH_OR_URI")]
    pub index_location: Option<OsString>,
    #[arg(long, value_parser = ["standard", "archived"])]
    pub data_class: Option<String>,
    #[arg(long, value_parser = ["Bulk", "Standard"])]
    pub restore_tier: Option<String>,
    #[arg(
        long,
        allow_hyphen_values = true,
        help = r"Comma-separated gitignore patterns; \, is a literal comma and \\ a backslash; empty clears configured patterns"
    )]
    pub ignore: Option<String>,
    #[arg(long, value_parser = ["error", "warn", "info", "debug", "trace"])]
    pub log_level: Option<String>,
    #[cfg(feature = "test-fixtures")]
    #[arg(long, hide = true, requires = "config")]
    pub test_report: Option<PathBuf>,
    #[cfg(feature = "test-fixtures")]
    #[arg(long, hide = true, requires_all = ["config", "index_location"])]
    pub test_endpoint: Option<String>,
    #[cfg(feature = "test-fixtures")]
    #[arg(long, hide = true, requires = "test_report")]
    pub test_actions: Option<PathBuf>,
}

#[derive(Debug)]
pub struct StartupError {
    pub exit_code: u8,
    pub message: String,
}
impl From<archive_config::ConfigError> for StartupError {
    fn from(e: archive_config::ConfigError) -> Self {
        Self {
            exit_code: if e.is_invalid_input() { 2 } else { 1 },
            message: e.to_string(),
        }
    }
}
impl From<OperationError> for StartupError {
    fn from(e: OperationError) -> Self {
        Self {
            exit_code: 1,
            message: e.to_string(),
        }
    }
}

pub struct PreparedGui {
    pub paths: Paths,
    pub settings: Settings,
    pub namespace: String,
}
impl GuiArgs {
    pub fn prepare(&self, paths: Paths) -> Result<PreparedGui, StartupError> {
        let ignore = self.ignore.as_deref().map(archive_sync::parse_ignore_list);
        let settings = archive_config::load(
            &paths,
            self.config.as_deref(),
            Overrides {
                index_location: self.index_location.clone(),
                data_class: self.data_class.clone(),
                restore_tier: self.restore_tier.clone(),
                ignore,
                log_level: self.log_level.clone(),
            },
        )?;
        let namespace = "aws".to_string();
        #[cfg(feature = "test-fixtures")]
        let namespace = match &self.test_endpoint {
            Some(endpoint) => {
                archive_infra::S3Store::fixture_namespace(endpoint).map_err(StartupError::from)?
            }
            None => namespace,
        };
        Ok(PreparedGui {
            paths,
            settings,
            namespace,
        })
    }
}

/// Owns a disposable local query result, never an S3 client or CLI process.
pub struct RootListing {
    listing: Box<dyn IndexListing>,
}
impl RootListing {
    pub fn summary(&self) -> ListingSummary {
        self.listing.summary()
    }
    pub async fn page(&self, start: u64) -> Result<ListingPage, OperationError> {
        self.listing.page(start, PAGE_SIZE).await
    }
    pub async fn excluded(
        &self,
        start: u64,
    ) -> Result<archive_index::ExcludedPage, OperationError> {
        self.listing.excluded(start, PAGE_SIZE).await
    }
}
impl PreparedGui {
    /// Factory selection lives here at the frontend boundary; the domain query
    /// knows neither SQLite nor local/S3 file transport.
    pub async fn open_roots(&self, control: BrowseControl) -> Result<RootListing, OperationError> {
        let path = prepare_index_path(
            &self.paths.home,
            &self.settings.index_location.value,
            &self.namespace,
        )?;
        let index = SqliteIndex::open(path).await?;
        let result = index
            .browse(
                BrowseRequest {
                    target: BrowseTarget::Roots {
                        ignore: self.settings.ignore.value.clone(),
                    },
                    projection: BrowseProjection::Exact,
                    sort: SortSpec::default(),
                },
                control,
            )
            .await;
        // The listing owns its private query file, so no source connection or
        // read transaction needs to live as long as the window.
        let closed = index.close().await;
        let listing = result?;
        closed?;
        Ok(RootListing { listing })
    }
    pub fn index_label(&self) -> String {
        match &self.settings.index_location.value {
            Location::Local(path) => path.display().to_string(),
            Location::Remote(id) => id.uri(),
        }
    }
    pub fn log_enabled(&self, level: LogLevel) -> bool {
        self.settings.log_level.value.allows(level)
    }
}
