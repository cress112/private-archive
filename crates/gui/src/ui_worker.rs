use crate::*;
use archive_config::Paths;
use archive_domain::*;
use archive_sync::{CancelContext, PortFuture, ReconcileObserver, ReconcileSummary};
use private_archive_gui::{
    PreparedGui, StartupError,
    browser::{Browser, details, input},
    cache::{Page, Row},
    reconcile::{self, Target},
    settings,
};
use std::collections::VecDeque;
use std::num::NonZeroU32;
use tokio::task::JoinHandle;

#[derive(Default)]
struct LiveState {
    status: String,
    log: VecDeque<String>,
    bytes: usize,
    observed: u64,
    ignored: u64,
    completed_observed: u64,
    completed_ignored: u64,
}
#[derive(Clone)]
struct Observer {
    weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<LiveState>>,
    queued: Arc<AtomicBool>,
    level: LogLevel,
    shared: Arc<Shared>,
    operation_id: u64,
}
impl Observer {
    fn publish(&self) {
        if self.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = self.clone();
        let weak = self.weak.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if this.operation_id != this.shared.operation_id.load(Ordering::Acquire)
                || this.shared.closing.load(Ordering::Acquire)
            {
                this.queued.store(false, Ordering::Release);
                return;
            }
            let state = this.state.lock().unwrap();
            if !state.status.is_empty() && ui.get_reconciling() {
                ui.set_status(state.status.clone().into());
            }
            ui.set_diagnostics(
                state
                    .log
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into(),
            );
            this.queued.store(false, Ordering::Release);
        });
    }
    fn log(&self, level: LogLevel, message: String) {
        if !self.level.allows(level) {
            return;
        }
        let mut message = message;
        truncate(&mut message, 8192);
        let mut state = self.state.lock().unwrap();
        let line = format!("{}: {message}", level.as_str());
        state.bytes += line.len();
        state.log.push_back(line);
        while state.bytes > 60 * 1024 {
            if let Some(old) = state.log.pop_front() {
                state.bytes = state.bytes.saturating_sub(old.len());
            } else {
                break;
            }
        }
        drop(state);
        self.publish();
    }
}
impl ReconcileObserver for Observer {
    fn ignored<'a>(&'a self, w: Warning, _direct: bool) -> PortFuture<'a, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            state.ignored += 1;
            let first = state.ignored == 1;
            state.status = format!(
                "再照合中 · 取得 {}件 · 除外 {}件",
                state.observed, state.ignored
            );
            drop(state);
            if first {
                self.log(LogLevel::Info, "ignore対象がありました".into());
            }
            self.log(LogLevel::Debug, format!("ignore {}", w.uri));
            self.publish();
            Ok(())
        })
    }
    fn progress<'a>(&'a self, current: Location, summary: ReconcileSummary) -> PortFuture<'a, ()> {
        Box::pin(async move {
            let mut uri = current.display();
            truncate(&mut uri, 2048);
            let mut state = self.state.lock().unwrap();
            state.observed = state.completed_observed + summary.observed;
            state.ignored = state.completed_ignored + summary.ignored;
            state.status = format!(
                "再照合中 · 取得 {}件 · 除外 {}件 · {uri}",
                state.observed, state.ignored
            );
            drop(state);
            self.publish();
            Ok(())
        })
    }
    fn finished<'a>(&'a self, summary: ReconcileSummary, succeeded: bool) -> PortFuture<'a, ()> {
        Box::pin(async move {
            {
                let mut state = self.state.lock().unwrap();
                state.completed_observed += summary.observed;
                state.completed_ignored += summary.ignored;
            }
            if summary.ignored > 0 {
                self.log(
                    LogLevel::Info,
                    format!("ignore: {} observed exclusions", summary.ignored),
                );
            }
            if !succeeded {
                self.log(
                    LogLevel::Warn,
                    format!(
                        "再照合未完了: 取得済み {}件を保持、未取得範囲の削除照合なし",
                        summary.observed
                    ),
                );
            }
            Ok(())
        })
    }
}
fn truncate(text: &mut String, max: usize) {
    if text.len() > max {
        let mut n = max;
        while !text.is_char_boundary(n) {
            n -= 1;
        }
        text.truncate(n);
    }
}
fn error(weak: &slint::Weak<AppWindow>, message: String) {
    let mut message = message;
    truncate(&mut message, 16384);
    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_error_message(message.into());
        ui.set_loading(false);
    });
}
fn snapshot(
    weak: &slint::Weak<AppWindow>,
    browser: &Browser,
    generation: u64,
    sender: mpsc::Sender<Command>,
    shared: Arc<Shared>,
) {
    let columns: Vec<_> = browser
        .columns
        .iter()
        .map(|c| {
            (
                c.parent
                    .as_ref()
                    .map(|e| e.name().to_string())
                    .unwrap_or_else(|| "バケット".into()),
                c.listing.summary().entries,
                c.selected_row,
                c.parent
                    .as_ref()
                    .map(|e| format!("{}:{}", e.id.namespace, e.id.uri()))
                    .unwrap_or_default(),
            )
        })
        .collect();
    let detail = details(
        browser
            .columns
            .iter()
            .rev()
            .find_map(|c| c.selected.as_ref()),
    );
    let label = browser.prepared.index_label();
    let selected_file = browser
        .columns
        .last()
        .and_then(|c| c.selected.as_ref())
        .is_some_and(|e| e.kind == IndexEntryKind::Object);
    let _ = weak.upgrade_in_event_loop(move |ui| {
        if generation != shared.generation.load(Ordering::Acquire)
            || shared.closing.load(Ordering::Acquire)
        {
            return;
        }
        UI_STATE.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                state.cache.borrow_mut().clear();
                let mut models = state.models.borrow_mut();
                models.clear();
                let previous: Vec<_> = ui.get_columns().iter().collect();
                let data: Vec<_> = columns
                    .into_iter()
                    .enumerate()
                    .map(|(column, (title, count, selected, scope))| {
                        let model = Rc::new(ui_model::PagedModel::new(
                            generation,
                            column,
                            count.min(i32::MAX as u64) as usize,
                            state.cache.clone(),
                            sender.clone(),
                        ));
                        models.push(model.clone());
                        ColumnData {
                            title: title.into(),
                            scroll_y: previous
                                .get(column)
                                .filter(|c| c.scope.as_str() == scope)
                                .map_or(0., |c| c.scroll_y),
                            width: previous.get(column).map_or(245., |c| c.width),
                            scope: scope.into(),
                            rows: ModelRc::from(model),
                            selected: selected.and_then(|n| i32::try_from(n).ok()).unwrap_or(-1),
                            count: format!("{count}件").into(),
                        }
                    })
                    .collect();
                ui.set_columns(ModelRc::new(VecModel::from(data)));
            }
        });
        ui.set_index_label(label.into());
        ui.set_details(ModelRc::new(VecModel::from(
            detail
                .into_iter()
                .map(|(label, value)| DetailRow {
                    label: label.into(),
                    value: value.into(),
                })
                .collect::<Vec<_>>(),
        )));
        ui.set_selected_file(selected_file);
        ui.set_loading(false);
        ui.set_error_message("".into());
        if !ui.get_reconciling() {
            ui.set_status("ローカルの記録".into());
        }
    });
}
fn show_settings(
    weak: &slint::Weak<AppWindow>,
    args: &GuiArgs,
    paths: &Paths,
) -> Result<(), StartupError> {
    let (exists, rows) = settings::read(args, paths)?;
    let path = archive_config::config_path(paths, args.config.as_deref())?;
    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_config_label(path.display().to_string().into());
        ui.set_settings_exists(exists);
        ui.set_settings(ModelRc::new(VecModel::from(
            rows.into_iter()
                .map(|r| SettingData {
                    key: r.key.as_str().into(),
                    value: r.saved.clone().unwrap_or_default().into(),
                    effective: r.effective.into(),
                    origin: r.origin.as_str().into(),
                    explicit: r.saved.is_some(),
                    dirty: false,
                })
                .collect::<Vec<_>>(),
        )));
        ui.set_settings_open(true);
    });
    Ok(())
}
fn prepared_clone(prepared: &PreparedGui) -> PreparedGui {
    PreparedGui {
        paths: Paths {
            home: prepared.paths.home.clone(),
            cwd: prepared.paths.cwd.clone(),
        },
        settings: prepared.settings.clone(),
        namespace: prepared.namespace.clone(),
    }
}
async fn operation(
    args: GuiArgs,
    prepared: PreparedGui,
    target: Target,
    depth: Option<NonZeroU32>,
    observer: Arc<Observer>,
    cancel: CancelContext,
) -> Result<ReconcileSummary, OperationError> {
    let future = reconcile::execute(&args, &prepared, target, depth, observer.as_ref(), &cancel);
    tokio::pin!(future);
    tokio::select! {biased;
        result=&mut future=>result,
        _=cancel.cancelled()=>tokio::time::timeout_at(cancel.cleanup_deadline(),&mut future).await.unwrap_or_else(|_|Err(OperationError::new(ErrorKind::Cancelled,"gui-reconcile","取消の収束期限に達しました"))),
    }
}

