//! Shared row budget, independent of the widget toolkit.
use std::collections::VecDeque;
pub const ROW_CACHE_BUDGET: usize = 32 * 1024 * 1024;
#[derive(Clone, Debug)]
pub struct Row {
    pub name: String,
    pub uri: String,
    pub kind: String,
    pub namespace: String,
}
pub struct Page {
    pub generation: u64,
    pub column: usize,
    pub start: usize,
    pub rows: Vec<Row>,
}
impl Page {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.rows.capacity() * std::mem::size_of::<Row>()
            + self
                .rows
                .iter()
                .map(|r| {
                    r.name.capacity()
                        + r.uri.capacity()
                        + r.kind.capacity()
                        + r.namespace.capacity()
                })
                .sum::<usize>()
    }
}
#[derive(Default)]
pub struct RowCache {
    pages: VecDeque<Page>,
    bytes: usize,
}
impl RowCache {
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn clear(&mut self) {
        self.pages.clear();
        self.bytes = 0;
    }
    pub fn row(&mut self, generation: u64, column: usize, row: usize) -> Option<Row> {
        let index = self.pages.iter().position(|p| {
            p.generation == generation
                && p.column == column
                && row >= p.start
                && row < p.start + p.rows.len()
        })?;
        let page = self.pages.remove(index)?;
        let result = page.rows[row - page.start].clone();
        self.pages.push_back(page);
        Some(result)
    }
    pub fn request_start(&self, generation: u64, column: usize, row: usize) -> usize {
        let aligned = row / 256 * 256;
        if self
            .pages
            .iter()
            .any(|p| p.generation == generation && p.column == column && p.start == aligned)
        {
            row
        } else {
            aligned
        }
    }
    pub fn insert(&mut self, page: Page) {
        if let Some(index) = self.pages.iter().position(|p| {
            p.generation == page.generation && p.column == page.column && p.start == page.start
        }) {
            self.bytes -= self.pages.remove(index).unwrap().bytes();
        }
        let bytes = page.bytes();
        if bytes > ROW_CACHE_BUDGET {
            return;
        }
        while self.bytes + bytes > ROW_CACHE_BUDGET {
            if let Some(old) = self.pages.pop_front() {
                self.bytes -= old.bytes();
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.pages.push_back(page);
    }
}
