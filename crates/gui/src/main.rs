use archive_domain::ObjectId;
use archive_index::{BrowseControl, IndexEntryKind, SortSpec, SortTerm};
use archive_sync::CancelContext;
use clap::Parser;
use private_archive_gui::{GuiArgs, cache::RowCache};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
slint::include_modules!();
mod ui_model;
#[cfg(feature = "test-fixtures")]
mod ui_test;
mod ui_worker;

const SORTS: [SortTerm; 6] = [
    SortTerm::NameAsc,
    SortTerm::NameDesc,
    SortTerm::SizeAsc,
    SortTerm::SizeDesc,
    SortTerm::UriAsc,
    SortTerm::UriDesc,
];
#[derive(Default)]
struct Shared {
    wake: tokio::sync::Notify,
    generation: AtomicU64,
    operation_id: AtomicU64,
    closing: AtomicBool,
    browse: Mutex<Option<BrowseControl>>,
    operation: Mutex<Option<CancelContext>>,
    shutdown_deadline: Mutex<Option<tokio::time::Instant>>,
}
impl Shared {
    fn advance(&self) -> u64 {
        if let Some(control) = self.browse.lock().unwrap().take() {
            control.cancel();
        }
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }
    fn cancel(&self) {
        if let Some(cancel) = self.operation.lock().unwrap().as_ref() {
            cancel.cancel();
        }
    }
    fn begin_close(&self) -> Option<tokio::time::Instant> {
        if self.closing.swap(true, Ordering::AcqRel) {
            return None;
        }
        let cancel = self.operation.lock().unwrap().clone();
        if let Some(cancel) = &cancel {
            cancel.cancel();
        }
        let deadline = cancel
            .as_ref()
            .map(CancelContext::cleanup_deadline)
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(50));
        *self.shutdown_deadline.lock().unwrap() = Some(deadline);
        self.advance();
        self.wake.notify_one();
        Some(deadline)
    }
}
enum Command {
    Select(u64, usize, ObjectId, IndexEntryKind),
    Top(u64),
    Reload(u64),
    Sort(u64, SortSpec),
    Page(u64, usize, usize),
    Reconcile(String),
    Settings,
    CreateSettings,
    SaveSettings(Vec<(archive_config::ConfigKey, Option<String>)>),
}
struct UiState {
    cache: Rc<RefCell<RowCache>>,
    models: RefCell<Vec<Rc<ui_model::PagedModel>>>,
    sorts: RefCell<Vec<SortTerm>>,
}
fn main() {
    match run(GuiArgs::parse()) {
        Ok(code) => std::process::exit(code.into()),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
fn enqueue(
    ui: &AppWindow,
    sender: &mpsc::Sender<Command>,
    shared: &Shared,
    command: Command,
) -> bool {
    if shared.closing.load(Ordering::Acquire) {
        return false;
    }
    if sender.try_send(command).is_err() {
        ui.set_loading(false);
        ui.set_error_message("要求が混み合っています。操作をもう一度実行してください。".into());
        return false;
    }
    true
}
fn run(args: GuiArgs) -> Result<u8, Box<dyn std::error::Error>> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()?;
    let ui = AppWindow::new()?;
    let shared = Arc::new(Shared::default());
    let exit = Arc::new(AtomicU8::new(0));
    let (sender, receiver) = mpsc::channel(32);
    let state = Rc::new(UiState {
        cache: Rc::new(RefCell::new(RowCache::default())),
        models: RefCell::new(vec![]),
        sorts: RefCell::new(vec![SortTerm::UriAsc]),
    });
    // Store UI-only state on the UI thread; worker messages only carry plain data.
    UI_STATE.with(|slot| *slot.borrow_mut() = Some(state.clone()));
    let weak = ui.as_weak();
    let tx = sender.clone();
    let s = shared.clone();
    ui.on_select_row(move |column, row| {
        if column < 0 || row < 0 {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            let Some(data) = ui
                .get_columns()
                .row_data(column as usize)
                .and_then(|c| c.rows.row_data(row as usize))
            else {
                return;
            };
            if !data.loaded {
                return;
            }
            let Ok(mut id) = ObjectId::parse_s3(data.uri.as_str()) else {
                return;
            };
            id.namespace = data.namespace.to_string();
            let kind = match data.kind.as_str() {
                "root" => IndexEntryKind::Root,
                "directory" => IndexEntryKind::Directory,
                _ => IndexEntryKind::Object,
            };
            let g = s.advance();
            ui.set_loading(true);
            enqueue(&ui, &tx, &s, Command::Select(g, column as usize, id, kind));
        }
    });
    let weak = ui.as_weak();
    let tx = sender.clone();
    let s = shared.clone();
    ui.on_reload(move || {
        if let Some(ui) = weak.upgrade() {
            let g = s.advance();
            ui.set_loading(true);
            enqueue(&ui, &tx, &s, Command::Reload(g));
        }
    });
    let weak = ui.as_weak();
    let tx = sender.clone();
    let s = shared.clone();
    ui.on_select_top(move || {
        if let Some(ui) = weak.upgrade() {
            let g = s.advance();
            ui.set_loading(true);
            enqueue(&ui, &tx, &s, Command::Top(g));
        }
    });
    let weak = ui.as_weak();
    let tx = sender.clone();
    let s = shared.clone();
    ui.on_reconcile(move || {
        if let Some(ui) = weak.upgrade()
            && !ui.get_reconciling()
        {
            *s.operation.lock().unwrap() = Some(CancelContext::new());
            ui.set_reconciling(true);
            ui.set_error_message("".into());
            ui.set_status("再照合開始…".into());
            if !enqueue(&ui, &tx, &s, Command::Reconcile(ui.get_depth().to_string())) {
                s.operation.lock().unwrap().take();
                ui.set_reconciling(false);
            }
        }
    });
    let s = shared.clone();
    let weak = ui.as_weak();
    ui.on_cancel_operation(move || {
        s.cancel();
        if let Some(ui) = weak.upgrade()
            && ui.get_reconciling()
        {
            ui.set_status("取消中… 取得済みの反映を保持します".into());
        }
    });
    let weak = ui.as_weak();
    let tx = sender.clone();
    let s = shared.clone();
    ui.on_open_settings(move || {
        if let Some(ui) = weak.upgrade() {
            enqueue(&ui, &tx, &s, Command::Settings);
        }
    });
    let weak = ui.as_weak();
    let tx = sender.clone();
    let s = shared.clone();
    ui.on_create_settings(move || {
        if let Some(ui) = weak.upgrade() {
            enqueue(&ui, &tx, &s, Command::CreateSettings);
        }
    });
    let weak = ui.as_weak();
    ui.on_close_settings(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_settings_open(false);
        }
    });
    let weak = ui.as_weak();
    ui.on_edit_setting(move |index, value| {
        if let Some(ui) = weak.upgrade()
            && let Some(mut row) = ui.get_settings().row_data(index as usize)
        {
            row.value = value;
            row.explicit = true;
            row.dirty = true;
            ui.get_settings().set_row_data(index as usize, row);
        }
    });
    let weak = ui.as_weak();
    ui.on_reset_setting(move |index| {
        if let Some(ui) = weak.upgrade()
            && let Some(mut row) = ui.get_settings().row_data(index as usize)
        {
            row.value = "".into();
            row.explicit = false;
            row.dirty = true;
            ui.get_settings().set_row_data(index as usize, row);
        }
    });
    let weak = ui.as_weak();
    let tx = sender.clone();
    let s = shared.clone();
    ui.on_save_settings(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_loading(true);
            let rows = ui
                .get_settings()
                .iter()
                .filter(|r| r.dirty)
                .filter_map(|r| {
                    r.key.parse().ok().map(|key| {
                        (
                            key,
                            if r.explicit {
                                Some(r.value.to_string())
                            } else {
                                None
                            },
                        )
                    })
                })
                .collect();
            enqueue(&ui, &tx, &s, Command::SaveSettings(rows));
        }
    });
    let weak = ui.as_weak();
    let st = state.clone();
    ui.on_open_sort(move || {
        if let Some(ui) = weak.upgrade() {
            set_sort_rows(&ui, &st.sorts.borrow());
            ui.set_sort_open(true);
        }
    });
    let weak = ui.as_weak();
    ui.on_close_sort(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_sort_open(false);
        }
    });
    let weak = ui.as_weak();
    ui.on_edit_sort(move |i, value| {
        if let Some(ui) = weak.upgrade() {
            ui.get_sorts()
                .set_row_data(i as usize, SortData { choice: value });
        }
    });
    let weak = ui.as_weak();
    ui.on_move_sort(move |i, delta| {
        if let Some(ui) = weak.upgrade() {
            let mut rows: Vec<_> = ui.get_sorts().iter().collect();
            let j = i + delta;
            if i >= 0 && j >= 0 && (i as usize) < rows.len() && (j as usize) < rows.len() {
                rows.swap(i as usize, j as usize);
                ui.set_sorts(ModelRc::new(VecModel::from(rows)));
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_remove_sort(move |i| {
        if let Some(ui) = weak.upgrade() {
            let mut rows: Vec<_> = ui.get_sorts().iter().collect();
            if rows.len() > 1 && i >= 0 && (i as usize) < rows.len() {
                rows.remove(i as usize);
                ui.set_sorts(ModelRc::new(VecModel::from(rows)));
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_add_sort(move || {
        if let Some(ui) = weak.upgrade() {
            let mut rows: Vec<_> = ui.get_sorts().iter().collect();
            if rows.len() < 3 {
                let choice = [0, 2, 4]
                    .into_iter()
                    .find(|c| !rows.iter().any(|r| r.choice / 2 == c / 2))
                    .unwrap_or(0);
                rows.push(SortData { choice });
                ui.set_sorts(ModelRc::new(VecModel::from(rows)));
            }
        }
    });
    let weak = ui.as_weak();
    let tx = sender.clone();
    let s = shared.clone();
    let st = state.clone();
    ui.on_apply_sort(move || {
        if let Some(ui) = weak.upgrade() {
            let terms: Vec<_> = ui
                .get_sorts()
                .iter()
                .filter_map(|r| SORTS.get(r.choice as usize).copied())
                .collect();
            match SortSpec::new(terms.clone()) {
                Ok(sort) => {
                    *st.sorts.borrow_mut() = terms;
                    ui.set_sort_open(false);
                    let g = s.advance();
                    ui.set_loading(true);
                    enqueue(&ui, &tx, &s, Command::Sort(g, sort));
                }
                Err(e) => ui.set_error_message(e.into()),
            }
        }
    });
    let weak = ui.as_weak();
    let s = shared.clone();
    ui.window().on_close_requested(move || {
        if let Some(deadline) = s.begin_close() {
            if let Some(ui) = weak.upgrade() {
                ui.set_closing(true);
                ui.set_loading(true);
                ui.set_status("終了中…".into());
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            slint::Timer::single_shot(remaining, || {
                let _ = slint::quit_event_loop();
            });
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    #[cfg(feature = "test-fixtures")]
    let test_args = args.clone();
    let weak = ui.as_weak();
    let s = shared.clone();
    let ex = exit.clone();
    let tx = sender.clone();
    let (finished, completion) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("archive-gui".into())
        .spawn(move || {
            match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(ui_worker::run(args, weak, receiver, tx, s, ex)),
                Err(e) => {
                    eprintln!("GUI runtime: {e}");
                    ex.store(1, Ordering::Release);
                    let _ = slint::invoke_from_event_loop(|| {
                        let _ = slint::quit_event_loop();
                    });
                }
            }
            let _ = finished.send(());
        })?;
    #[cfg(feature = "test-fixtures")]
    ui_test::start(&ui, test_args);
    let event_result = ui.run();
    shared.begin_close();
    let deadline = shared.shutdown_deadline.lock().unwrap().unwrap();
    let _ =
        completion.recv_timeout(deadline.saturating_duration_since(tokio::time::Instant::now()));
    if thread.is_finished() {
        let _ = thread.join();
    }
    UI_STATE.with(|slot| slot.borrow_mut().take());
    event_result?;
    Ok(exit.load(Ordering::Acquire))
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    #[test]
    fn close_reuses_the_first_cancel_deadline_even_after_worker_completion() {
        let shared = Shared::default();
        let cancel = CancelContext::new();
        cancel.cancel();
        let expected = cancel.cleanup_deadline();
        *shared.operation.lock().unwrap() = Some(cancel);
        assert_eq!(shared.begin_close(), Some(expected));
        shared.operation.lock().unwrap().take();
        assert!(shared.begin_close().is_none());
        assert_eq!(*shared.shutdown_deadline.lock().unwrap(), Some(expected));
    }
}
fn set_sort_rows(ui: &AppWindow, terms: &[SortTerm]) {
    ui.set_sorts(ModelRc::new(VecModel::from(
        terms
            .iter()
            .map(|t| SortData {
                choice: SORTS.iter().position(|v| v == t).unwrap() as i32,
            })
            .collect::<Vec<_>>(),
    )));
}
thread_local! { static UI_STATE:RefCell<Option<Rc<UiState>>>=const{RefCell::new(None)}; }