pub async fn run(
    args: GuiArgs,
    weak: slint::Weak<AppWindow>,
    mut receiver: mpsc::Receiver<Command>,
    sender: mpsc::Sender<Command>,
    shared: Arc<Shared>,
    exit: Arc<AtomicU8>,
) {
    let paths = match Paths::from_env() {
        Ok(paths) => paths,
        Err(e) => {
            error(&weak, e.to_string());
            exit.store(2, Ordering::Release);
            return;
        }
    };
    let initial = args.prepare(Paths {
        home: paths.home.clone(),
        cwd: paths.cwd.clone(),
    });
    let control = BrowseControl::default();
    *shared.browse.lock().unwrap() = Some(control.clone());
    let result = match initial {
        Ok(prepared) => Browser::new(prepared, control)
            .await
            .map_err(StartupError::from),
        Err(e) => Err(e),
    };
    *shared.browse.lock().unwrap() = None;
    let mut browser = match result {
        Ok(browser) => {
            snapshot(&weak, &browser, 0, sender.clone(), shared.clone());
            Some(browser)
        }
        Err(e) => {
            if !shared.closing.load(Ordering::Acquire) {
                exit.store(e.exit_code, Ordering::Release);
                error(&weak, e.message);
            }
            None
        }
    };
    let mut task: Option<JoinHandle<Result<ReconcileSummary, OperationError>>> = None;
    loop {
        if shared.closing.load(Ordering::Acquire) {
            break;
        }
        tokio::select! {biased;
                    _=shared.wake.notified()=>{if shared.closing.load(Ordering::Acquire){break;}}
                    result=async {match task.as_mut(){Some(task)=>Some(task.await),None=>std::future::pending().await}}=>{
                        task.take();let cancel=shared.operation.lock().unwrap().take();
                        let result=result.unwrap().unwrap_or_else(|e|Err(OperationError::new(ErrorKind::Io,"gui-worker",e.to_string())));
                        let text=match &result{Ok(s)=>format!("再照合完了 · 取得 {}件 · 削除反映 {}件 · 除外 {}件",s.observed,s.removed,s.ignored),Err(e) if e.kind==ErrorKind::Cancelled || cancel.as_ref().is_some_and(|c|c.is_cancelled() && c.cancelled_at()==c.stopped_at())=>"再照合を取り消しました · 取得済みの反映は保持".into(),Err(e)=>format!("再照合失敗: {e} {}",e.context.causes.join("; "))};
                        let is_error=result.is_err();let message=text.clone();
                        let _=weak.upgrade_in_event_loop(move |ui|{ui.set_reconciling(false);ui.set_status(message.clone().into());if is_error{ui.set_error_message(message.into());}});
                        if let Some(browser)=browser.as_mut(){
                            let g=shared.generation.load(Ordering::Acquire);let control=BrowseControl::default();*shared.browse.lock().unwrap()=Some(control.clone());
                            if browser.refresh(None,None,control).await.is_ok(){snapshot(&weak,browser,g,sender.clone(),shared.clone());}
                            *shared.browse.lock().unwrap()=None;
                        }
                        let _=weak.upgrade_in_event_loop(move |ui|{ui.set_status(text.clone().into());if is_error{ui.set_error_message(text.into());}});
                    }
                    command=receiver.recv()=>{
                        let Some(command)=command else{break;};
        CommandContext{browser:&mut browser,task:&mut task,args:&args,paths:&paths,weak:&weak,sender:&sender,shared:&shared,exit:&exit}.handle(command).await;
                    }
                }
    }
    if let Some(mut task) = task {
        shared.cancel();
        let deadline = shared
            .operation
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.cleanup_deadline())
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(50));
        if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
            task.abort();
        }
    }
    drop(browser);
    let _ = slint::invoke_from_event_loop(|| {
        let _ = slint::quit_event_loop();
    });
}

