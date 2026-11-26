use archive_domain::{
    EntryKind, ErrorKind, Location, ObjectId, OperationError, RemoteObject, Warning,
    WarningCategory, compile_ignore,
};
use ignore::gitignore::Gitignore;
use std::{
    num::NonZeroU32,
    path::{Path, PathBuf},
};

fn input(message: impl Into<String>) -> OperationError {
    OperationError::new(ErrorKind::Input, "selection", message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Exact,
    Descendants,
}

#[derive(Debug, Clone)]
pub struct Selection {
    pub root: Location,
    pub mode: SelectionMode,
    pub max_depth: Option<NonZeroU32>,
    pub ignore_base: Location,
}

fn prefix(key: &str) -> String {
    if key.is_empty() {
        String::new()
    } else if key.ends_with('/') {
        key.to_owned()
    } else {
        format!("{key}/")
    }
}

fn same_store(left: &ObjectId, right: &ObjectId) -> bool {
    left.namespace == right.namespace
        && left.scheme == right.scheme
        && left.authority == right.authority
}

impl Selection {
    pub fn new(
        root: Location,
        recursive: bool,
        depth: Option<NonZeroU32>,
        cwd: &Path,
    ) -> Result<Self, OperationError> {
        if !recursive && depth.is_some() {
            return Err(input("--recursive-depth requires --recursive"));
        }
        if !cwd.is_absolute() {
            return Err(input("CWD must be absolute"));
        }
        let mode = if recursive {
            SelectionMode::Descendants
        } else {
            SelectionMode::Exact
        };
        let ignore_base = match &root {
            Location::Local(_) => Location::Local(cwd.into()),
            Location::Remote(id) => {
                let key = if recursive {
                    prefix(&id.key)
                } else {
                    id.key
                        .rsplit_once('/')
                        .map_or("", |(parent, _)| parent)
                        .to_owned()
                };
                Location::Remote(id.with_key(prefix(&key)))
            }
        };
        let root = match root {
            Location::Remote(id) if recursive => Location::Remote(id.with_key(prefix(&id.key))),
            root => root,
        };
        Ok(Self {
            root,
            mode,
            max_depth: depth,
            ignore_base,
        })
    }

    /// A single candidate is considered at a time; no collection of the range.
    pub fn contains(&self, location: &Location) -> bool {
        if self.mode == SelectionMode::Exact {
            return self.root == *location;
        }
        let depth = match (&self.root, location) {
            (Location::Local(root), Location::Local(path)) => path
                .strip_prefix(root)
                .ok()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.components().count()),
            (Location::Remote(root), Location::Remote(id)) if same_store(root, id) => id
                .key
                .strip_prefix(&root.key)
                .filter(|key| !key.is_empty())
                .map(|key| key.split('/').count()),
            _ => None,
        };
        depth.is_some_and(|n| self.max_depth.is_none_or(|max| n <= max.get() as usize))
    }
}

/// Matches only supplied patterns. The base remains fixed for this selection.
#[derive(Debug, Clone)]
pub struct IgnoreFilter {
    base: Location,
    matcher: Gitignore,
}

impl IgnoreFilter {
    pub fn new(base: Location, patterns: &[String]) -> Result<Self, OperationError> {
        // All matching happens on explicit relative paths, including remote keys.
        let matcher = compile_ignore(Path::new(""), patterns).map_err(input)?;
        Ok(Self { base, matcher })
    }

