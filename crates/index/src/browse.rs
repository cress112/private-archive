use crate::{IndexFuture, IndexScope};
use archive_domain::*;
use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortTerm {
    NameAsc,
    NameDesc,
    SizeAsc,
    SizeDesc,
    UriAsc,
    UriDesc,
}
impl SortTerm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NameAsc => "name-asc",
            Self::NameDesc => "name-desc",
            Self::SizeAsc => "size-asc",
            Self::SizeDesc => "size-desc",
            Self::UriAsc => "uri-asc",
            Self::UriDesc => "uri-desc",
        }
    }
    fn field(self) -> u8 {
        match self {
            Self::NameAsc | Self::NameDesc => 0,
            Self::SizeAsc | Self::SizeDesc => 1,
            _ => 2,
        }
    }
}
impl FromStr for SortTerm {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "name-asc" => Ok(Self::NameAsc),
            "name-desc" => Ok(Self::NameDesc),
            "size-asc" => Ok(Self::SizeAsc),
            "size-desc" => Ok(Self::SizeDesc),
            "uri-asc" => Ok(Self::UriAsc),
            "uri-desc" => Ok(Self::UriDesc),
            _ => Err(
                "sort must use name-asc/name-desc, size-asc/size-desc or uri-asc/uri-desc".into(),
            ),
        }
    }
}
#[derive(Debug, Clone)]
pub struct SortSpec(Vec<SortTerm>);
impl Default for SortSpec {
    fn default() -> Self {
        Self(vec![SortTerm::UriAsc])
    }
}
impl SortSpec {
    pub fn new(terms: Vec<SortTerm>) -> Result<Self, String> {
        if terms.is_empty() {
            return Err("at least one sort condition is required".into());
        }
        let mut seen = 0u8;
        for term in &terms {
            let flag = 1 << term.field();
            if seen & flag != 0 {
                return Err("a sort field may only be specified once".into());
            }
            seen |= flag;
        }
        Ok(Self(terms))
    }
    pub fn terms(&self) -> &[SortTerm] {
        &self.0
    }
}
impl FromStr for SortSpec {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Self::new(s.split(',').map(str::parse).collect::<Result<_, _>>()?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexEntryKind {
    Object,
    Directory,
    Root,
}
impl IndexEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Directory => "directory",
            Self::Root => "root",
        }
    }
}
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub id: ObjectId,
    pub kind: IndexEntryKind,
    /// Only real objects have metadata; virtual ancestors have unknown values.
    pub metadata: Option<ObjectMetadata>,
}
impl IndexEntry {
    pub fn uri(&self) -> String {
        if self.kind == IndexEntryKind::Root {
            format!("{}://{}", self.id.scheme, self.id.authority)
        } else {
            self.id.uri()
        }
    }
    pub fn name(&self) -> &str {
        if self.kind == IndexEntryKind::Root {
            return &self.id.authority;
        }
        let key = if self.kind == IndexEntryKind::Directory {
            self.id.key.strip_suffix('/').unwrap_or(&self.id.key)
        } else {
            &self.id.key
        };
        key.rsplit('/').next().unwrap_or("")
    }
}
#[derive(Debug, Clone)]
pub enum BrowseTarget {
    Roots {
        ignore: Vec<String>,
    },
    Scope(IndexScope),
    /// A GUI column keeps its ignore origin when navigating deeper.
    View {
        scope: IndexScope,
        ignore_base: ObjectId,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowseProjection {
    Exact,
    Objects,
    Children,
}
#[derive(Debug, Clone)]
pub struct BrowseRequest {
    pub target: BrowseTarget,
    pub projection: BrowseProjection,
    pub sort: SortSpec,
}

#[derive(Clone, Default)]
pub struct BrowseControl(Arc<AtomicBool>);
impl BrowseControl {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn check(&self) -> Result<(), OperationError> {
        if self.0.load(Ordering::Acquire) {
            Err(OperationError::new(
                ErrorKind::Cancelled,
                "index-browse",
                "listing cancelled",
            ))
        } else {
            Ok(())
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub struct ListingSummary {
    pub entries: u64,
    pub ignored: u64,
    pub directly_ignored: bool,
}
pub struct ListingPage {
    pub entries: Vec<IndexEntry>,
    pub next: Option<u64>,
}
pub struct ExcludedPage {
    pub ids: Vec<ObjectId>,
    pub next: Option<u64>,
}
/// Local-only query projection. File formats and sort engines belong to adapters.
pub trait IndexBrowse: Send + Sync {
    fn browse<'a>(
        &'a self,
        request: BrowseRequest,
        control: BrowseControl,
    ) -> IndexFuture<'a, Box<dyn IndexListing>>;
}
/// A bounded, disposable query result; never an operation journal or a store index.
pub trait IndexListing: Send + Sync {
    fn summary(&self) -> ListingSummary;
    fn page<'a>(&'a self, start: u64, limit: usize) -> IndexFuture<'a, ListingPage>;
    fn excluded<'a>(&'a self, start: u64, limit: usize) -> IndexFuture<'a, ExcludedPage>;
    fn position<'a>(
        &'a self,
        id: &'a ObjectId,
        kind: IndexEntryKind,
    ) -> IndexFuture<'a, Option<u64>> {
        Box::pin(async move {
            let mut start = 0;
            loop {
                let page = self.page(start, crate::MAX_PAGE_ENTRIES).await?;
                if let Some(n) = page
                    .entries
                    .iter()
                    .position(|e| &e.id == id && e.kind == kind)
                {
                    return Ok(Some(start + n as u64));
                }
                match page.next {
                    Some(next) => start = next,
                    None => return Ok(None),
                }
            }
        })
    }
}