struct CommandContext<'a> {
    browser: &'a mut Option<Browser>,
    task: &'a mut Option<JoinHandle<Result<ReconcileSummary, OperationError>>>,
    args: &'a GuiArgs,
    paths: &'a Paths,
    weak: &'a slint::Weak<AppWindow>,
    sender: &'a mpsc::Sender<Command>,
    shared: &'a Arc<Shared>,
    exit: &'a Arc<AtomicU8>,
}
impl CommandContext<'_> {
    async fn handle(self, command: Command) {
        let Self {
            browser,
            task,
            args,
            paths,
            weak,
            sender,
            shared,
            exit,
        } = self;
        match command {
            Command::Page(g, column, start) => {
                if g != shared.generation.load(Ordering::Acquire) {
                    return;
                }
                if let Some(listing) = browser
                    .as_ref()
                    .and_then(|b| b.columns.get(column))
                    .map(|c| c.listing.clone())
                {
                    match listing.page(start as u64, 256).await {
                        Ok(page) => {
                            let page = Page {
                                generation: g,
                                column,
                                start,
                                rows: page
                                    .entries
                                    .into_iter()
                                    .map(|e| Row {
                                        name: e.name().to_string(),
                                        uri: e.uri(),
                                        kind: e.kind.as_str().into(),
                                        namespace: e.id.namespace,
                                    })
                                    .collect(),
                            };
                            let s = shared.clone();
                            let _ = weak.upgrade_in_event_loop(move |_| {
                                if g == s.generation.load(Ordering::Acquire) {
                                    UI_STATE.with(|slot| {
                                        if let Some(state) = slot.borrow().as_ref()
                                            && let Some(model) = state.models.borrow().get(column)
                                            && model.generation == g
                                        {
                                            model.received(page);
                                        }
                                    });
                                }
                            });
                        }
                        Err(e) => error(weak, e.to_string()),
                    }
                }
            }
            Command::Reconcile(depth) => {
                if task.is_some() {
                    return;
                }
                let result = (|| -> Result<_, OperationError> {
                    let browser = browser
                        .as_ref()
                        .ok_or_else(|| input("インデックスを読み込めません"))?;
                    let target = match browser
                        .columns
                        .iter()
                        .rev()
                        .find_map(|c| c.selected.clone())
                    {
                        Some(entry) => Target::Selected(Box::new(entry)),
                        None => Target::Top,
                    };
                    let file =
                        matches!(&target,Target::Selected(e) if e.kind==IndexEntryKind::Object);
                    let depth = if file || depth.trim().is_empty() {
                        None
                    } else {
                        Some(
                            depth
                                .parse::<NonZeroU32>()
                                .map_err(|_| input("深さには1以上の整数を指定してください"))?,
                        )
                    };
                    Ok((prepared_clone(&browser.prepared), target, depth))
                })();
                match result {
                    Ok((prepared, target, depth)) => {
                        let cancel = shared.operation.lock().unwrap().clone().unwrap_or_default();
                        *shared.operation.lock().unwrap() = Some(cancel.clone());
                        let operation_id = shared.operation_id.fetch_add(1, Ordering::AcqRel) + 1;
                        let observer = Arc::new(Observer {
                            weak: weak.clone(),
                            state: Arc::new(Mutex::new(LiveState::default())),
                            queued: Arc::new(AtomicBool::new(false)),
                            level: prepared.settings.log_level.value,
                            shared: shared.clone(),
                            operation_id,
                        });
                        *task = Some(tokio::spawn(operation(
                            args.clone(),
                            prepared,
                            target,
                            depth,
                            observer,
                            cancel,
                        )));
                    }
                    Err(e) => {
                        shared.operation.lock().unwrap().take();
                        error(weak, e.to_string());
                        let _ = weak.upgrade_in_event_loop(|ui| ui.set_reconciling(false));
                    }
                }
            }
            Command::Settings => {
                if let Err(e) = show_settings(weak, args, paths) {
                    error(weak, e.message);
                }
            }
            Command::CreateSettings => match archive_config::init(paths, args.config.as_deref()) {
                Ok(_) => {
                    if let Err(e) = show_settings(weak, args, paths) {
                        error(weak, e.message);
                    }
                }
                Err(e) => error(weak, e.to_string()),
            },
            Command::SaveSettings(rows) => {
                if let Err(e) = settings::save(args, paths, &rows) {
                    error(weak, e.message);
                    return;
                }
                let _ = weak.upgrade_in_event_loop(|ui| ui.set_settings_open(false));
                let g = shared.advance();
                let control = BrowseControl::default();
                *shared.browse.lock().unwrap() = Some(control.clone());
                let result: Result<(), StartupError> = async {
                    if let Some(browser) = browser.as_mut() {
                        browser.reload(args, control).await?;
                    } else {
                        *browser = Some(
                            Browser::new(
                                args.prepare(Paths {
                                    home: paths.home.clone(),
                                    cwd: paths.cwd.clone(),
                                })?,
                                control,
                            )
                            .await?,
                        );
                    }
                    Ok(())
                }
                .await;
                *shared.browse.lock().unwrap() = None;
                match result {
                    Ok(()) => {
                        exit.store(0, Ordering::Release);
                        snapshot(
                            weak,
                            browser.as_ref().unwrap(),
                            g,
                            sender.clone(),
                            shared.clone(),
                        );
                    }
                    Err(e) => {
                        if let Some(browser) = browser.as_ref()
                            && browser.columns.is_empty()
                        {
                            snapshot(weak, browser, g, sender.clone(), shared.clone());
                        }
                        error(weak, e.message)
                    }
                }
            }
            command => {
                let g = match &command {
                    Command::Select(g, ..)
                    | Command::Top(g)
                    | Command::Reload(g)
                    | Command::Sort(g, _) => *g,
                    _ => unreachable!(),
                };
                if g != shared.generation.load(Ordering::Acquire) {
                    return;
                }
                let control = BrowseControl::default();
                *shared.browse.lock().unwrap() = Some(control.clone());
                let result: Result<(), StartupError> = async {
                    if browser.is_none() {
                        *browser = Some(
                            Browser::new(
                                args.prepare(Paths {
                                    home: paths.home.clone(),
                                    cwd: paths.cwd.clone(),
                                })?,
                                control.clone(),
                            )
                            .await?,
                        );
                    }
                    let browser = browser.as_mut().unwrap();
                    match command {
                        Command::Select(_, column, id, kind) => {
                            let listing = browser
                                .columns
                                .get(column)
                                .ok_or_else(|| input("column no longer exists"))?
                                .listing
                                .clone();
                            let row = listing.position(&id, kind).await?.ok_or_else(|| {
                                input("選択対象がありません。ローカル再読込を実行してください。")
                            })?;
                            browser.select(column, row, control).await?;
                        }
                        Command::Top(_) => browser.top(),
                        Command::Reload(_) => browser.reload(args, control).await?,
                        Command::Sort(_, sort) => {
                            browser.refresh(None, Some(sort), control).await?
                        }
                        _ => unreachable!(),
                    }
                    Ok(())
                }
                .await;
                *shared.browse.lock().unwrap() = None;
                if g == shared.generation.load(Ordering::Acquire) {
                    match result {
                        Ok(()) => {
                            exit.store(0, Ordering::Release);
                            snapshot(
                                weak,
                                browser.as_ref().unwrap(),
                                g,
                                sender.clone(),
                                shared.clone(),
                            );
                        }
                        Err(e) => error(weak, e.message),
                    }
                }
            }
        }
    }
}
