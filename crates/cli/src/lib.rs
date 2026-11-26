use archive_config::{ConfigKey, Overrides, Paths, Settings};
use archive_domain::Location;
use archive_index::{BrowseProjection, BrowseRequest, BrowseTarget, IndexScope, SortSpec};
use archive_sync::{Selection, parse_ignore_list};
use clap::{Args, Parser, Subcommand};
use std::{ffi::OsString, path::PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "private-archive",
    version,
    about = "Private archive manager",
    after_help = "Configuration priority: built-in defaults, TOML file, runtime options. Runtime overrides are not saved. Recursive operations include hidden names unless excluded by configured ignore or --ignore; --ignore '' clears configured exclusions. Results go to stdout, diagnostics/progress to stderr; progress requires a terminal and info logging. Exit codes: 0 success/warnings, 1 operation failure/unknown/no targets, 2 invalid input/configuration, 130 Ctrl+C. A valid empty reconcile succeeds. Ctrl+C stops new operations and allows up to 50 seconds for cleanup; a second signal forces exit. ls reads the saved copy without contacting S3; reconcile explicitly queries S3."
)]
pub struct Cli {
    #[cfg(feature = "test-fixtures")]
    #[arg(long,hide=true,requires_all=["config","index_location"])]
    pub test_endpoint: Option<String>,
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[arg(long, global = true, value_name = "PATH_OR_URI")]
    pub index_location: Option<OsString>,
    #[arg(long, global = true, value_parser = ["Bulk", "Standard"], help = "Override the configured restore tier")]
    pub restore_tier: Option<String>,
    #[arg(
        long,
        global = true,
        allow_hyphen_values = true,
        help = r"Comma-separated gitignore patterns; \, is a literal comma and \\ a backslash; empty clears configured patterns"
    )]
    pub ignore: Option<String>,
    #[arg(long, global = true, value_parser = ["error", "warn", "info", "debug", "trace"], help = "Override logging.level (default: info)")]
    pub log_level: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Cp(CopyArgs),
    /// Display effective settings, or explicitly create/edit the TOML file.
    Cfg(ConfigArgs),
    /// List the local index only; even the first call makes no store requests.
    Ls(ListArgs),
    /// Query the actual store and reconcile only the requested index scope.
    Reconcile(ReconcileArgs),
    /// Request restoration after resolving and validating all targets.
    Restore(RestoreArgs),
    /// Remove selected objects after one confirmation (or --auto-approve).
    Rm(RemoveArgs),
}

