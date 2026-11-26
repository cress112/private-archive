use crate::{
    PreparedList,
    output::{Console, output_work as emit},
};
use archive_domain::*;
use archive_index::*;
use archive_infra::{SqliteIndex, prepare_index_path};
use archive_sync::{CancelContext, ancillary};
use serde::Serialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub async fn execute(prepared: PreparedList) -> u8 {
    let session =
        match crate::session::Session::start("ls", prepared.settings.log_level.value).await {
            Ok(s) => s,
            Err(_) => return 1,
        };
    let console = session.console.clone();
    let cancel = session.cancel.clone();
    let control = BrowseControl::default();
    let query = control.clone();
    let signal = cancel.clone();
    let bridge = tokio::spawn(async move {
        signal.cancelled().await;
        query.cancel();
    });
    let mut started = false;
    let mut result = if prepared.json {
        match crate::session::cancellable_output(console.raw("["), &cancel).await {
            Ok(()) => {
                started = true;
                run(&prepared, &console, &cancel, control).await
            }
            Err(e) => Err(e),
        }
    } else {
        run(&prepared, &console, &cancel, control).await
    };
    if result.is_err() {
        cancel.record_failure();
    }
    if started {
        let close = if result.is_ok() {
            Some(emit(console.result("]"), &cancel).await)
        } else {
            ancillary(
                &cancel,
                Duration::from_secs(50),
                Duration::ZERO,
                console.result("]"),
            )
            .await
        };
        if result.is_ok() {
            result = match close {
                Some(r) => r,
                None => Err(OperationError::new(
                    ErrorKind::Io,
                    "ls-json",
                    "could not finish JSON output",
                )),
            };
        }
    }
    bridge.abort();
    let _ = bridge.await;
    session.finish(result).await
}

async fn run(
    prepared: &PreparedList,
    console: &Console,
    cancel: &CancelContext,
    control: BrowseControl,
) -> Result<(), OperationError> {
    cancel.check("ls")?;
    let path = prepare_index_path(
        &prepared.paths.home,
        &prepared.settings.index_location.value,
        &prepared.namespace,
    )?;
    // Pure filesystem setup: neither SDK construction nor credential resolution.
    let index = SqliteIndex::open(path).await?;
    let result = list(prepared, console, cancel, &index, control).await;
    if result.is_err() {
        cancel.record_failure();
    }
    let close = ancillary(
        cancel,
        Duration::from_secs(3),
        Duration::ZERO,
        index.close(),
    )
    .await;
    match result {
        Err(e) => Err(e),
        Ok(()) => close.unwrap_or_else(|| {
            Err(OperationError::new(
                ErrorKind::Io,
                "ls-index-close",
                "index worker did not finish",
            ))
        }),
    }
}

