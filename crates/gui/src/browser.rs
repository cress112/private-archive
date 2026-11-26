//! Local-only column sessions and selection identity. No UI or store client.
use crate::{GuiArgs, PreparedGui};
use archive_domain::*;
use archive_index::*;
use archive_infra::{SqliteIndex, prepare_index_path};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Column {
    pub parent: Option<IndexEntry>,
    pub listing: Arc<dyn IndexListing>,
    pub selected: Option<IndexEntry>,
    pub selected_row: Option<u64>,
}
pub struct Browser {
    pub prepared: PreparedGui,
    pub sort: SortSpec,
    pub columns: Vec<Column>,
}
pub fn input(message: impl Into<String>) -> OperationError {
    OperationError::new(ErrorKind::Input, "gui", message)
}
impl Browser {
    pub async fn new(
        prepared: PreparedGui,
        control: BrowseControl,
    ) -> Result<Self, OperationError> {
        let mut result = Self {
            prepared,
            sort: SortSpec::default(),
            columns: vec![],
        };
        let root = result.column(None, control).await?;
        result.columns.push(root);
        Ok(result)
    }
    async fn index(&self) -> Result<SqliteIndex, OperationError> {
        let path = prepare_index_path(
            &self.prepared.paths.home,
            &self.prepared.settings.index_location.value,
            &self.prepared.namespace,
        )?;
        SqliteIndex::open(path).await
    }
    async fn column(
        &self,
        parent: Option<IndexEntry>,
        control: BrowseControl,
    ) -> Result<Column, OperationError> {
        let target = match &parent {
            None => BrowseTarget::Roots {
                ignore: self.prepared.settings.ignore.value.clone(),
            },
            Some(entry) => BrowseTarget::View {
                scope: IndexScope {
                    root: entry.id.clone(),
                    recursive: true,
                    max_depth: None,
                    ignore: self.prepared.settings.ignore.value.clone(),
                },
                ignore_base: entry.id.with_key(String::new()),
            },
        };
        let index = self.index().await?;
        let result = index
            .browse(
                BrowseRequest {
                    target,
                    projection: BrowseProjection::Children,
                    sort: self.sort.clone(),
                },
                control,
            )
            .await;
        let closed = index.close().await;
        let listing = result?;
        closed?;
        Ok(Column {
            parent,
            listing: Arc::from(listing),
            selected: None,
            selected_row: None,
        })
    }
    pub fn selected(&self) -> Option<&IndexEntry> {
        self.columns.iter().rev().find_map(|c| c.selected.as_ref())
    }
    pub fn top(&mut self) {
        self.columns.truncate(1);
        if let Some(c) = self.columns.first_mut() {
            c.selected = None;
            c.selected_row = None;
        }
    }
    pub async fn select(
        &mut self,
        column: usize,
        row: u64,
        control: BrowseControl,
    ) -> Result<(), OperationError> {
        let listing = self
            .columns
            .get(column)
            .ok_or_else(|| input("column no longer exists"))?
            .listing
            .clone();
        let Some(mut entry) = listing.page(row, 1).await?.entries.into_iter().next() else {
            return Err(input("row no longer exists"));
        };
        // Refresh object details through a local lookup. Prefixes are not sums.
        if entry.kind == IndexEntryKind::Object {
            let index = self.index().await?;
            let meta = index.get(&entry.id).await;
            let close = index.close().await;
            entry.metadata = meta?;
            close?;
        }
        let child = if entry.kind != IndexEntryKind::Object {
            Some(self.column(Some(entry.clone()), control).await?)
        } else {
            None
        };
        self.columns.truncate(column + 1);
        self.columns[column].selected = Some(entry);
        self.columns[column].selected_row = Some(row);
        if let Some(child) = child {
            self.columns.push(child);
        }
        Ok(())
    }
    /// Rebuild disposable results and restore by URI+kind, never by old row.
    pub async fn refresh(
        &mut self,
        prepared: Option<PreparedGui>,
        sort: Option<SortSpec>,
        control: BrowseControl,
    ) -> Result<(), OperationError> {
        let mut path: Vec<_> = self
            .columns
            .iter()
            .filter_map(|c| c.selected.clone())
            .collect();
        if let Some(prepared) = prepared {
            if self.prepared.settings.index_location.value != prepared.settings.index_location.value
            {
                path.clear();
                self.columns.clear();
            }
            self.prepared = prepared;
        }
        if let Some(sort) = sort {
            self.sort = sort;
        }
        // Keep the old view on a failed query; commit only a complete new path.
        let mut fresh = vec![self.column(None, control.clone()).await?];
        for entry in path {
            let last = fresh.last_mut().unwrap();
            let Some(row) = last.listing.position(&entry.id, entry.kind).await? else {
                break;
            };
            last.selected = Some(entry.clone());
            last.selected_row = Some(row);
            if entry.kind == IndexEntryKind::Object {
                let index = self.index().await?;
                let meta = index.get(&entry.id).await;
                let closed = index.close().await;
                last.selected.as_mut().unwrap().metadata = meta?;
                closed?;
                break;
            }
            fresh.push(self.column(Some(entry), control.clone()).await?);
        }
        self.columns = fresh;
        Ok(())
    }
    pub async fn reload(
        &mut self,
        args: &GuiArgs,
        control: BrowseControl,
    ) -> Result<(), crate::StartupError> {
        let prepared = args.prepare(archive_config::Paths {
            home: self.prepared.paths.home.clone(),
            cwd: self.prepared.paths.cwd.clone(),
        })?;
        self.refresh(Some(prepared), None, control).await?;
        Ok(())
    }
}