    pub fn is_ignored(&self, location: &Location, is_dir: bool) -> Result<bool, OperationError> {
        let relative = match (&self.base, location) {
            (Location::Local(base), Location::Local(path)) => relative_local(base, path)?,
            (Location::Remote(base), Location::Remote(id)) if same_store(base, id) => {
                let key = id
                    .key
                    .strip_prefix(&base.key)
                    .ok_or_else(|| input("remote candidate is outside its ignore base"))?;
                // Keep the raw key when testing each parent; Path::components
                // would discard repeated separators and dot segments.
                for (offset, _) in key.match_indices('/') {
                    if offset > 0
                        && self
                            .matcher
                            .matched(Path::new(&key[..offset]), true)
                            .is_ignore()
                    {
                        return Ok(true);
                    }
                }
                return Ok(self.matcher.matched(Path::new(key), is_dir).is_ignore());
            }
            _ => return Err(input("candidate and ignore base refer to different stores")),
        };
        // Walk top-down: a child negation cannot resurrect an excluded directory.
        let mut current = PathBuf::new();
        let components: Vec<_> = relative.components().collect();
        for (i, component) in components.iter().enumerate() {
            current.push(component);
            if self
                .matcher
                .matched(&current, i + 1 < components.len() || is_dir)
                .is_ignore()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn relative_local(base: &Path, path: &Path) -> Result<PathBuf, OperationError> {
    // Keep paths outside CWD expressible as ../...; do not canonicalize symlinks.
    let left: Vec<_> = base.components().collect();
    let right: Vec<_> = path.components().collect();
    let common = left.iter().zip(&right).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return Err(input("local paths must be absolute"));
    }
    let mut relative = PathBuf::new();
    for _ in common..left.len() {
        relative.push("..");
    }
    for component in &right[common..] {
        relative.push(component);
    }
    Ok(relative)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CopyRoute {
    Upload {
        #[serde(with = "archive_domain::path_bytes")]
        local_source: PathBuf,
        remote_destination: RemoteObject,
    },
    Download {
        remote_source: RemoteObject,
        #[serde(with = "archive_domain::path_bytes")]
        local_destination: PathBuf,
    },
    Remote {
        remote_source: RemoteObject,
        remote_destination: RemoteObject,
    },
}

impl CopyRoute {
    pub fn new(source: Location, destination: Location) -> Result<Self, OperationError> {
        match (source, destination) {
            (Location::Local(local_source), Location::Remote(remote_destination)) => {
                Ok(Self::Upload {
                    local_source,
                    remote_destination: remote_destination.require_object().map_err(input)?,
                })
            }
            (Location::Remote(remote_source), Location::Local(local_destination)) => {
                Ok(Self::Download {
                    remote_source: remote_source.require_object().map_err(input)?,
                    local_destination,
                })
            }
            (Location::Remote(remote_source), Location::Remote(remote_destination)) => {
                Ok(Self::Remote {
                    remote_source: remote_source.require_object().map_err(input)?,
                    remote_destination: remote_destination.require_object().map_err(input)?,
                })
            }
            _ => Err(input("cp supports local→S3, S3→local and S3→S3")),
        }
    }
}

/// Resolves destinations one at a time. Probe the local destination once to set
/// destination_is_directory; per-object collisions belong to the disk manifest.
pub struct CopyLayout {
    pub source: Selection,
    pub destination: Location,
    destination_is_directory: bool,
    source_filter: IgnoreFilter,
    destination_filter: IgnoreFilter,
}

pub enum Candidate {
    Copy(CopyRoute),
    Skip(Warning),
    OutsideSelection,
}

impl CopyLayout {
    pub fn new(
        source: Selection,
        destination: Location,
        destination_is_directory: bool,
        cwd: &Path,
        patterns: &[String],
    ) -> Result<Self, OperationError> {
        if matches!(
            (&source.root, &destination),
            (Location::Local(_), Location::Local(_))
        ) {
            return Err(input("local→local is unsupported"));
        }
        let source_filter = IgnoreFilter::new(source.ignore_base.clone(), patterns)?;
        let destination_base = Selection::new(
            destination.clone(),
            source.mode == SelectionMode::Descendants,
            None,
            cwd,
        )?
        .ignore_base;
        let destination_filter = IgnoreFilter::new(destination_base, patterns)?;
        Ok(Self {
            source,
            destination,
            destination_is_directory,
            source_filter,
            destination_filter,
        })
    }

    pub fn resolve(&self, source: &Location, kind: EntryKind) -> Result<Candidate, OperationError> {
        if !self.source.contains(source) {
            return Ok(Candidate::OutsideSelection);
        }
        // Ignore before rejecting symlinks/specials, including directly named ones.
        if self
            .source_filter
            .is_ignored(source, kind == EntryKind::Directory)?
        {
            return Ok(ignored(source));
        }
        let destination = self.destination_for(source)?;
        if self
            .destination_filter
            .is_ignored(&destination, kind == EntryKind::Directory)?
        {
            return Ok(ignored(&destination));
        }
        match kind {
            EntryKind::Symlink | EntryKind::Special => Err(OperationError::new(
                ErrorKind::Unsupported,
                "selection",
                "symlinks and special files are unsupported",
            )
            .at(source)),
            EntryKind::Directory if self.source.mode == SelectionMode::Exact => {
                Err(input("copying a directory requires --recursive"))
            }
            EntryKind::Directory => Ok(Candidate::OutsideSelection), // no empty directory objects
            EntryKind::File => Ok(Candidate::Copy(CopyRoute::new(
                source.clone(),
                destination,
            )?)),
        }
    }

    pub fn source_filter(&self) -> &IgnoreFilter {
        &self.source_filter
    }

    fn destination_for(&self, source: &Location) -> Result<Location, OperationError> {
        if self.source.mode == SelectionMode::Exact {
            let append = match &self.destination {
                Location::Remote(id) => id.key.is_empty() || id.key.ends_with('/'),
                Location::Local(path) => {
                    self.destination_is_directory
                        || path.as_os_str().as_encoded_bytes().ends_with(b"/")
                }
            };
            if !append {
                return Ok(self.destination.clone());
            }
        }
        let relative = if self.source.mode == SelectionMode::Descendants {
            match (&self.source.root, source) {
                (Location::Local(root), Location::Local(path)) => path
                    .strip_prefix(root)
                    .map_err(|_| input("source is outside root"))?
                    .as_os_str()
                    .to_owned(),
                (Location::Remote(root), Location::Remote(id)) => id
                    .key
                    .strip_prefix(&root.key)
                    .ok_or_else(|| input("source is outside prefix"))?
                    .into(),
                _ => return Err(input("source store mismatch")),
            }
        } else {
            match source {
                Location::Local(path) => path
                    .file_name()
                    .ok_or_else(|| input("source has no file name"))?
                    .to_owned(),
                Location::Remote(id) => id
                    .key
                    .rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| input("source key has no file name"))?
                    .into(),
            }
        };
        match &self.destination {
            Location::Remote(id) => {
                let append = self.source.mode == SelectionMode::Descendants
                    || id.key.is_empty()
                    || id.key.ends_with('/');
                let key = if append {
                    let name = relative
                        .to_str()
                        .ok_or_else(|| input("non-UTF-8 local names cannot become S3 keys"))?;
                    format!("{}{name}", prefix(&id.key))
                } else {
                    id.key.clone()
                };
                Ok(Location::Remote(id.with_key(key)))
            }
            Location::Local(path) => {
                let append = self.source.mode == SelectionMode::Descendants
                    || self.destination_is_directory
                    || path.as_os_str().as_encoded_bytes().ends_with(b"/");
                if !append {
                    return Ok(Location::Local(path.clone()));
                }
                let name = relative
                    .to_str()
                    .ok_or_else(|| input("remote key is not UTF-8"))?;
                if name.split('/').any(|part| {
                    part.is_empty() || part == "." || part == ".." || part.contains('\0')
                }) {
                    return Err(input(
                        "S3 key cannot be placed locally without path traversal or normalization",
                    ));
                }
                Ok(Location::Local(path.join(name)))
            }
        }
    }
}

fn ignored(location: &Location) -> Candidate {
    Candidate::Skip(Warning {
        category: WarningCategory::Ignored,
        stage: "selection",
        uri: location.display(),
        code: None,
        message: "skipped by ignore pattern".into(),
    })
}

pub use archive_domain::parse_ignore_list;

impl CopyRoute {
    pub fn source(&self) -> Location {
        match self {
            Self::Upload { local_source, .. } => Location::Local(local_source.clone()),
            Self::Download { remote_source, .. } | Self::Remote { remote_source, .. } => {
                Location::Remote(remote_source.id().clone())
            }
        }
    }
    pub fn destination(&self) -> Location {
        match self {
            Self::Download {
                local_destination, ..
            } => Location::Local(local_destination.clone()),
            Self::Upload {
                remote_destination, ..
            }
            | Self::Remote {
                remote_destination, ..
            } => Location::Remote(remote_destination.id().clone()),
        }
    }
}