async fn list(
    prepared: &PreparedList,
    console: &Console,
    cancel: &CancelContext,
    index: &dyn IndexBrowse,
    control: BrowseControl,
) -> Result<(), OperationError> {
    let listing = index.browse(prepared.request.clone(), control).await?;
    let summary = listing.summary();
    if summary.ignored > 0 {
        if summary.directly_ignored
            && let BrowseTarget::Scope(scope) = &prepared.request.target
        {
            let _ = emit(
                console.log(
                    LogLevel::Warn,
                    &format!("ignored explicit target {:?}", scope.root.uri()),
                ),
                cancel,
            )
            .await;
        }
        let _ = emit(
            console.log(
                LogLevel::Info,
                &format!("ignore: {} observed exclusions", summary.ignored),
            ),
            cancel,
        )
        .await;
        if prepared.settings.log_level.value.allows(LogLevel::Debug) {
            let mut start = 0;
            loop {
                cancel.check("ls")?;
                let page = listing.excluded(start, MAX_PAGE_ENTRIES).await?;
                for id in page.ids {
                    let _ = emit(
                        console.log(LogLevel::Debug, &format!("ignored {:?}", id.uri())),
                        cancel,
                    )
                    .await;
                }
                match page.next {
                    Some(next) => start = next,
                    None => break,
                }
            }
        }
    }
    if summary.entries == 0 {
        return Err(OperationError::new(
            ErrorKind::NotFound,
            "ls",
            "no targets; requested listing could not be performed",
        ));
    }
    let mut start = 0;
    let mut count = 0;
    loop {
        cancel.check("ls")?;
        let page = listing.page(start, MAX_PAGE_ENTRIES).await?;
        for entry in page.entries {
            cancel.check("ls")?;
            if prepared.json {
                let row = DisplayRow::from_entry(&entry)?;
                let text = serde_json::to_string(&row).map_err(|e| {
                    OperationError::new(ErrorKind::Integrity, "ls-json", e.to_string())
                })?;
                emit(
                    console.raw(&format!("{}{text}", if count == 0 { "" } else { "," })),
                    cancel,
                )
                .await?;
            } else if prepared.detail {
                if count == 0 {
                    emit(console.result("uri\tkind\tsize_bytes\tdata_class\trestore_state\trestore_until\toriginal_mtime\toriginal_creation_time\tunix_mode\tqueried_at"),cancel).await?;
                }
                emit(
                    console.result(&DisplayRow::from_entry(&entry)?.detail()),
                    cancel,
                )
                .await?;
            } else {
                emit(console.result(&escape(&entry.uri())), cancel).await?;
            }
            count += 1;
        }
        match page.next {
            Some(next) => start = next,
            None => break,
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct DisplayRow {
    uri: String,
    kind: &'static str,
    size_bytes: Option<u64>,
    data_class: Option<String>,
    restore_state: Option<&'static str>,
    restore_until: Option<String>,
    original_mtime: Option<String>,
    original_creation_time: Option<String>,
    unix_mode: Option<String>,
    queried_at: Option<String>,
}
fn timestamp(time: Option<SystemTime>) -> Result<Option<String>, OperationError> {
    time.map(|time| {
        let nanos = match time.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        };
        let date = time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .map_err(|e| OperationError::new(ErrorKind::Integrity, "ls-time", e.to_string()))?;
        date.to_offset(time::UtcOffset::UTC)
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| OperationError::new(ErrorKind::Integrity, "ls-time", e.to_string()))
    })
    .transpose()
}
impl DisplayRow {
    fn from_entry(entry: &IndexEntry) -> Result<Self, OperationError> {
        let meta = entry.metadata.as_ref();
        let state = meta.map(|m| &m.restore_state);
        let restore_state = match state {
            Some(RestoreState::NotRequired) => Some("not_required"),
            Some(RestoreState::Archived) => Some("archived"),
            Some(RestoreState::Restoring) => Some("restoring"),
            Some(RestoreState::Restored { .. }) => Some("restored"),
            _ => None,
        };
        let until = match state {
            Some(RestoreState::Restored { until }) => *until,
            _ => None,
        };
        Ok(Self {
            uri: entry.uri(),
            kind: entry.kind.as_str(),
            size_bytes: meta.and_then(|m| m.size),
            data_class: meta.and_then(|m| m.data_class.as_ref()).map(|c| match c {
                StoredClass::Standard => "standard".into(),
                StoredClass::Archived => "archived".into(),
                StoredClass::Other(s) => s.clone(),
            }),
            restore_state,
            restore_until: timestamp(until)?,
            original_mtime: timestamp(meta.and_then(|m| m.original_mtime))?,
            original_creation_time: timestamp(meta.and_then(|m| m.original_creation_time))?,
            unix_mode: meta.and_then(|m| m.unix_mode).map(|m| format!("{m:04o}")),
            queried_at: timestamp(meta.and_then(|m| m.queried_at))?,
        })
    }
    fn detail(&self) -> String {
        let unknown = "不明";
        let state = match self.restore_state {
            Some("not_required") => "不要",
            Some("archived") => "未復元",
            Some("restoring") => "復元中",
            Some("restored") => "復元済み",
            _ => unknown,
        };
        [
            self.uri.clone(),
            self.kind.into(),
            self.size_bytes
                .map(|n| n.to_string())
                .unwrap_or_else(|| unknown.into()),
            self.data_class.as_deref().unwrap_or(unknown).into(),
            state.into(),
            self.restore_until.as_deref().unwrap_or(unknown).into(),
            self.original_mtime.as_deref().unwrap_or(unknown).into(),
            self.original_creation_time
                .as_deref()
                .unwrap_or(unknown)
                .into(),
            self.unix_mode.as_deref().unwrap_or(unknown).into(),
            self.queried_at.as_deref().unwrap_or(unknown).into(),
        ]
        .iter()
        .map(|s| escape(s))
        .collect::<Vec<_>>()
        .join("\t")
    }
}
fn escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_control() || c == '\\' {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