fn timestamp(time: Option<SystemTime>) -> String {
    time.and_then(|t| {
        let nanos = match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        };
        time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .ok()?
            .format(&time::format_description::well_known::Rfc3339)
            .ok()
    })
    .unwrap_or_else(|| "不明".into())
}
pub fn details(entry: Option<&IndexEntry>) -> Vec<(String, String)> {
    let Some(entry) = entry else { return vec![] };
    let m = entry.metadata.as_ref();
    let unknown = || "不明".to_string();
    let (state, until) = match m.map(|m| &m.restore_state) {
        Some(RestoreState::NotRequired) => ("不要", None),
        Some(RestoreState::Archived) => ("未復元", None),
        Some(RestoreState::Restoring) => ("復元中", None),
        Some(RestoreState::Restored { until }) => ("復元済み", *until),
        _ => ("不明", None),
    };
    vec![
        ("URI".into(), entry.uri()),
        (
            "種別".into(),
            match entry.kind {
                IndexEntryKind::Root => "バケット",
                IndexEntryKind::Directory => "フォルダ",
                IndexEntryKind::Object => "ファイル",
            }
            .into(),
        ),
        (
            "サイズ (bytes)".into(),
            m.and_then(|m| m.size)
                .map(|n| n.to_string())
                .unwrap_or_else(unknown),
        ),
        (
            "保存クラス".into(),
            m.and_then(|m| m.data_class.as_ref())
                .map(|c| match c {
                    StoredClass::Standard => "standard".into(),
                    StoredClass::Archived => "archived".into(),
                    StoredClass::Other(v) => v.clone(),
                })
                .unwrap_or_else(unknown),
        ),
        ("復元状態".into(), state.into()),
        ("復元期限".into(), timestamp(until)),
        (
            "元の更新日時".into(),
            timestamp(m.and_then(|m| m.original_mtime)),
        ),
        (
            "元の作成日時".into(),
            timestamp(m.and_then(|m| m.original_creation_time)),
        ),
        (
            "UNIX mode".into(),
            m.and_then(|m| m.unix_mode)
                .map(|n| format!("{n:04o}"))
                .unwrap_or_else(unknown),
        ),
        (
            "最終照会日時".into(),
            timestamp(m.and_then(|m| m.queried_at)),
        ),
    ]
}
