//! A bounded session-owned set. Callbacks apply directly; there is no event queue.
use gmux_protocol::program_status::{Kind, MAX_RECORDS, Record, State, Summary};

pub(crate) struct Report<'a> {
    pub id: &'a str,
    pub state: State,
    pub kind: Option<Kind>,
    pub progress: Option<u8>,
    pub app: Option<&'a str>,
    pub title: &'a str,
    pub message: &'a str,
}
impl Report<'_> {
    fn matches(&self, record: &Record) -> bool {
        self.id == record.id
            && self.state == record.state
            && self.kind == record.kind
            && self.progress == record.progress
            && self.app == record.app.as_deref()
            && self.title == record.title
            && self.message == record.message
    }
    fn owned(self) -> Record {
        Record {
            id: self.id.into(),
            state: self.state,
            kind: self.kind,
            progress: self.progress,
            app: self.app.map(str::to_owned),
            title: self.title.into(),
            message: self.message.into(),
        }
    }
}

struct Entry {
    record: Record,
    updated: u64,
}
#[derive(Default)]
pub(crate) struct Records {
    entries: Vec<Entry>,
    clock: u64,
    dirty: bool,
    summary: Option<Summary>,
}
impl Records {
    pub(crate) fn report(&mut self, report: Report<'_>) {
        self.clock += 1;
        if let Some(entry) = self.entries.iter_mut().find(|e| e.record.id == report.id) {
            entry.updated = self.clock;
            if report.matches(&entry.record) {
                return;
            }
            entry.record = report.owned();
        } else {
            if self.entries.len() == MAX_RECORDS {
                let index = self
                    .entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.updated)
                    .unwrap()
                    .0;
                self.entries.swap_remove(index);
            }
            self.entries.push(Entry {
                record: report.owned(),
                updated: self.clock,
            });
        }
        self.dirty = true;
    }
    fn retain(&mut self, keep: impl Fn(&Record) -> bool) {
        let before = self.entries.len();
        self.entries.retain(|e| keep(&e.record));
        self.dirty |= before != self.entries.len();
    }
    pub(crate) fn clear(&mut self, id: &str) {
        self.retain(|r| {
            !id.is_empty()
                && r.id != id
                && !r.id.strip_prefix(id).is_some_and(|s| s.starts_with('/'))
        });
    }
    pub(crate) fn end_execution(&mut self) {
        self.retain(|r| !r.state.transient());
    }
    pub(crate) fn acknowledge(&mut self) {
        self.retain(|r| !r.state.result());
    }
    pub(crate) fn has_result(&self) -> bool {
        self.entries.iter().any(|e| e.record.state.result())
    }
    fn app(&self, mut id: &str) -> Option<&str> {
        loop {
            if let Some(app) = self
                .entries
                .iter()
                .find(|e| e.record.id == id)
                .and_then(|e| e.record.app.as_deref())
            {
                return Some(app);
            }
            if id.is_empty() {
                return None;
            }
            id = id.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }
    pub(crate) fn take_changed(&mut self) -> bool {
        if !std::mem::take(&mut self.dirty) {
            return false;
        }
        let record = self
            .entries
            .iter()
            .min_by(|a, b| {
                (a.record.state.priority(), &a.record.id)
                    .cmp(&(b.record.state.priority(), &b.record.id))
            })
            .map(|e| &e.record);
        let Some(record) = record else {
            return self.summary.take().is_some();
        };
        let report = Report {
            id: &record.id,
            state: record.state,
            kind: record.kind,
            progress: record.progress,
            app: self.app(&record.id),
            title: &record.title,
            message: &record.message,
        };
        if self
            .summary
            .as_ref()
            .is_some_and(|s| s.count == self.entries.len() && report.matches(&s.record))
        {
            return false;
        }
        self.summary = Some(Summary {
            record: report.owned(),
            count: self.entries.len(),
        });
        true
    }
    pub(crate) fn summary(&self) -> Option<&Summary> {
        self.summary.as_ref()
    }
    pub(crate) fn records(&self) -> Vec<&Record> {
        let mut records: Vec<_> = self.entries.iter().map(|e| &e.record).collect();
        records.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        records
    }
    pub(crate) fn memory_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<Entry>()
            + self
                .entries
                .iter()
                .map(|e| e.record.heap_bytes())
                .sum::<usize>()
            + self.summary.as_ref().map_or(0, |s| s.record.heap_bytes())
    }
}