#[derive(Debug, Args)]
#[command(
    after_help = "Resolves the actual store before one confirmation for the URI, count and total size. Without --auto-approve, stdin must be a terminal; non-terminal input is rejected before store access. Only yes approves; another answer or EOF exits 1 without deleting. Hidden names are included unless explicitly ignored. Requests are serial in key order; the first failure/cancellation stops new requests. No retries or rollback."
)]
pub struct RemoveArgs {
    pub uri: OsString,
    #[arg(long, short = 'r')]
    pub recursive: bool,
    #[arg(long,requires="recursive",value_parser=clap::value_parser!(u32).range(1..))]
    pub recursive_depth: Option<u32>,
    #[arg(long)]
    pub auto_approve: bool,
}
pub struct PreparedRemove {
    pub paths: Paths,
    pub settings: Settings,
    pub request: archive_sync::RemoveRequest,
    #[cfg(feature = "test-fixtures")]
    pub test_endpoint: Option<String>,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Use a positive duration such as 7d or 1w. All global options precede the first URI. Each URI has its own -r/--recursive and --recursive-depth N. All targets are resolved/deduplicated and checked before any restore request. Already restoring/restored or immediately accessible objects fail the whole preflight. Acceptance is reported without waiting or polling."
)]
pub struct RestoreArgs {
    #[arg(long, required = true, value_name = "ND|NW")]
    pub restore_duration: archive_sync::RestoreDays,
    #[arg(required=true,num_args=1..,trailing_var_arg=true,allow_hyphen_values=true,value_name="URI [URI_OPTIONS]...")]
    pub targets: Vec<OsString>,
}
pub struct PreparedRestore {
    pub paths: Paths,
    pub settings: Settings,
    pub request: archive_sync::RestoreRequest,
    #[cfg(feature = "test-fixtures")]
    pub test_endpoint: Option<String>,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Queries the store explicitly; results are observation/removal/ignore counts. Use ls separately to list the saved copy. A successfully queried empty scope is success; all targets ignored is an error and does not prune old records. Hidden names are included by default. No automatic retries."
)]
pub struct ReconcileArgs {
    pub uri: OsString,
    #[arg(long, short = 'r')]
    pub recursive: bool,
    #[arg(long,requires="recursive",value_parser=clap::value_parser!(u32).range(1..))]
    pub recursive_depth: Option<u32>,
}
pub struct PreparedReconcile {
    pub paths: Paths,
    pub settings: Settings,
    pub request: archive_sync::ReconcileRequest,
    #[cfg(feature = "test-fixtures")]
    pub test_endpoint: Option<String>,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Reads the saved local copy, including stale observations. No S3 HEAD/GET/LIST, authentication or automatic fetch. Non-recursive queries prefer an exact object, then a virtual directory/root. Recursive queries return objects. Sort keys are prioritized left to right; unknown values are last, ties use URI ascending. Hidden names are included unless explicitly ignored. JSON always has all 10 fields; zero results emits [] and exits 1."
)]
pub struct ListArgs {
    pub uri: Option<OsString>,
    #[arg(long, short = 'r', requires = "uri")]
    pub recursive: bool,
    #[arg(long,requires="recursive",value_parser=clap::value_parser!(u32).range(1..))]
    pub recursive_depth: Option<u32>,
    #[arg(long)]
    pub detail: bool,
    #[arg(long)]
    pub json: bool,
    #[arg(long, default_value = "uri-asc", value_name = "KEY[,KEY...]")]
    pub sort: SortSpec,
}

pub struct PreparedList {
    pub paths: Paths,
    pub settings: Settings,
    pub request: BrowseRequest,
    pub detail: bool,
    pub json: bool,
    pub namespace: String,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: Option<ConfigCommand>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Create a commented default file; never overwrite an existing file.
    Init,
    /// Print the effective value of one setting.
    Get { key: ConfigKey },
    /// Save one explicit value; a missing file requires cfg init first.
    Set {
        key: ConfigKey,
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Remove an explicit value, allowing the built-in default to apply.
    Unset { key: ConfigKey },
}

pub enum PreparedCommand {
    Copy(PreparedCopy),
    Config(PreparedConfig),
    List(PreparedList),
    Reconcile(PreparedReconcile),
    Restore(PreparedRestore),
    Remove(PreparedRemove),
}

pub struct PreparedConfig {
    pub paths: Paths,
    pub settings: Settings,
    pub command: Option<ConfigCommand>,
}

#[derive(Debug)]
pub struct PreparationError {
    pub exit_code: u8,
    pub message: String,
}
impl std::fmt::Display for PreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl From<String> for PreparationError {
    fn from(message: String) -> Self {
        Self {
            exit_code: 2,
            message,
        }
    }
}
impl From<&str> for PreparationError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl From<archive_config::ConfigError> for PreparationError {
    fn from(error: archive_config::ConfigError) -> Self {
        Self {
            exit_code: if error.is_invalid_input() { 2 } else { 1 },
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Args)]
#[command(
    after_help = "All selected sources and destinations are resolved before transfer. The first operation failure stops new files; completed files remain. Downloads replace a destination only after body verification. Index and attribute failures are warnings. No automatic retry or resume. Hidden names are included unless explicitly ignored with configuration or --ignore; empty --ignore clears configured patterns. Results: stdout; diagnostics/progress: stderr. Exit: 0 success/warnings, 1 failure/unknown/no-targets, 2 invalid input, 130 Ctrl+C. Ctrl+C allows up to 50 seconds for cleanup; a second signal forces exit."
)]
pub struct CopyArgs {
    pub source: OsString,
    pub destination: OsString,
    #[arg(long, short = 'r')]
    pub recursive: bool,
    #[arg(long, value_name = "N", requires = "recursive", value_parser = clap::value_parser!(u32).range(1..))]
    pub recursive_depth: Option<u32>,
    #[arg(long, value_parser = ["standard", "archived"])]
    pub data_class: Option<String>,
}

