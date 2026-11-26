use crate::*;
use private_archive_gui::cache::{Page, RowCache};
use slint::{ModelNotify, ModelTracker};
use std::collections::BTreeSet;

pub struct PagedModel {
    pub generation: u64,
    pub column: usize,
    pub count: usize,
    cache: Rc<RefCell<RowCache>>,
    pending: RefCell<BTreeSet<usize>>,
    sender: mpsc::Sender<Command>,
    notify: ModelNotify,
}
impl PagedModel {
    pub fn new(
        generation: u64,
        column: usize,
        count: usize,
        cache: Rc<RefCell<RowCache>>,
        sender: mpsc::Sender<Command>,
    ) -> Self {
        Self {
            generation,
            column,
            count,
            cache,
            pending: RefCell::new(BTreeSet::new()),
            sender,
            notify: ModelNotify::default(),
        }
    }
    pub fn received(&self, page: Page) {
        self.pending.borrow_mut().remove(&page.start);
        let start = page.start;
        let len = page.rows.len();
        self.cache.borrow_mut().insert(page);
        for row in start..start + len {
            self.notify.row_changed(row);
        }
    }
}
impl Model for PagedModel {
    type Data = FileRow;
    fn row_count(&self) -> usize {
        self.count
    }
    fn row_data(&self, row: usize) -> Option<FileRow> {
        if row >= self.count {
            return None;
        }
        if let Some(r) = self
            .cache
            .borrow_mut()
            .row(self.generation, self.column, row)
        {
            return Some(FileRow {
                name: r.name.into(),
                uri: r.uri.into(),
                kind: r.kind.into(),
                namespace: r.namespace.into(),
                loaded: true,
            });
        }
        let start = self
            .cache
            .borrow()
            .request_start(self.generation, self.column, row);
        if !self.pending.borrow().contains(&start)
            && self
                .sender
                .try_send(Command::Page(self.generation, self.column, start))
                .is_ok()
        {
            self.pending.borrow_mut().insert(start);
        }
        Some(FileRow {
            name: "読み込み中…".into(),
            uri: "".into(),
            kind: "".into(),
            namespace: "".into(),
            loaded: false,
        })
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}