pub struct PreparedCopy {
    pub home: PathBuf,
    pub cwd: PathBuf,
    #[cfg(feature = "test-fixtures")]
    pub test_endpoint: Option<String>,
    pub settings: Settings,
    pub selection: Selection,
    pub destination: Location,
}

impl Cli {
    pub fn prepare(self) -> Result<PreparedCommand, PreparationError> {
        let restore_targets = match &self.command {
            Command::Restore(args) => Some(restore_args::parse(&args.targets)?),
            _ => None,
        };
        let paths = Paths::from_env()?;
        let data_class = match &self.command {
            Command::Cp(args) => args.data_class.clone(),
            _ => None,
        };
        let settings = archive_config::load(
            &paths,
            self.config.as_deref(),
            Overrides {
                index_location: self.index_location,
                data_class,
                restore_tier: self.restore_tier,
                ignore: self.ignore.as_deref().map(parse_ignore_list),
                log_level: self.log_level,
            },
        )?;
        let args = match self.command {
            Command::Rm(args) => {
                let Location::Remote(root) = Location::parse(&args.uri, &paths.cwd)? else {
                    return Err("rm expects a store URI".into());
                };
                if !args.recursive && root.key.is_empty() {
                    return Err("a non-recursive rm URI requires an object key".into());
                }
                #[cfg(feature = "test-fixtures")]
                let root = if let Some(endpoint) = self.test_endpoint.as_deref() {
                    let mut root = root;
                    root.namespace = archive_infra::S3Store::fixture_namespace(endpoint)
                        .map_err(|e| e.to_string())?;
                    root
                } else {
                    root
                };
                let selection = Selection::new(
                    Location::Remote(root),
                    args.recursive,
                    args.recursive_depth.and_then(std::num::NonZeroU32::new),
                    &paths.cwd,
                )
                .map_err(|e| e.to_string())?;
                let request = archive_sync::RemoveRequest {
                    protected: None,
                    selection,
                    ignore: settings.ignore.value.clone(),
                    auto_approve: args.auto_approve,
                };
                return Ok(PreparedCommand::Remove(PreparedRemove {
                    paths,
                    settings,
                    request,
                    #[cfg(feature = "test-fixtures")]
                    test_endpoint: self.test_endpoint,
                }));
            }
            Command::Restore(args) => {
                let mut selections = vec![];
                for target in restore_targets.unwrap() {
                    let Location::Remote(root) = Location::parse(&target.uri, &paths.cwd)? else {
                        return Err("restore expects store URIs".into());
                    };
                    if !target.recursive && root.key.is_empty() {
                        return Err("a non-recursive restore URI requires an object key".into());
                    }
                    #[cfg(feature = "test-fixtures")]
                    let root = if let Some(endpoint) = self.test_endpoint.as_deref() {
                        let mut root = root;
                        root.namespace = archive_infra::S3Store::fixture_namespace(endpoint)
                            .map_err(|e| e.to_string())?;
                        root
                    } else {
                        root
                    };
                    selections.push(
                        Selection::new(
                            Location::Remote(root),
                            target.recursive,
                            target.depth.and_then(std::num::NonZeroU32::new),
                            &paths.cwd,
                        )
                        .map_err(|e| e.to_string())?,
                    );
                }
                let request = archive_sync::RestoreRequest {
                    protected: None,
                    selections,
                    ignore: settings.ignore.value.clone(),
                    days: args.restore_duration,
                    tier: settings.restore_tier.value,
                };
                return Ok(PreparedCommand::Restore(PreparedRestore {
                    paths,
                    settings,
                    request,
                    #[cfg(feature = "test-fixtures")]
                    test_endpoint: self.test_endpoint,
                }));
            }
            Command::Cp(args) => args,
            Command::Cfg(args) => {
                return Ok(PreparedCommand::Config(PreparedConfig {
                    paths,
                    settings,
                    command: args.command,
                }));
            }
            Command::Ls(args) => {
                let namespace = "aws".to_owned();
                #[cfg(feature = "test-fixtures")]
                let namespace = match self.test_endpoint.as_deref() {
                    Some(endpoint) => archive_infra::S3Store::fixture_namespace(endpoint)
                        .map_err(|e| e.to_string())?,
                    None => namespace,
                };
                let target = if let Some(uri) = args.uri {
                    let Location::Remote(mut root) = Location::parse(&uri, &paths.cwd)? else {
                        return Err("ls expects a store URI, not a local filesystem path".into());
                    };
                    root.namespace = namespace.clone();
                    BrowseTarget::Scope(IndexScope {
                        root,
                        recursive: args.recursive,
                        max_depth: args.recursive_depth.and_then(std::num::NonZeroU32::new),
                        ignore: settings.ignore.value.clone(),
                    })
                } else {
                    BrowseTarget::Roots {
                        ignore: settings.ignore.value.clone(),
                    }
                };
                return Ok(PreparedCommand::List(PreparedList {
                    paths,
                    settings,
                    request: BrowseRequest {
                        target,
                        projection: if args.recursive {
                            BrowseProjection::Objects
                        } else {
                            BrowseProjection::Exact
                        },
                        sort: args.sort,
                    },
                    detail: args.detail,
                    json: args.json,
                    namespace,
                }));
            }
            Command::Reconcile(args) => {
                let Location::Remote(root) = Location::parse(&args.uri, &paths.cwd)? else {
                    return Err("reconcile expects a store URI".into());
                };
                if !args.recursive && root.key.is_empty() {
                    return Err("non-recursive reconciliation requires an object key; use --recursive for a bucket".into());
                }
                #[cfg(feature = "test-fixtures")]
                let root = if let Some(endpoint) = self.test_endpoint.as_deref() {
                    let mut root = root;
                    root.namespace = archive_infra::S3Store::fixture_namespace(endpoint)
                        .map_err(|e| e.to_string())?;
                    root
                } else {
                    root
                };
                let selection = Selection::new(
                    Location::Remote(root),
                    args.recursive,
                    args.recursive_depth.and_then(std::num::NonZeroU32::new),
                    &paths.cwd,
                )
                .map_err(|e| e.to_string())?;
                let ignore = settings.ignore.value.clone();
                return Ok(PreparedCommand::Reconcile(PreparedReconcile {
                    paths,
                    settings,
                    request: archive_sync::ReconcileRequest { selection, ignore },
                    #[cfg(feature = "test-fixtures")]
                    test_endpoint: self.test_endpoint,
                }));
            }
        };
        let source = Location::parse(&args.source, &paths.cwd)?;
        let destination = Location::parse(&args.destination, &paths.cwd)?;
        if matches!(
            (&source, &destination),
            (Location::Local(_), Location::Local(_))
        ) {
            return Err("cp supports local→S3, S3→local and S3→S3".into());
        }
        if !args.recursive && matches!(&source, Location::Remote(id) if id.key.is_empty()) {
            return Err("a non-recursive S3 source requires an object key".into());
        }
        let selection = Selection::new(
            source,
            args.recursive,
            args.recursive_depth.and_then(std::num::NonZeroU32::new),
            &paths.cwd,
        )
        .map_err(|e| e.to_string())?;
        Ok(PreparedCommand::Copy(PreparedCopy {
            home: paths.home,
            cwd: paths.cwd,
            #[cfg(feature = "test-fixtures")]
            test_endpoint: self.test_endpoint,
            settings,
            selection,
            destination,
        }))
    }
}

mod cfg;
mod ls;
mod output;
mod reconcile;
mod remove;
mod restore;
mod restore_args;
mod run;
mod session;
mod startup;
pub use run::execute;
pub use startup::{display_parse_error, display_preparation_error};
